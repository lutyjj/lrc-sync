#![cfg(target_os = "linux")]

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::TempDir;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn event_mode_does_not_open_unchanged_audio_at_startup_or_while_idle() {
    unchanged_audio_is_not_opened("native", Duration::from_millis(500));
}

#[test]
fn polling_does_not_open_unchanged_audio_between_metadata_checks() {
    unchanged_audio_is_not_opened("poll", Duration::from_millis(3200));
}

fn unchanged_audio_is_not_opened(mode: &str, idle: Duration) {
    let library = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let audio = library.path().join("song.mp3");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ),
        &audio,
    )
    .unwrap();
    fs::write(audio.with_extension("lrc"), "existing curated lyrics").unwrap();
    let (tx, rx) = mpsc::channel();
    let mut observer = RecommendedWatcher::new(
        move |event: notify::Result<Event>| {
            if let Ok(event) = event
                && matches!(event.kind, EventKind::Access(_))
            {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    )
    .unwrap();
    observer.watch(&audio, RecursiveMode::NonRecursive).unwrap();
    let log = state.path().join("daemon.log");
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_lrc-sync"))
            .env_clear()
            .env("LRCSYNC_MUSIC_DIR", library.path())
            .env("LRCSYNC_DB_PATH", state.path().join("cache.sqlite3"))
            .env("LRCSYNC_WATCH_MODE", mode)
            .env("LRCSYNC_POLL_INTERVAL_SECONDS", "1")
            .env("LRCSYNC_STARTUP_SCAN", "false")
            .env("LRCSYNC_FALLBACK_SCAN_SECONDS", "0")
            .stdout(Stdio::from(fs::File::create(&log).unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&log)
        .unwrap()
        .contains("started filesystem watcher")
    {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        assert!(Instant::now() < deadline, "watcher did not start");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rx.recv_timeout(idle).is_err(),
        "idle {mode} mode opened unchanged audio"
    );
    let staging = TempDir::new().unwrap();
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ),
        staging.path().join("new.mp3"),
    )
    .unwrap();
    fs::write(staging.path().join("new.lrc"), "imported curated lyrics").unwrap();
    fs::rename(staging.path(), library.path().join("imported")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cache_has_lyrics(&state.path().join("cache.sqlite3")) {
        assert!(Instant::now() < deadline, "import event was not processed");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "importing an album reopened unrelated audio"
    );
}

#[test]
fn startup_reconciliation_reuses_file_cache_without_reopening_unchanged_files() {
    let library = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let audio = library.path().join("song.mp3");
    let sidecar = audio.with_extension("lrc");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ),
        &audio,
    )
    .unwrap();
    fs::write(&sidecar, "existing curated lyrics").unwrap();
    let cache = state.path().join("cache.sqlite3");
    for round in 0..2 {
        let (tx, rx) = mpsc::channel();
        let mut observer = RecommendedWatcher::new(
            move |event: notify::Result<Event>| {
                if let Ok(event) = event
                    && matches!(event.kind, EventKind::Access(_))
                {
                    let _ = tx.send(event);
                }
            },
            notify::Config::default(),
        )
        .unwrap();
        observer.watch(&audio, RecursiveMode::NonRecursive).unwrap();
        observer
            .watch(&sidecar, RecursiveMode::NonRecursive)
            .unwrap();
        let log = state.path().join(format!("daemon-{round}.log"));
        let mut daemon = Daemon(
            Command::new(env!("CARGO_BIN_EXE_lrc-sync"))
                .env_clear()
                .env("LRCSYNC_MUSIC_DIR", library.path())
                .env("LRCSYNC_DB_PATH", &cache)
                .stdout(Stdio::from(fs::File::create(&log).unwrap()))
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            let completed = fs::read_to_string(&log)
                .unwrap()
                .contains("initial library scan queued");
            let cached = cache_has_lyrics(&cache);
            if completed && cached {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "startup reconciliation did not finish"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let event = rx.recv_timeout(Duration::from_millis(500));
        if round == 0 {
            assert!(event.is_ok(), "first pass must read the uncached files");
        } else {
            assert!(
                event.is_err(),
                "unchanged cached files were reopened on restart"
            );
        }
        drop(daemon);
    }
}

fn cache_has_lyrics(path: &Path) -> bool {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT count(*) FROM tracks", [], |row| {
                row.get::<_, i64>(0)
            })
            .ok()
        })
        .is_some_and(|count| count == 1)
}

#[test]
fn losing_the_native_root_exits_and_restart_watches_the_replacement() {
    for rename in [false, true] {
        let temporary = TempDir::new().unwrap();
        let library = temporary.path().join("music");
        fs::create_dir(&library).unwrap();
        let state = TempDir::new().unwrap();
        let cache = state.path().join("cache.sqlite3");
        let log = state.path().join("daemon.log");
        let spawn = || {
            Daemon(
                Command::new(env!("CARGO_BIN_EXE_lrc-sync"))
                    .env_clear()
                    .env("LRCSYNC_MUSIC_DIR", &library)
                    .env("LRCSYNC_DB_PATH", &cache)
                    .env("LRCSYNC_WATCH_MODE", "native")
                    .env("LRCSYNC_FALLBACK_SCAN_SECONDS", "0")
                    .stdout(Stdio::from(fs::File::create(&log).unwrap()))
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        };
        let wait_started = |daemon: &mut Daemon| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read_to_string(&log)
                .unwrap()
                .contains("started filesystem watcher")
            {
                assert!(daemon.0.try_wait().unwrap().is_none());
                assert!(Instant::now() < deadline, "watcher did not start");
                thread::sleep(Duration::from_millis(10));
            }
        };
        let mut daemon = spawn();
        wait_started(&mut daemon);
        if rename {
            fs::rename(&library, temporary.path().join("old-music")).unwrap();
        } else {
            fs::remove_dir(&library).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                assert!(!status.success(), "watch loss must fail the daemon");
                break;
            }
            assert!(Instant::now() < deadline, "lost native watch stayed alive");
            thread::sleep(Duration::from_millis(10));
        }
        drop(daemon);
        fs::create_dir(&library).unwrap();
        let mut replacement = spawn();
        wait_started(&mut replacement);
        fs::write(library.join("imported.lrc"), "replacement library lyrics").unwrap();
        fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/whitespace-artist.mp3"
            ),
            library.join("imported.mp3"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cache_has_lyrics(&cache) {
            assert!(replacement.0.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline, "replacement import was missed");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn write_close_refreshes_cached_lyrics_without_reopening_audio() {
    check_write_close(false);
}

#[test]
fn write_close_updates_hardlinked_sidecars_across_directories_without_full_repair() {
    check_write_close(true);
}

fn check_write_close(hardlinked: bool) {
    use rustix::mm::{MapFlags, MsyncFlags, ProtFlags};

    struct Mapping {
        address: *mut std::ffi::c_void,
        _file: fs::File,
    }
    impl Mapping {
        fn write(&mut self, text: &[u8; 16]) {
            // SAFETY: this test owns a writable 16-byte file mapping; text does not overlap it.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), self.address.cast(), text.len());
                rustix::mm::msync(self.address, 16, MsyncFlags::SYNC).unwrap();
            }
        }
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: the mapping is live, exclusively owned, and has no Rust references.
            let _ = unsafe { rustix::mm::munmap(self.address, 16) };
        }
    }

    let library = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let audio = library.path().join("song.mp3");
    let sidecar = audio.with_extension("lrc");
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ),
        &audio,
    )
    .unwrap();
    fs::write(&sidecar, b"first lyric text").unwrap();
    let alias = library.path().join("other/linked.lrc");
    if hardlinked {
        fs::create_dir(alias.parent().unwrap()).unwrap();
        fs::copy(&audio, alias.with_extension("mp3")).unwrap();
        fs::hard_link(&sidecar, &alias).unwrap();
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&sidecar)
        .unwrap();
    // SAFETY: the test owns this 16-byte file and keeps its descriptor and mapping alive.
    let address = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            16,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            &file,
            0,
        )
    }
    .unwrap();
    let mut mapping = Mapping {
        address,
        _file: file,
    };
    mapping.write(b"second lyric txt");
    let (tx, rx) = mpsc::channel();
    let mut observer = RecommendedWatcher::new(
        move |event: notify::Result<Event>| {
            if let Ok(event) = event
                && matches!(event.kind, EventKind::Access(_))
            {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    )
    .unwrap();
    observer.watch(&audio, RecursiveMode::NonRecursive).unwrap();
    if hardlinked {
        observer
            .watch(&alias.with_extension("mp3"), RecursiveMode::NonRecursive)
            .unwrap();
    }
    let cache = state.path().join("cache.sqlite3");
    let log = state.path().join("daemon.log");
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_lrc-sync"))
            .env_clear()
            .env("LRCSYNC_MUSIC_DIR", library.path())
            .env("LRCSYNC_DB_PATH", &cache)
            .env("LRCSYNC_WATCH_MODE", "native")
            .env("LRCSYNC_FALLBACK_SCAN_SECONDS", "0")
            .stdout(Stdio::from(fs::File::create(log).unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let wait_for = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let matching = rusqlite::Connection::open_with_flags(
                &cache,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM tracks WHERE lyrics=?1",
                    [expected],
                    |row| row.get::<_, i64>(0),
                )
                .ok()
            });
            if matching == Some(if hardlinked { 2 } else { 1 }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cached lyrics did not update to {expected}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for("second lyric txt");
    while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
    mapping.write(b"third lyric text");
    drop(mapping);
    // Filesystems differ: repeated mmap writes may preserve the entire stamp or update it.
    // Both must deliver the edited content; db's restart regression also exercises a fixed stamp.
    wait_for("third lyric text");
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "sidecar edit reopened unchanged audio"
    );
    let removed = if hardlinked { &alias } else { &sidecar };
    fs::remove_file(removed).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !removed.exists() {
        assert!(Instant::now() < deadline, "edited lyrics were not restored");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(removed).unwrap(), "third lyric text");
    let conn =
        rusqlite::Connection::open_with_flags(&cache, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let repairs = conn
        .query_row(
            "SELECT count(*) FROM file_versions WHERE path=X''",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(repairs, 0, "notified writes must not rescan the library");
}
