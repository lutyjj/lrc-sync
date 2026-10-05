use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::shutdown::Shutdown;

#[derive(Debug, Clone)]
pub struct WorkItem {
    key: PathBuf,
    pub directory: PathBuf,
}

impl WorkItem {
    pub fn physical_directory(&self) -> &Path {
        &self.key
    }
}

enum Entry {
    Queued(PathBuf),
    Running { directory: PathBuf, dirty: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Unavailable,
    Full,
    Stopped,
}

#[derive(Default)]
struct State {
    ready: VecDeque<PathBuf>,
    entries: HashMap<PathBuf, Entry>,
    closed: bool,
}

/// Bounds both queued and running directories; canonical keys serialize symlink aliases.
pub struct WorkQueue {
    state: Mutex<State>,
    changed: Condvar,
    capacity: usize,
    shutdown: Shutdown,
    repair: AtomicBool,
}

impl WorkQueue {
    pub fn new(capacity: usize, shutdown: Shutdown) -> Self {
        assert!(capacity > 0);
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            capacity,
            shutdown,
            repair: AtomicBool::new(false),
        }
    }

    /// Blocking admission waits for capacity; an unavailable path never stops other work.
    pub fn submit(&self, directory: PathBuf, wait: bool) -> Admission {
        let mut state = self.state.lock().expect("work queue poisoned");
        loop {
            if state.closed || self.shutdown.is_cancelled() {
                return Admission::Stopped;
            }
            let Ok(key) = fs::canonicalize(&directory) else {
                return Admission::Unavailable;
            };
            if let Some(entry) = state.entries.get_mut(&key) {
                match entry {
                    Entry::Queued(latest) => *latest = directory,
                    Entry::Running {
                        directory: latest,
                        dirty,
                    } => {
                        *latest = directory;
                        *dirty = true;
                    }
                }
                return Admission::Accepted;
            }
            if state.entries.len() < self.capacity {
                state.entries.insert(key.clone(), Entry::Queued(directory));
                state.ready.push_back(key);
                self.changed.notify_all();
                return Admission::Accepted;
            }
            if !wait {
                self.request_repair();
                return Admission::Full;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .expect("work queue poisoned")
                .0;
        }
    }

    pub fn take(&self) -> Option<WorkItem> {
        let mut state = self.state.lock().expect("work queue poisoned");
        loop {
            if state.closed || self.shutdown.is_cancelled() {
                return None;
            }
            if let Some(key) = state.ready.pop_front() {
                let Some(Entry::Queued(directory)) = state.entries.remove(&key) else {
                    unreachable!("queued directory missing");
                };
                if !fs::canonicalize(&directory).is_ok_and(|current| current == key) {
                    self.request_repair();
                    self.changed.notify_all();
                    continue;
                }
                state.entries.insert(
                    key.clone(),
                    Entry::Running {
                        directory: directory.clone(),
                        dirty: false,
                    },
                );
                return Some(WorkItem { key, directory });
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .expect("work queue poisoned")
                .0;
        }
    }

    pub fn finish(&self, item: WorkItem, changed_during_processing: bool) {
        let mut state = self.state.lock().expect("work queue poisoned");
        let Some(Entry::Running { directory, dirty }) = state.entries.remove(&item.key) else {
            unreachable!("running directory missing");
        };
        if !state.closed && !self.shutdown.is_cancelled() && (dirty || changed_during_processing) {
            state
                .entries
                .insert(item.key.clone(), Entry::Queued(directory));
            state.ready.push_back(item.key);
        }
        self.changed.notify_all();
    }

    pub fn close(&self) {
        let mut state = self.state.lock().expect("work queue poisoned");
        state.closed = true;
        state.ready.clear();
        state
            .entries
            .retain(|_, entry| matches!(entry, Entry::Running { .. }));
        self.changed.notify_all();
    }

    pub fn request_repair(&self) {
        self.repair.store(true, Ordering::Release);
    }

    pub fn take_repair_request(&self) -> bool {
        self.repair.swap(false, Ordering::AcqRel)
    }

    #[cfg(test)]
    pub fn idle(&self) -> bool {
        self.state.lock().unwrap().entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread, time::Instant};
    use tempfile::TempDir;

    #[test]
    fn events_coalesce_before_admission_and_running_changes_get_one_rerun() {
        let dir = TempDir::new().unwrap();
        let queue = WorkQueue::new(1, Shutdown::default());
        for _ in 0..20 {
            assert_eq!(queue.submit(dir.path().into(), false), Admission::Accepted);
        }
        let work = queue.take().unwrap();
        for _ in 0..20 {
            assert_eq!(queue.submit(dir.path().into(), false), Admission::Accepted);
        }
        queue.finish(work, false);
        let rerun = queue.take().unwrap();
        queue.finish(rerun, false);
        assert!(queue.idle());
    }

    #[test]
    fn capacity_counts_running_work_and_reports_overflow() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let queue = WorkQueue::new(1, Shutdown::default());
        assert_eq!(
            queue.submit(first.path().into(), false),
            Admission::Accepted
        );
        let work = queue.take().unwrap();
        assert_eq!(queue.submit(second.path().into(), false), Admission::Full);
        queue.finish(work, false);
        assert_eq!(
            queue.submit(second.path().into(), false),
            Admission::Accepted
        );
    }

    #[test]
    fn cancel_releases_blocked_producer_and_discards_pending_work() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let shutdown = Shutdown::default();
        let queue = Arc::new(WorkQueue::new(1, shutdown.clone()));
        queue.submit(first.path().into(), false);
        let producer = queue.clone();
        let path = second.path().to_owned();
        let handle = thread::spawn(move || producer.submit(path, true));
        let start = Instant::now();
        shutdown.cancel();
        queue.close();
        assert_eq!(handle.join().unwrap(), Admission::Stopped);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(queue.take().is_none());
        assert!(queue.idle());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_one_owner() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("album");
        fs::create_dir(&dir).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&dir, &alias).unwrap();
        let queue = WorkQueue::new(1, Shutdown::default());
        assert_eq!(queue.submit(dir, false), Admission::Accepted);
        assert_eq!(queue.submit(alias, false), Admission::Accepted);
        let item = queue.take().unwrap();
        queue.finish(item, false);
        assert!(queue.idle());
    }
}
