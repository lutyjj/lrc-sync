use std::{
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use notify::{
    Config as NotifyConfig, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode,
    Watcher,
};
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::{
    config::{Config, WatchMode},
    files,
    queue::WorkQueue,
    shutdown::Shutdown,
};

pub fn start(config: Config, queue: Arc<WorkQueue>, shutdown: Shutdown) -> Result<JoinHandle<()>> {
    let (tx, rx) = mpsc::sync_channel(256);
    let overflow = queue.clone();
    let callback = move |event| {
        if let Err(mpsc::TrySendError::Full(_)) = tx.try_send(event) {
            overflow.request_repair();
        }
    };
    let options = NotifyConfig::default()
        .with_follow_symlinks(config.follow_symlinks)
        .with_poll_interval(config.poll_interval);
    let mut watcher: Box<dyn Watcher + Send> = match config.watch_mode {
        WatchMode::Poll => Box::new(PollWatcher::new(callback, options)?),
        WatchMode::Native => Box::new(RecommendedWatcher::new(callback, options)?),
    };
    watcher
        .watch(&config.music_dir, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", config.music_dir.display()))?;
    info!(mode = ?config.watch_mode, path = %config.music_dir.display(), "started filesystem watcher");
    thread::Builder::new()
        .name("lrc-sync-watcher".into())
        .spawn(move || {
            let _watcher = watcher;
            while !shutdown.is_cancelled() {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(event)) => dispatch(&config, &queue, event),
                    Ok(Err(err)) => {
                        warn!(error = %err, "filesystem event failed; requesting repair scan");
                        queue.request_repair();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .context("starting watcher dispatcher")
}

fn dispatch(config: &Config, queue: &WorkQueue, event: Event) {
    if event.need_rescan() {
        queue.request_repair();
    }
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    ) {
        return;
    }
    for path in event.paths {
        if !files::eligible(config, &path) {
            continue;
        }
        if path.is_dir() {
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
    use notify::event::{CreateKind, RemoveKind};
    use tempfile::TempDir;

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
