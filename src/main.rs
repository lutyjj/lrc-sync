mod config;
mod db;
mod files;
mod lrclib;
mod queue;
#[cfg(test)]
mod regressions;
mod scan;
mod shutdown;
mod tags;
#[cfg(test)]
mod test_support;
mod watch;

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use crate::{
    config::Config,
    db::CacheDb,
    lrclib::LrclibClient,
    queue::{WorkItem, WorkQueue},
    scan::{ProcessingOutcome, Processor, queue_library},
    shutdown::Shutdown,
};

fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .compact()
        .init();
    let config = Config::from_env()?;
    let shutdown = Shutdown::default();
    let signals = shutdown.clone();
    ctrlc::set_handler(move || {
        info!("received shutdown signal");
        signals.cancel();
    })
    .context("setting signal handler")?;
    let lrclib = LrclibClient::new(
        config.request_interval,
        config.request_timeout,
        shutdown.clone(),
    )?;
    run(config, shutdown, lrclib)
}

struct StopOnDrop<'a> {
    queue: &'a WorkQueue,
    shutdown: &'a Shutdown,
}
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.queue.close();
    }
}

fn run(mut config: Config, shutdown: Shutdown, lrclib: LrclibClient) -> Result<()> {
    config.music_dir = config
        .music_dir
        .canonicalize()
        .with_context(|| format!("opening music directory {}", config.music_dir.display()))?;
    if !config.music_dir.is_dir() {
        bail!("music path is not a directory");
    }
    info!(?config, "starting lrc-sync");
    let db = CacheDb::open(config.db_file.clone(), config.retry_not_found_days)?;
    let processor = Processor::new(config.clone(), db, lrclib, shutdown.clone());
    let queue = Arc::new(WorkQueue::new(config.max_pending_dirs, shutdown.clone()));
    thread::scope(|scope| -> Result<()> {
        let _stop = StopOnDrop {
            queue: &queue,
            shutdown: &shutdown,
        };
        let workers: Vec<_> = (0..config.concurrency)
            .map(|_| {
                let queue = queue.clone();
                let processor = processor.clone();
                let shutdown = shutdown.clone();
                scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        while let Some(item) = queue.take() {
                            process_item(&processor, &queue, item);
                        }
                    }));
                    if let Err(panic) = result {
                        shutdown.cancel();
                        queue.close();
                        std::panic::resume_unwind(panic);
                    }
                })
            })
            .collect();
        let watcher = match watch::start(config.clone(), queue.clone(), shutdown.clone()) {
            Ok(handle) => Some(handle),
            Err(err) => {
                warn!(error = %format!("{err:#}"), "watcher unavailable; scheduled repair scans remain active");
                None
            }
        };
        let result = (|| -> Result<()> {
            let count = queue_library(&config, &queue)?;
            info!(directories = count, "initial library scan queued");
            let mut next_scan = Instant::now() + config.fallback_interval;
            let mut watcher_warned = false;
            while !shutdown.is_cancelled() {
                if !watcher_warned && watcher.as_ref().is_some_and(|handle| handle.is_finished()) {
                    warn!("watcher stopped; scheduled repair scans remain active");
                    watcher_warned = true;
                }
                if queue.take_repair_request() || Instant::now() >= next_scan {
                    let count = queue_library(&config, &queue)?;
                    info!(directories = count, "repair library scan queued");
                    next_scan = Instant::now() + config.fallback_interval;
                }
                shutdown.wait(Duration::from_millis(100));
            }
            Ok(())
        })();
        shutdown.cancel();
        queue.close();
        if let Some(handle) = watcher {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("watcher dispatcher panicked"))?;
        }
        for handle in workers {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("worker panicked"))?;
        }
        result
    })?;
    info!("shutdown complete; workers and watcher joined");
    Ok(())
}

fn process_item(processor: &Processor, queue: &WorkQueue, item: WorkItem) {
    let changed = match processor.process_work(&item) {
        Ok(ProcessingOutcome::Retry) => true,
        Ok(ProcessingOutcome::Complete) => false,
        Ok(ProcessingOutcome::StaleRoute) => {
            queue.request_repair();
            false
        }
        Err(err) => {
            error!(path = %item.directory.display(), error = %format!("{err:#}"), "directory processing failed");
            false
        }
    };
    queue.finish(item, changed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestServer, config, tags, write_flac};
    use std::fs;
    use tempfile::TempDir;

    fn wait_for(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "daemon did not reach expected state"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(config: Config, server: &TestServer) -> (Shutdown, thread::JoinHandle<Result<()>>) {
        let shutdown = Shutdown::default();
        let stopping = shutdown.clone();
        let client = LrclibClient::test_client(server.url.clone(), shutdown.clone());
        let handle = thread::spawn(move || run(config, stopping, client));
        (shutdown, handle)
    }

    #[test]
    fn full_daemon_restores_deleted_lyrics_and_imported_directories() {
        let root = TempDir::new().unwrap();
        let seed = root.path().join("seed.flac");
        write_flac(&seed, &tags());
        fs::write(seed.with_extension("lrc"), "curated fixture lyrics").unwrap();
        let config = config(root.path());
        let database = config.db_file.clone();
        let server = TestServer::new(|_| panic!("all matching lyrics exist in cache"));
        let (shutdown, handle) = start(config, &server);
        wait_for(|| {
            CacheDb::open(database.clone(), 7)
                .ok()
                .and_then(|db| db.cached(&seed, &tags()).ok())
                .flatten()
                .is_some()
        });
        fs::remove_file(seed.with_extension("lrc")).unwrap();
        wait_for(|| seed.with_extension("lrc").exists());
        let staging = TempDir::new().unwrap();
        write_flac(&staging.path().join("song.flac"), &tags());
        let imported = root.path().join("imported");
        fs::rename(staging.path(), &imported).unwrap();
        wait_for(|| imported.join("song.lrc").exists());
        shutdown.cancel();
        handle.join().unwrap().unwrap();
        assert_eq!(
            files::read_lyrics(&imported.join("song.lrc")).unwrap(),
            "curated fixture lyrics"
        );
    }

    #[test]
    fn poll_backend_processes_a_new_track_without_native_events() {
        let root = TempDir::new().unwrap();
        let seed = root.path().join("seed.flac");
        write_flac(&seed, &tags());
        fs::write(seed.with_extension("lrc"), "poll fixture lyrics").unwrap();
        let mut config = config(root.path());
        config.watch_mode = config::WatchMode::Poll;
        let database = config.db_file.clone();
        let server = TestServer::new(|_| panic!("cache has the matching recording"));
        let (shutdown, handle) = start(config, &server);
        wait_for(|| {
            CacheDb::open(database.clone(), 7)
                .ok()
                .and_then(|db| db.cached(&seed, &tags()).ok())
                .flatten()
                .is_some()
        });
        let new = root.path().join("new.flac");
        write_flac(&new, &tags());
        wait_for(|| new.with_extension("lrc").exists());
        shutdown.cancel();
        handle.join().unwrap().unwrap();
        assert_eq!(
            files::read_lyrics(&new.with_extension("lrc")).unwrap(),
            "poll fixture lyrics"
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_daemon_never_writes_through_an_excluded_symlink() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let seed = root.path().join("seed.flac");
        write_flac(&seed, &tags());
        fs::write(seed.with_extension("lrc"), "fixture lyrics").unwrap();
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let external = outside.path().join("song.flac");
        write_flac(&external, &tags());
        let config = config(root.path());
        let database = config.db_file.clone();
        let server = TestServer::new(|_| panic!("excluded paths never request lyrics"));
        let (shutdown, handle) = start(config, &server);
        wait_for(|| {
            CacheDb::open(database.clone(), 7)
                .ok()
                .and_then(|db| db.cached(&seed, &tags()).ok())
                .flatten()
                .is_some()
        });
        write_flac(&external, &tags());
        thread::sleep(Duration::from_millis(250));
        shutdown.cancel();
        handle.join().unwrap().unwrap();
        assert!(!external.with_extension("lrc").exists());
    }

    #[test]
    fn empty_library_cancellation_joins_every_service_thread() {
        let root = TempDir::new().unwrap();
        let server = TestServer::new(|_| panic!("no audio"));
        let (shutdown, handle) = start(config(root.path()), &server);
        shutdown.cancel();
        let start = Instant::now();
        handle.join().unwrap().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
