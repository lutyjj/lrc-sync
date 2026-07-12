mod config;
mod db;
mod lrclib;
mod scan;
mod tags;
mod watch;

use std::{sync::Arc, thread};

use anyhow::{Context, Result};
use rayon::ThreadPoolBuilder;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

use crate::{
    config::Config,
    db::CacheDb,
    lrclib::LrclibClient,
    scan::{sync_library, Processor},
    watch::watch_music,
};

fn main() -> Result<()> {
    init_logging();

    let config = Config::from_env()?;
    info!(?config, "starting lrc-sync");

    let db = CacheDb::open(config.db_file.clone(), config.retry_not_found_days)?;
    db.migrate_legacy_json()?;
    db.load_memory_cache()?;

    let lrclib = LrclibClient::new(config.request_interval, config.request_timeout)?;
    let processor = Processor::new(config.clone(), db, lrclib);
    let pool = Arc::new(
        ThreadPoolBuilder::new()
            .num_threads(config.concurrency)
            .thread_name(|idx| format!("lrc-sync-worker-{idx}"))
            .build()
            .context("building worker pool")?,
    );

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        info!("received shutdown signal");
        let _ = shutdown_tx.send(());
    })
    .context("setting signal handler")?;

    let watch_processor = processor.clone();
    let watch_pool = Arc::clone(&pool);
    let watch_config = config.clone();
    let watcher_handle = thread::spawn(move || {
        if let Err(err) = watch_music(watch_config, watch_processor, watch_pool) {
            error!(error = %err, "filesystem watcher stopped");
        }
    });

    if let Err(err) = sync_library(pool.as_ref(), processor.clone()) {
        error!(error = %err, "initial sync failed");
    }

    let mut watcher_warned = false;
    loop {
        if !watcher_warned && watcher_handle.is_finished() {
            warn!("filesystem watcher thread has stopped; only scheduled scans remain active");
            watcher_warned = true;
        }

        match shutdown_rx.recv_timeout(config.fallback_interval) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                info!("shutting down gracefully");
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        info!("running scheduled fallback sync scan");
        if let Err(err) = sync_library(pool.as_ref(), processor.clone()) {
            error!(error = %err, "scheduled sync failed");
        }
    }

    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).compact().init();
}
