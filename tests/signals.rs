#![cfg(unix)]

use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn sigterm_exits_successfully_after_joining_workers() {
    let library = TempDir::new().unwrap();
    let cache = library.path().join("cache.sqlite3");
    let log_path = library.path().join("daemon.log");
    let output = fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lrc-sync"))
        .env_clear()
        .env("LRCSYNC_MUSIC_DIR", library.path())
        .env("LRCSYNC_DB_PATH", &cache)
        .env("LRCSYNC_WATCH_MODE", "native")
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&log_path)
        .unwrap()
        .contains("initial library scan queued")
    {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "daemon did not finish startup: {}",
                fs::read_to_string(&log_path).unwrap()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    let status = Command::new("/bin/sh")
        .args([
            "-c",
            "kill -TERM \"$1\"",
            "lrc-sync-signal-test",
            &child.id().to_string(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("SIGTERM did not stop the daemon");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        fs::read_to_string(log_path)
            .unwrap()
            .contains("shutdown complete; workers and watcher joined")
    );
}
