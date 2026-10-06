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
    if config.follow_symlinks && config.watch_mode == config::WatchMode::Native {
        bail!(
            "LRCSYNC_FOLLOW_SYMLINKS=true requires LRCSYNC_WATCH_MODE=poll to track linked targets reliably"
        );
    }
    info!(?config, "starting lrc-sync");
    let db = CacheDb::open(config.db_file.clone(), config.retry_not_found_days)?;
    let processor = Processor::new(config.clone(), db.clone(), lrclib, shutdown.clone());
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
        let watcher = match watch::start(
            config.clone(),
            db.clone(),
            queue.clone(),
            shutdown.clone(),
        ) {
            Ok(handle) => Some(handle),
            Err(err) if config.fallback_interval.is_some() => {
                warn!(error = %format!("{err:#}"), "watcher unavailable; scheduled repair scans remain active");
                None
            }
            Err(err) => {
                return Err(err)
                    .context("filesystem watcher required when periodic scans are disabled");
            }
        };
        let result = (|| -> Result<()> {
            if config.startup_scan {
                let count = queue_library(&config, &queue)?;
                info!(directories = count, "initial library scan queued");
            } else {
                info!("watching filesystem changes; startup scan disabled");
            }
            let mut next_scan = config
                .fallback_interval
                .map(|interval| Instant::now() + interval);
            let mut watcher_warned = false;
            while !shutdown.is_cancelled() {
                if !watcher_warned && watcher.as_ref().is_some_and(|handle| handle.is_finished()) {
                    if config.fallback_interval.is_none() {
                        bail!("filesystem watcher stopped while periodic scans are disabled");
                    }
                    warn!("watcher stopped; scheduled repair scans remain active");
                    watcher_warned = true;
                }
                if queue.take_repair_request()
                    || next_scan.is_some_and(|deadline| Instant::now() >= deadline)
                {
                    db.invalidate_all_snapshots()?;
                    let count = queue_library(&config, &queue)?;
                    info!(directories = count, "repair library scan queued");
                    next_scan = config
                        .fallback_interval
                        .map(|interval| Instant::now() + interval);
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
        #[cfg(unix)]
        {
            let sidecar = seed.with_extension("lrc");
            let modified = fs::metadata(&sidecar).unwrap().modified().unwrap();
            fs::write(&sidecar, "edit fixture lyrics").unwrap();
            fs::File::open(&sidecar)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            wait_for(|| {
                CacheDb::open(database.clone(), 7)
                    .unwrap()
                    .cached(&seed, &tags())
                    .unwrap()
                    == Some(db::CachedResult::Found {
                        lyrics: "edit fixture lyrics".into(),
                        origin: db::Origin::Curated,
                    })
            });
        }
        shutdown.cancel();
        handle.join().unwrap().unwrap();
        assert_eq!(
            files::read_lyrics(&new.with_extension("lrc")).unwrap(),
            "poll fixture lyrics"
        );
    }

    #[cfg(unix)]
    #[test]
    fn polling_follows_external_file_targets_replacements_and_aliases() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("song.flac");
        let linked = root.path().join("linked.flac");
        write_flac(&target, &tags());
        fs::write(target.with_extension("lrc"), "target curated lyrics").unwrap();
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("folder")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("missing.flac"),
            root.path().join("dangling.flac"),
        )
        .unwrap();
        let intermediate = outside.path().join("intermediate.flac");
        let chain = root.path().join("chain.flac");
        std::os::unix::fs::symlink(&target, &intermediate).unwrap();
        std::os::unix::fs::symlink(&intermediate, &chain).unwrap();
        fs::write(
            intermediate.with_extension("lrc"),
            "intermediate curated lyrics",
        )
        .unwrap();
        fs::write(chain.with_extension("lrc"), "chain curated lyrics").unwrap();
        fs::write(linked.with_extension("lrc"), "curated lyrics").unwrap();
        let mut settings = config(root.path());
        settings.follow_symlinks = true;
        settings.watch_mode = config::WatchMode::Poll;
        let database = settings.db_file.clone();
        let server = TestServer::new(|_| panic!("curated lyrics do not require HTTP"));
        let (shutdown, handle) = start(settings, &server);
        let artist = |path: &std::path::Path| {
            CacheDb::open(database.clone(), 7)
                .ok()
                .and_then(|db| db.record(path).ok())
                .flatten()
                .map(|record| record.tags.artist)
        };
        wait_for(|| artist(&linked).is_some());
        let mut corrected = tags();
        corrected.artist = "Changed Artist".into();
        write_flac(&target, &corrected);
        wait_for(|| artist(&linked) == Some(corrected.normalized().artist));
        wait_for(|| artist(&target) == Some(corrected.normalized().artist));
        let replacement = outside.path().join("replacement.flac");
        corrected.artist = "Replaced Artist".into();
        write_flac(&replacement, &corrected);
        fs::rename(replacement, &target).unwrap();
        wait_for(|| artist(&linked) == Some(corrected.normalized().artist));
        let new_target = outside.path().join("added.flac");
        let new_alias = root.path().join("added.flac");
        write_flac(&new_target, &tags());
        fs::write(
            new_target.with_extension("lrc"),
            "new target curated lyrics",
        )
        .unwrap();
        fs::write(new_alias.with_extension("lrc"), "new curated lyrics").unwrap();
        std::os::unix::fs::symlink(&new_target, &new_alias).unwrap();
        wait_for(|| artist(&new_alias).is_some());
        write_flac(&new_target, &corrected);
        wait_for(|| artist(&new_alias) == Some(corrected.normalized().artist));
        let elsewhere = TempDir::new().unwrap();
        let retargeted = elsewhere.path().join("retargeted.flac");
        corrected.artist = "Retargeted Artist".into();
        write_flac(&retargeted, &corrected);
        let temporary_link = root.path().join("replacement-link");
        std::os::unix::fs::symlink(&retargeted, &temporary_link).unwrap();
        fs::rename(temporary_link, &linked).unwrap();
        wait_for(|| artist(&linked) == Some(corrected.normalized().artist));
        let temporary_target = outside.path().join("replacement-link");
        std::os::unix::fs::symlink(&retargeted, &temporary_target).unwrap();
        fs::rename(temporary_target, &intermediate).unwrap();
        wait_for(|| artist(&chain) == Some(corrected.normalized().artist));
        let second_alias = root.path().join("second.flac");
        fs::write(second_alias.with_extension("lrc"), "second curated lyrics").unwrap();
        std::os::unix::fs::symlink(&retargeted, &second_alias).unwrap();
        wait_for(|| artist(&second_alias) == Some(corrected.normalized().artist));
        corrected.artist = "Shared Target Artist".into();
        write_flac(&retargeted, &corrected);
        wait_for(|| {
            artist(&linked) == Some(corrected.normalized().artist.clone())
                && artist(&second_alias) == Some(corrected.normalized().artist)
        });
        shutdown.cancel();
        handle.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_mode_requires_polling_for_followed_links() {
        let root = TempDir::new().unwrap();
        let mut settings = config(root.path());
        settings.follow_symlinks = true;
        let server = TestServer::new(|_| panic!("configuration is rejected before processing"));
        let (shutdown, handle) = start(settings, &server);
        let error = handle.join().unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires LRCSYNC_WATCH_MODE=poll")
        );
        shutdown.cancel();
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
