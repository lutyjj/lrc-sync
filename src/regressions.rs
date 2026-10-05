use std::{
    fs,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::Connection;
use tempfile::TempDir;

use crate::{
    config::{Config, OrphanAction},
    db::{CacheDb, CachedResult, Origin},
    files,
    lrclib::LrclibClient,
    queue::{Admission, WorkQueue},
    scan::Processor,
    shutdown::Shutdown,
    test_support::{Response, TestServer, config, lyrics, tags, write_flac},
};

fn processor(config: &Config, server: &TestServer) -> Processor {
    let shutdown = Shutdown::default();
    Processor::new(
        config.clone(),
        CacheDb::open(config.db_file.clone(), 7).unwrap(),
        LrclibClient::test_client(server.url.clone(), shutdown.clone()),
        shutdown,
    )
}

fn found(text: &str) -> CachedResult {
    CachedResult::Found {
        lyrics: text.into(),
        origin: Origin::Generated,
    }
}

#[test]
fn replacing_an_audio_format_retains_generated_sidecar_provenance() {
    let root = TempDir::new().unwrap();
    let original = root.path().join("song.flac");
    let replacement = root.path().join("song.mp3");
    write_flac(&original, &tags());
    let server = TestServer::new(|request| {
        let track = crate::tags::TrackTags {
            artist: request.query["artist_name"].clone(),
            title: request.query["track_name"].clone(),
            album: request.query["album_name"].clone(),
            duration_secs: request.query["duration"].parse().unwrap(),
        };
        Response::json(200, lyrics(&track, &format!("lyrics for {}", track.artist)))
    });
    let settings = config(root.path());
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    fs::remove_file(&original).unwrap();
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ),
        &replacement,
    )
    .unwrap();
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    assert_eq!(
        files::read_lyrics(&replacement.with_extension("lrc")).unwrap(),
        "lyrics for Fixture Album Artist"
    );
    assert_eq!(
        files::read_lyrics(&root.path().join(files::QUARANTINE).join("song.lrc")).unwrap(),
        "lyrics for Fixture Artist"
    );
}

#[test]
fn format_rename_of_the_same_recording_preserves_generated_origin() {
    let root = TempDir::new().unwrap();
    let original = root.path().join("song.flac");
    let replacement = root.path().join("song.FLAC");
    write_flac(&original, &tags());
    let server = TestServer::new(|request| {
        let mut track = tags();
        track.artist = request.query["artist_name"].clone();
        Response::json(200, lyrics(&track, &format!("lyrics for {}", track.artist)))
    });
    let settings = config(root.path());
    let worker = processor(&settings, &server);
    worker.process_directory(root.path()).unwrap();
    fs::rename(&original, &replacement).unwrap();
    worker.process_directory(root.path()).unwrap();
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    assert_eq!(
        cache.cached(&replacement, &tags()).unwrap(),
        Some(found("lyrics for Fixture Artist"))
    );
    let mut changed = tags();
    changed.artist = "Replacement Artist".into();
    write_flac(&replacement, &changed);
    worker.process_directory(root.path()).unwrap();
    assert_eq!(
        files::read_lyrics(&replacement.with_extension("lrc")).unwrap(),
        "lyrics for Replacement Artist"
    );
}

#[test]
fn paired_tracks_still_make_orphan_title_matching_ambiguous() {
    let root = TempDir::new().unwrap();
    let paired = root.path().join("a.flac");
    let missing = root.path().join("b.flac");
    write_flac(&paired, &tags());
    fs::write(paired.with_extension("lrc"), "paired correction").unwrap();
    let mut other = tags();
    other.artist = "Another Artist".into();
    write_flac(&missing, &other);
    let orphan = root.path().join("01 - Fixture Song.lrc");
    fs::write(&orphan, "unknown recording").unwrap();
    let server = TestServer::new(|_| Response::json(404, serde_json::json!({})));
    let mut settings = config(root.path());
    settings.orphan_action = OrphanAction::Reconcile;
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(files::read_lyrics(&orphan).unwrap(), "unknown recording");
    assert!(!missing.with_extension("lrc").exists());
    assert_eq!(
        files::read_lyrics(&paired.with_extension("lrc")).unwrap(),
        "paired correction"
    );
}

#[cfg(unix)]
#[test]
fn directory_aliases_cannot_expose_archived_sidecars_to_cleanup() {
    for action in [OrphanAction::Delete, OrphanAction::Quarantine] {
        let root = TempDir::new().unwrap();
        let archived = root.path().join(files::QUARANTINE).join("album");
        fs::create_dir_all(&archived).unwrap();
        let sidecar = archived.join("preserved.lrc");
        fs::write(&sidecar, "archived correction").unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&archived, &alias).unwrap();
        let mut settings = config(root.path());
        settings.follow_symlinks = true;
        settings.orphan_action = action;
        let server = TestServer::new(|_| panic!("archives never initiate lookups"));
        let worker = processor(&settings, &server);
        let queue = WorkQueue::new(1, Shutdown::default());
        assert_eq!(queue.submit(alias.clone(), false), Admission::Accepted);
        let item = queue.take().unwrap();
        worker.process_work(&item).unwrap();
        queue.finish(item, false);
        assert_eq!(files::read_lyrics(&sidecar).unwrap(), "archived correction");
        worker.process_directory(&alias).unwrap();
        assert_eq!(files::read_lyrics(&sidecar).unwrap(), "archived correction");
        assert!(!archived.join(files::QUARANTINE).exists());
    }
}

#[test]
fn long_recordings_search_past_a_wrong_duration_candidate() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("long.flac");
    let mut track = tags();
    track.duration_secs = 4000;
    write_flac(&audio, &track);
    let expected = track.clone();
    let server = TestServer::new(move |request| {
        if request.path == "/get" {
            return Response::json(200, lyrics(&tags(), "wrong short recording"));
        }
        assert_eq!(request.path, "/search");
        assert_eq!(request.query["album_name"], expected.album);
        Response::json(
            200,
            serde_json::json!([lyrics(&expected, "correct long recording")]),
        )
    });
    let mut settings = config(root.path());
    settings.clean_fallback = false;
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "correct long recording"
    );
    let cache = CacheDb::open(settings.db_file, 7).unwrap();
    assert_eq!(
        cache.cached(&audio, &track).unwrap(),
        Some(found("correct long recording"))
    );
}

#[test]
fn rejected_cleaned_lookup_continues_to_validated_search() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    let mut track = tags();
    track.title = "Fixture Song (Remastered 2024)".into();
    write_flac(&audio, &track);
    let server = TestServer::new(|request| {
        if request.path == "/get" {
            if request.query["track_name"].contains("Remastered") {
                return Response::json(404, serde_json::json!({}));
            }
            let mut wrong = tags();
            wrong.duration_secs = 500;
            return Response::json(200, lyrics(&wrong, "wrong recording"));
        }
        Response::json(
            200,
            serde_json::json!([lyrics(&tags(), "validated recording")]),
        )
    });
    processor(&config(root.path()), &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "validated recording"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}

#[test]
fn generated_orphans_reuse_exact_identity_after_an_audio_rename() {
    let root = TempDir::new().unwrap();
    let original = root.path().join("Fixture Song.flac");
    let renamed = root.path().join("renamed.flac");
    write_flac(&original, &tags());
    let server = TestServer::new(|request| {
        let mut track = tags();
        track.artist = request.query["artist_name"].clone();
        Response::json(200, lyrics(&track, &format!("lyrics for {}", track.artist)))
    });
    let mut settings = config(root.path());
    settings.orphan_action = OrphanAction::Reconcile;
    let worker = processor(&settings, &server);
    worker.process_directory(root.path()).unwrap();
    fs::rename(&original, &renamed).unwrap();
    worker.process_directory(root.path()).unwrap();
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    assert_eq!(
        cache.cached(&renamed, &tags()).unwrap(),
        Some(found("lyrics for Fixture Artist"))
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let mut replacement = tags();
    replacement.artist = "Replacement Artist".into();
    write_flac(&renamed, &replacement);
    worker.process_directory(root.path()).unwrap();
    assert_eq!(
        files::read_lyrics(&renamed.with_extension("lrc")).unwrap(),
        "lyrics for Replacement Artist"
    );
    assert_eq!(
        files::read_lyrics(&root.path().join(files::QUARANTINE).join("renamed.lrc")).unwrap(),
        "lyrics for Fixture Artist"
    );
}

#[test]
fn known_orphan_identity_cannot_be_assigned_to_another_artist() {
    for curated in [false, true] {
        let root = TempDir::new().unwrap();
        let original = root.path().join("Fixture Song.flac");
        let renamed = root.path().join("renamed.flac");
        write_flac(&original, &tags());
        let server = TestServer::new(|request| {
            let mut track = tags();
            track.artist = request.query["artist_name"].clone();
            Response::json(200, lyrics(&track, &format!("lyrics for {}", track.artist)))
        });
        let mut settings = config(root.path());
        settings.orphan_action = OrphanAction::Reconcile;
        let worker = processor(&settings, &server);
        worker.process_directory(root.path()).unwrap();
        if curated {
            fs::write(original.with_extension("lrc"), "curated first recording").unwrap();
            worker.process_directory(root.path()).unwrap();
        }
        fs::rename(&original, &renamed).unwrap();
        let mut replacement = tags();
        replacement.artist = "Replacement Artist".into();
        write_flac(&renamed, &replacement);
        worker.process_directory(root.path()).unwrap();
        assert_eq!(
            files::read_lyrics(&renamed.with_extension("lrc")).unwrap(),
            "lyrics for Replacement Artist"
        );
        assert!(original.with_extension("lrc").exists());
    }
}

#[cfg(unix)]
#[test]
fn queued_coalesced_work_keeps_the_latest_valid_directory() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let queue = WorkQueue::new(2, Shutdown::default());
    queue.submit(alias.clone(), false);
    retarget(&alias, &b);
    queue.submit(a.clone(), false);
    queue.submit(b.clone(), false);
    let first = queue.take().unwrap();
    assert_eq!(first.physical_directory(), a);
    queue.finish(first, false);
    let second = queue.take().unwrap();
    assert_eq!(second.physical_directory(), b);
    queue.finish(second, false);
    assert!(queue.idle());
}

#[cfg(unix)]
#[test]
fn active_coalesced_work_keeps_the_latest_valid_directory_for_its_rerun() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let sentinel = root.path().join("sentinel");
    for path in [&a, &b, &sentinel] {
        fs::create_dir(path).unwrap();
    }
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let queue = WorkQueue::new(3, Shutdown::default());
    queue.submit(alias.clone(), false);
    let active = queue.take().unwrap();
    retarget(&alias, &b);
    queue.submit(a.clone(), false);
    queue.submit(b.clone(), false);
    queue.finish(active, false);
    queue.submit(sentinel.clone(), false);
    let other = queue.take().unwrap();
    assert_eq!(other.physical_directory(), b);
    queue.finish(other, false);
    let rerun = queue.take().unwrap();
    assert_eq!(rerun.physical_directory(), a);
    queue.finish(rerun, false);
    let final_item = queue.take().unwrap();
    assert_eq!(final_item.physical_directory(), sentinel);
    queue.finish(final_item, false);
    assert!(queue.idle());
}

#[cfg(unix)]
#[test]
fn discarded_queued_alias_requests_an_immediate_repair_scan() {
    stale_coalesced_alias_requests_repair(false);
}

#[cfg(unix)]
#[test]
fn discarded_running_alias_requests_an_immediate_repair_scan() {
    stale_coalesced_alias_requests_repair(true);
}

#[cfg(unix)]
fn stale_coalesced_alias_requests_repair(running: bool) {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let sentinel = root.path().join("sentinel");
    for path in [&a, &b, &sentinel] {
        fs::create_dir(path).unwrap();
    }
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let queue = WorkQueue::new(3, Shutdown::default());
    assert_eq!(queue.submit(a, false), Admission::Accepted);
    let active = running.then(|| queue.take().unwrap());
    assert_eq!(queue.submit(alias.clone(), false), Admission::Accepted);
    retarget(&alias, &b);
    if let Some(active) = active {
        queue.finish(active, false);
    }
    queue.submit(b.clone(), false);
    queue.submit(sentinel.clone(), false);
    let other = queue.take().unwrap();
    assert_eq!(other.physical_directory(), b);
    queue.finish(other, false);
    let final_item = queue.take().unwrap();
    assert_eq!(final_item.physical_directory(), sentinel);
    queue.finish(final_item, false);
    assert!(queue.idle());
    assert!(queue.take_repair_request());
    assert!(!queue.take_repair_request());
}

#[cfg(unix)]
#[test]
fn stale_running_route_repairs_its_original_owner_without_entering_an_archive() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join(files::QUARANTINE).join("b");
    let original = a.join("song.flac");
    let archived = b.join("song.flac");
    write_flac(&original, &tags());
    write_flac(&archived, &tags());
    fs::write(original.with_extension("lrc"), "curated original").unwrap();
    fs::write(archived.with_extension("lrc"), "archived content").unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let mut settings = config(root.path());
    settings.follow_symlinks = true;
    let server = TestServer::new(|_| panic!("curated restoration requires no lookup"));
    let worker = processor(&settings, &server);
    let queue = WorkQueue::new(4, Shutdown::default());
    queue.submit(alias.clone(), false);
    let item = queue.take().unwrap();
    retarget(&alias, &b);
    crate::process_item(&worker, &queue, item);
    assert!(queue.idle());
    assert!(queue.take_repair_request());
    assert!(!queue.take_repair_request());
    crate::scan::queue_library(&settings, &queue).unwrap();
    while !queue.idle() {
        crate::process_item(&worker, &queue, queue.take().unwrap());
    }
    let db = CacheDb::open(settings.db_file, 7).unwrap();
    assert!(db.record(&original).unwrap().is_some());
    assert!(db.record(&archived).unwrap().is_none());
    assert_eq!(
        fs::read_to_string(archived.with_extension("lrc")).unwrap(),
        "archived content"
    );
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn repeated_scans_preserve_a_miss_timestamp_until_the_lookup_expires() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let settings = config(root.path());
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    cache
        .store(&audio, &tags(), &CachedResult::NotFound)
        .unwrap();
    let sql = Connection::open(&settings.db_file).unwrap();
    let original = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3600;
    sql.execute("UPDATE tracks SET checked_at=?1", [original])
        .unwrap();
    let server = TestServer::new(|_| Response::json(404, serde_json::json!({})));
    let worker = processor(&settings, &server);
    for _ in 0..3 {
        worker.process_directory(root.path()).unwrap();
    }
    assert!(server.requests.lock().unwrap().is_empty());
    let checked: i64 = sql
        .query_row("SELECT checked_at FROM tracks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(checked, original);
    sql.execute("UPDATE tracks SET checked_at=0", []).unwrap();
    worker.process_directory(root.path()).unwrap();
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    let checked: i64 = sql
        .query_row("SELECT checked_at FROM tracks", [], |row| row.get(0))
        .unwrap();
    assert!(checked > original);
}

#[test]
fn failed_provenance_commit_cannot_publish_an_untracked_sidecar() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let settings = config(root.path());
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    let sql = Connection::open(&settings.db_file).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_store BEFORE INSERT ON tracks BEGIN SELECT RAISE(ABORT,'fixture store failure'); END;").unwrap();
    let server = TestServer::new(|request| {
        let mut track = tags();
        track.artist = request.query["artist_name"].clone();
        Response::json(200, lyrics(&track, &format!("lyrics for {}", track.artist)))
    });
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert!(!audio.with_extension("lrc").exists());
    assert!(cache.record(&audio).unwrap().is_none());
    sql.execute_batch("DROP TRIGGER fail_store;").unwrap();
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    let mut replacement = tags();
    replacement.artist = "Replacement Artist".into();
    write_flac(&audio, &replacement);
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "lyrics for Replacement Artist"
    );
    assert_eq!(
        files::read_lyrics(&root.path().join(files::QUARANTINE).join("song.lrc")).unwrap(),
        "lyrics for Fixture Artist"
    );
}

#[test]
fn committed_lookup_recovers_an_interrupted_publication() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let settings = config(root.path());
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    cache
        .store(&audio, &tags(), &found("committed lookup"))
        .unwrap();
    let server = TestServer::new(|_| panic!("committed lookup restores without HTTP"));
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "committed lookup"
    );
    assert_eq!(
        cache.cached(&audio, &tags()).unwrap(),
        Some(found("committed lookup"))
    );
}

#[test]
fn copied_curated_content_is_generated_at_its_new_destination() {
    let root = TempDir::new().unwrap();
    let original = root.path().join("a.flac");
    let copy = root.path().join("b.flac");
    write_flac(&original, &tags());
    fs::write(original.with_extension("lrc"), "curated original").unwrap();
    write_flac(&copy, &tags());
    let server = TestServer::new(|_| {
        let mut track = tags();
        track.artist = "Replacement Artist".into();
        Response::json(200, lyrics(&track, "replacement recording"))
    });
    let settings = config(root.path());
    let worker = processor(&settings, &server);
    worker.process_directory(root.path()).unwrap();
    let mut replacement = tags();
    replacement.artist = "Replacement Artist".into();
    write_flac(&copy, &replacement);
    worker.process_directory(root.path()).unwrap();
    assert_eq!(
        files::read_lyrics(&copy.with_extension("lrc")).unwrap(),
        "replacement recording"
    );
    assert_eq!(
        files::read_lyrics(&original.with_extension("lrc")).unwrap(),
        "curated original"
    );
}

#[test]
fn uppercase_curated_sidecar_created_during_lookup_wins() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let uppercase = audio.with_extension("LRC");
    let created = uppercase.clone();
    let server = TestServer::new(move |_| {
        fs::write(&created, "late curated correction").unwrap();
        Response::json(200, lyrics(&tags(), "generated lyrics"))
    });
    let settings = config(root.path());
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert!(!audio.with_extension("lrc").exists());
    assert_eq!(
        files::read_lyrics(&uppercase).unwrap(),
        "late curated correction"
    );
    let cache = CacheDb::open(settings.db_file, 7).unwrap();
    assert_eq!(
        cache.cached(&audio, &tags()).unwrap(),
        Some(CachedResult::Found {
            lyrics: "late curated correction".into(),
            origin: Origin::Curated
        })
    );
}

#[test]
fn reconciliation_keeps_numeric_title_content() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("99 Fixture Balloons.flac");
    let mut track = tags();
    track.title = "99 Fixture Balloons".into();
    write_flac(&audio, &track);
    let orphan = root.path().join("01 - Fixture Balloons.lrc");
    fs::write(&orphan, "different title").unwrap();
    let server = TestServer::new(|_| Response::json(404, serde_json::json!({})));
    let mut settings = config(root.path());
    settings.orphan_action = OrphanAction::Reconcile;
    processor(&settings, &server)
        .process_directory(root.path())
        .unwrap();
    assert!(orphan.exists());
    assert!(!audio.with_extension("lrc").exists());
}

#[cfg(unix)]
#[test]
fn excluded_sidecar_symlink_never_enters_the_cache() {
    let root = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let sidecar = audio.with_extension("lrc");
    let target = external.path().join("outside.txt");
    fs::write(&target, "outside content").unwrap();
    std::os::unix::fs::symlink(&target, &sidecar).unwrap();
    let settings = config(root.path());
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    cache
        .store(&audio, &tags(), &found("known valid lyrics"))
        .unwrap();
    let server = TestServer::new(|_| panic!("cache hit does not fetch"));
    let worker = processor(&settings, &server);
    worker.process_directory(root.path()).unwrap();
    assert_eq!(
        cache.cached(&audio, &tags()).unwrap(),
        Some(found("known valid lyrics"))
    );
    fs::remove_file(&sidecar).unwrap();
    worker.process_directory(root.path()).unwrap();
    assert_eq!(files::read_lyrics(&sidecar).unwrap(), "known valid lyrics");
    assert_eq!(fs::read_to_string(target).unwrap(), "outside content");
}

#[cfg(unix)]
#[test]
fn special_sidecar_collision_does_not_block_a_worker() {
    let root = TempDir::new().unwrap();
    let audio = root.path().join("song.flac");
    write_flac(&audio, &tags());
    let sidecar = audio.with_extension("lrc");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&sidecar)
            .status()
            .unwrap()
            .success()
    );
    let settings = config(root.path());
    let cache = CacheDb::open(settings.db_file.clone(), 7).unwrap();
    cache
        .store(&audio, &tags(), &found("valid lyrics"))
        .unwrap();
    let server = TestServer::new(|_| panic!("cache hit"));
    let worker = processor(&settings, &server);
    let (send, receive) = std::sync::mpsc::channel();
    let directory = root.path().to_owned();
    let handle = thread::spawn(move || send.send(worker.process_directory(&directory)).unwrap());
    receive
        .recv_timeout(Duration::from_secs(1))
        .expect("FIFO collision stalled a worker")
        .unwrap();
    handle.join().unwrap();
    assert_eq!(
        cache.cached(&audio, &tags()).unwrap(),
        Some(found("valid lyrics"))
    );
}

#[cfg(unix)]
#[test]
fn directory_aliases_restore_the_last_manual_correction() {
    let root = TempDir::new().unwrap();
    let album = root.path().join("album");
    let alias = root.path().join("alias");
    let audio = album.join("song.flac");
    write_flac(&audio, &tags());
    std::os::unix::fs::symlink(&album, &alias).unwrap();
    fs::write(audio.with_extension("lrc"), "original").unwrap();
    let server = TestServer::new(|_| panic!("curated restoration"));
    let mut settings = config(root.path());
    settings.follow_symlinks = true;
    let worker = processor(&settings, &server);
    worker.process_directory(&album).unwrap();
    fs::write(audio.with_extension("lrc"), "corrected").unwrap();
    worker.process_directory(&alias).unwrap();
    fs::remove_file(audio.with_extension("lrc")).unwrap();
    worker.process_directory(&album).unwrap();
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "corrected"
    );
}

#[cfg(unix)]
fn retarget(alias: &Path, directory: &Path) {
    fs::remove_file(alias).unwrap();
    std::os::unix::fs::symlink(directory, alias).unwrap();
}

#[cfg(unix)]
#[test]
fn retargeted_queued_alias_cannot_create_two_owners_for_one_directory() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let alias = root.path().join("alias");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let queue = WorkQueue::new(2, Shutdown::default());
    queue.submit(alias.clone(), false);
    retarget(&alias, &b);
    queue.submit(b.clone(), false);
    let item = queue.take().unwrap();
    assert_eq!(item.physical_directory(), b);
    queue.finish(item, false);
    assert!(queue.idle());
}

#[cfg(unix)]
#[test]
fn capacity_blocked_admission_resolves_the_current_alias() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let occupied = root.path().join("occupied");
    let alias = root.path().join("alias");
    for path in [&a, &b, &occupied] {
        fs::create_dir(path).unwrap();
    }
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let queue = Arc::new(WorkQueue::new(1, Shutdown::default()));
    queue.submit(occupied, false);
    let item = queue.take().unwrap();
    let waiting = queue.clone();
    let logical = alias.clone();
    let producer = thread::spawn(move || waiting.submit(logical, true));
    thread::sleep(Duration::from_millis(30));
    retarget(&alias, &b);
    queue.finish(item, false);
    assert_eq!(producer.join().unwrap(), Admission::Accepted);
    assert_eq!(queue.submit(b.clone(), false), Admission::Accepted);
    let item = queue.take().unwrap();
    assert_eq!(item.physical_directory(), b);
    queue.finish(item, false);
    assert!(queue.idle());
}

#[cfg(unix)]
#[test]
fn active_directory_work_stays_on_its_pinned_owner() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let alias = root.path().join("alias");
    let audio = a.join("song.flac");
    write_flac(&audio, &tags());
    fs::create_dir(&b).unwrap();
    std::os::unix::fs::symlink(&a, &alias).unwrap();
    let changing_alias = alias.clone();
    let new_target = b.clone();
    let server = TestServer::new(move |_| {
        retarget(&changing_alias, &new_target);
        Response::json(200, lyrics(&tags(), "recording A"))
    });
    let mut settings = config(root.path());
    settings.follow_symlinks = true;
    let queue = WorkQueue::new(1, Shutdown::default());
    queue.submit(alias, false);
    let item = queue.take().unwrap();
    processor(&settings, &server).process_work(&item).unwrap();
    queue.finish(item, false);
    assert_eq!(
        files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
        "recording A"
    );
    assert!(!b.join("song.lrc").exists());
}

#[test]
fn cancellation_unblocks_the_initial_scan_while_http_is_active() {
    let root = TempDir::new().unwrap();
    for index in 0..12 {
        write_flac(
            &root.path().join(format!("album-{index}/song.flac")),
            &tags(),
        );
    }
    let server = TestServer::new(|_| {
        let mut response = Response::json(200, lyrics(&tags(), "fixture lyrics"));
        response.body_delay = Duration::from_millis(250);
        response
    });
    let mut settings = config(root.path());
    settings.concurrency = 1;
    settings.max_pending_dirs = 1;
    let shutdown = Shutdown::default();
    let cancel = shutdown.clone();
    let client = LrclibClient::test_client(server.url.clone(), shutdown.clone());
    let daemon = thread::spawn(move || crate::run(settings, shutdown, client));
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    cancel.cancel();
    daemon.join().unwrap().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    for index in 0..12 {
        assert!(!root.path().join(format!("album-{index}/song.lrc")).exists());
    }
}
