use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct Shutdown(Arc<(Mutex<bool>, Condvar)>);

impl Shutdown {
    pub fn cancel(&self) {
        let (cancelled, wake) = self.0.as_ref();
        *cancelled.lock().expect("shutdown lock poisoned") = true;
        wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.0.lock().expect("shutdown lock poisoned")
    }

    /// Returns false when cancellation interrupts the wait.
    pub fn wait(&self, duration: Duration) -> bool {
        let (cancelled, wake) = self.0.as_ref();
        let cancelled = cancelled.lock().expect("shutdown lock poisoned");
        let (cancelled, _) = wake
            .wait_timeout_while(cancelled, duration, |cancelled| !*cancelled)
            .expect("shutdown lock poisoned");
        !*cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn cancellation_wakes_all_waiters() {
        let shutdown = Shutdown::default();
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let shutdown = shutdown.clone();
                thread::spawn(move || shutdown.wait(Duration::from_secs(60)))
            })
            .collect();
        let start = Instant::now();
        shutdown.cancel();
        for handle in handles {
            assert!(!handle.join().unwrap());
        }
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
