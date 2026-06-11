mod config;
mod db;
mod lrclib;
mod scan;
mod tags;
mod watch;

use std::{sync::Arc, thread, time::Duration};

use anyhow::{Context, Result};
use rayon::ThreadPoolBuilder;
use tracing::{error, info};
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
    info!(?config, "starting lrcget-cli");

    let db = CacheDb::new(config.db_file.clone(), config.retry_not_found_days);
    db.init()?;
    db.migrate_legacy_json()?;
    db.load_memory_cache()?;

    let lrclib = LrclibClient::new(config.request_interval)?;
    let processor = Processor::new(config.clone(), db, lrclib);
    let pool = Arc::new(
        ThreadPoolBuilder::new()
            .num_threads(config.concurrency)
            .thread_name(|idx| format!("lrcget-worker-{idx}"))
            .build()
            .context("building worker pool")?,
    );

    let watch_processor = processor.clone();
    let watch_pool = Arc::clone(&pool);
    let watch_config = config.clone();
    thread::spawn(move || {
        if let Err(err) = watch_music(watch_config, watch_processor, watch_pool) {
            error!(error = %err, "filesystem watcher stopped");
        }
    });

    if let Err(err) = sync_library(pool.as_ref(), processor.clone()) {
        error!(error = %err, "initial sync failed");
    }

    loop {
        thread::sleep(config.fallback_interval.max(Duration::from_secs(60)));
        info!("running scheduled fallback sync scan");
        if let Err(err) = sync_library(pool.as_ref(), processor.clone()) {
            error!(error = %err, "scheduled sync failed");
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).compact().init();
}
