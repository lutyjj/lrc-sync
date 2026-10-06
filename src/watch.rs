use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind},
};
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::{
    config::{Config, WatchMode},
    db::CacheDb,
    files::{self, FileStamp},
    queue::WorkQueue,
    shutdown::Shutdown,
};

pub fn start(
    config: Config,
    db: CacheDb,
    queue: Arc<WorkQueue>,
    shutdown: Shutdown,
) -> Result<JoinHandle<()>> {
    if config.watch_mode == WatchMode::Poll {
        return start_poll(config, queue, shutdown);
    }
    let (tx, rx) = mpsc::sync_channel(256);
    let overflow = queue.clone();
    let root_lost = Arc::new(AtomicBool::new(false));
    let callback_root_lost = root_lost.clone();
    let root = config.music_dir.clone();
    let callback = move |event: notify::Result<Event>| {
        if event.as_ref().is_ok_and(|event| {
            event.paths.iter().any(|path| path == &root)
                && matches!(
                    event.kind,
                    EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                )
        }) {
            // Watch loss must survive a full change channel.
            callback_root_lost.store(true, Ordering::Relaxed);
            return;
        }
        // Readers generate native access events too; they must not fill the change channel.
        if event.as_ref().is_ok_and(|event| !is_change(event)) {
            return;
        }
        if let Err(mpsc::TrySendError::Full(_)) = tx.try_send(event) {
            overflow.request_repair();
        }
    };
    let options = NotifyConfig::default()
        .with_follow_symlinks(config.follow_symlinks)
        .with_poll_interval(config.poll_interval);
    let mut watcher = RecommendedWatcher::new(callback, options)?;
    watcher
        .watch(&config.music_dir, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", config.music_dir.display()))?;
    info!(mode = ?config.watch_mode, path = %config.music_dir.display(), "started filesystem watcher");
    thread::Builder::new()
        .name("lrc-sync-watcher".into())
        .spawn(move || {
            let _watcher = watcher;
            while !shutdown.is_cancelled() {
                if root_lost.load(Ordering::Relaxed) {
                    warn!("music root removed or renamed; filesystem watch lost");
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(mut event)) => {
                        if matches!(
                            event.kind,
                            EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
                                | EventKind::Any
                                | EventKind::Access(AccessKind::Close(AccessMode::Write))
                        ) {
                            let invalidated = (|| -> Result<()> {
                                let mut affected: BTreeSet<_> =
                                    event.paths.iter().cloned().collect();
                                for path in &event.paths {
                                    if files::eligible(&config, path)
                                        && (files::is_audio(path) || files::is_lrc(path))
                                        && !path.is_dir()
                                    {
                                        affected.extend(db.paths_sharing_file(path)?);
                                    }
                                }
                                let changed: Vec<_> = affected
                                    .iter()
                                    .filter(|path| {
                                        files::eligible(&config, path)
                                            && (files::is_audio(path) || files::is_lrc(path))
                                            && !path.is_dir()
                                    })
                                    .cloned()
                                    .collect();
                                db.invalidate_files(&changed)?;
                                event.paths = affected.into_iter().collect();
                                Ok(())
                            })();
                            if let Err(err) = invalidated {
                                warn!(error = %err, "content-cache invalidation failed");
                                break;
                            }
                        }
                        dispatch(&config, &queue, event);
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "filesystem event failed; requesting repair scan");
                        queue.request_repair();
                        if config.fallback_interval.is_none() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .context("starting watcher dispatcher")
}

fn metadata_snapshot(config: &Config) -> Result<BTreeMap<PathBuf, FileStamp>> {
    let mut snapshot = BTreeMap::new();
    for entry in WalkDir::new(&config.music_dir)
        .follow_links(config.follow_symlinks)
        .into_iter()
        .filter_entry(|entry| files::eligible(config, entry.path()))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err)
                if err
                    .io_error()
                    .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let path = entry.path();
        if (files::is_audio(path) || files::is_lrc(path)) && path.is_file() {
            match FileStamp::read(path) {
                Ok(stamp) => {
                    snapshot.insert(path.to_owned(), stamp);
                }
                Err(err) if !path.exists() => {
                    let _ = err;
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(snapshot)
}

fn start_poll(config: Config, queue: Arc<WorkQueue>, shutdown: Shutdown) -> Result<JoinHandle<()>> {
    let mut previous = metadata_snapshot(&config)?;
    info!(mode = ?config.watch_mode, path = %config.music_dir.display(), "started filesystem watcher");
    thread::Builder::new()
        .name("lrc-sync-poll-watcher".into())
        .spawn(move || {
            while shutdown.wait(config.poll_interval) {
                let current = match metadata_snapshot(&config) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        warn!(error = %err, "metadata polling failed");
                        break;
                    }
                };
                for (path, stamp) in &current {
                    if previous.get(path) != Some(stamp) {
                        dispatch(
                            &config,
                            &queue,
                            Event::new(EventKind::Create(CreateKind::File)).add_path(path.clone()),
                        );
                    }
                }
                for path in previous.keys().filter(|path| !current.contains_key(*path)) {
                    dispatch(
                        &config,
                        &queue,
                        Event::new(EventKind::Remove(RemoveKind::File)).add_path(path.clone()),
                    );
                }
                previous = current;
            }
        })
        .context("starting metadata polling")
}

fn is_change(event: &Event) -> bool {
    event.need_rescan()
        || matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(_)
                | EventKind::Remove(_)
                | EventKind::Any
                | EventKind::Access(AccessKind::Close(AccessMode::Write))
        )
}

fn dispatch(config: &Config, queue: &WorkQueue, event: Event) {
    if event.need_rescan() {
        queue.request_repair();
    }
    if !is_change(&event) {
        return;
    }
    for path in event.paths {
        if !files::eligible(config, &path) {
            continue;
        }
        if path.is_dir() {
            // Child changes have their own events. A directory timestamp is not a new subtree.
            if matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(_))) {
                continue;
            }
            // A renamed album can already contain tracks before the watcher sees its directory.
            for entry in WalkDir::new(&path)
                .follow_links(config.follow_symlinks)
                .into_iter()
                .filter_entry(|entry| files::eligible(config, entry.path()))
            {
                match entry {
                    Ok(entry) if entry.file_type().is_dir() => {
                        queue.submit(entry.into_path(), false);
                    }
                    Ok(_) => {}
                    Err(_) => queue.request_repair(),
                }
            }
        } else if (files::is_audio(&path) || files::is_lrc(&path))
            && let Some(parent) = path.parent()
        {
            queue.submit(parent.to_owned(), false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{config, tags, write_flac};
    use notify::event::{AccessKind, AccessMode, CreateKind, MetadataKind, RemoveKind};
    use tempfile::TempDir;

    #[test]
    fn reads_and_directory_timestamps_do_not_request_library_work() {
        let root = TempDir::new().unwrap();
        let settings = config(root.path());
        let queue = WorkQueue::new(1, Shutdown::default());
        let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(root.path().join("song.flac"));
        assert!(!is_change(&read));
        assert!(is_change(&Event::new(EventKind::Access(
            AccessKind::Close(AccessMode::Write)
        ))));
        dispatch(&settings, &queue, read);
        dispatch(
            &settings,
            &queue,
            Event::new(EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::WriteTime,
            )))
            .add_path(root.path().to_owned()),
        );
        assert!(queue.idle());
        assert!(!queue.take_repair_request());
    }

    #[test]
    fn lyric_removal_and_populated_directory_events_reach_the_queue() {
        let root = TempDir::new().unwrap();
        let config = config(root.path());
        let queue = WorkQueue::new(4, Shutdown::default());
        dispatch(
            &config,
            &queue,
            Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.path().join("song.lrc")),
        );
        let item = queue.take().unwrap();
        assert_eq!(item.directory, root.path());
        queue.finish(item, false);
        let album = root.path().join("imported/album");
        write_flac(&album.join("song.flac"), &tags());
        dispatch(
            &config,
            &queue,
            Event::new(EventKind::Create(CreateKind::Folder))
                .add_path(root.path().join("imported")),
        );
        let first = queue.take().unwrap();
        let second = queue.take().unwrap();
        assert!([first.directory.clone(), second.directory.clone()].contains(&album));
        queue.finish(first, false);
        queue.finish(second, false);
        assert!(!queue.take_repair_request());
    }

    #[test]
    fn overflow_requests_a_repair_scan_and_archives_are_ignored() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let config = config(root.path());
        let queue = WorkQueue::new(1, Shutdown::default());
        queue.submit(first, false);
        dispatch(
            &config,
            &queue,
            Event::new(EventKind::Create(CreateKind::File)).add_path(second.join("song.flac")),
        );
        assert!(queue.take_repair_request());
        dispatch(
            &config,
            &queue,
            Event::new(EventKind::Create(CreateKind::File))
                .add_path(root.path().join(files::QUARANTINE).join("song.lrc")),
        );
        assert!(!queue.take_repair_request());
    }

    #[cfg(unix)]
    #[test]
    fn native_and_scan_paths_share_symlink_policy() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let config = config(root.path());
        assert!(!files::eligible(&config, &link.join("song.flac")));
        let queue = WorkQueue::new(1, Shutdown::default());
        dispatch(
            &config,
            &queue,
            Event::new(EventKind::Create(CreateKind::File)).add_path(link.join("song.flac")),
        );
        assert!(queue.idle());
        assert!(!queue.take_repair_request());
    }
}
