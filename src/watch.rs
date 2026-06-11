use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::ThreadPool;
use tracing::{debug, info, warn};

use crate::{config::Config, scan::Processor};

pub fn watch_music(config: Config, processor: Processor, pool: Arc<ThreadPool>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            if tx.send(event).is_err() {
                warn!("filesystem watcher receiver dropped");
            }
        },
        NotifyConfig::default(),
    )?;

    watcher
        .watch(&config.music_dir, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", config.music_dir.display()))?;
    info!(path = %config.music_dir.display(), "started filesystem watcher");

    for event in rx {
        match event {
            Ok(event) if should_handle(&event.kind) => {
                for path in event.paths {
                    submit_path(pool.as_ref(), processor.clone(), path);
                }
            }
            Ok(event) => {
                debug!(kind = ?event.kind, "ignoring filesystem event");
            }
            Err(err) => warn!(error = %err, "filesystem watch event failed"),
        }
    }

    Ok(())
}

fn should_handle(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
    )
}

fn submit_path(pool: &ThreadPool, processor: Processor, path: PathBuf) {
    pool.spawn(move || processor.process_watch_path(path));
}
