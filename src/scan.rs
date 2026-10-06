use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use crate::{
    config::{Config, OrphanAction},
    db::{CacheDb, CachedResult, Origin},
    files::{self, FileStamp},
    lrclib::{LrclibClient, LyricsResult},
    queue::{Admission, WorkItem, WorkQueue},
    shutdown::Shutdown,
    tags::{TrackTags, read_tags},
};

static TRACK_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{1,3}[-._\s]+\s*").expect("valid track prefix regex"));

#[derive(Clone)]
pub struct Processor {
    inner: Arc<ProcessorInner>,
}

pub enum ProcessingOutcome {
    Complete,
    Retry,
    StaleRoute,
}

struct ProcessorInner {
    config: Config,
    db: CacheDb,
    lrclib: LrclibClient,
    shutdown: Shutdown,
}

struct Audio {
    path: PathBuf,
    physical_path: PathBuf,
    cache_path: PathBuf,
    tags: TrackTags,
    stamp: FileStamp,
    version: i64,
}

impl Processor {
    pub fn new(config: Config, db: CacheDb, lrclib: LrclibClient, shutdown: Shutdown) -> Self {
        Self {
            inner: Arc::new(ProcessorInner {
                config,
                db,
                lrclib,
                shutdown,
            }),
        }
    }

    /// Access policy uses the submitted path; filesystem work uses its pinned owner.
    pub fn process_work(&self, work: &WorkItem) -> Result<ProcessingOutcome> {
        let eligible = files::eligible(&self.inner.config, &work.directory);
        if !work
            .directory
            .canonicalize()
            .is_ok_and(|current| current == work.physical_directory())
        {
            return Ok(ProcessingOutcome::StaleRoute);
        }
        if !eligible {
            return Ok(ProcessingOutcome::Complete);
        }
        let mut config = self.inner.config.clone();
        config.music_dir = work.physical_directory().to_owned();
        let changed = Self::new(
            config,
            self.inner.db.clone(),
            self.inner.lrclib.clone(),
            self.inner.shutdown.clone(),
        )
        .process_directory(work.physical_directory())?;
        Ok(if changed {
            ProcessingOutcome::Retry
        } else {
            ProcessingOutcome::Complete
        })
    }

    /// Called by the directory's sole queue owner; true requests a fresh pass.
    pub fn process_directory(&self, directory: &Path) -> Result<bool> {
        if self.inner.shutdown.is_cancelled() || !files::eligible(&self.inner.config, directory) {
            return Ok(false);
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", directory.display()));
            }
        };
        let mut audio = BTreeMap::<OsString, Vec<PathBuf>>::new();
        let mut lrc = BTreeMap::<OsString, Vec<PathBuf>>::new();
        let mut reserved = BTreeSet::new();
        for entry in entries {
            let path = entry.path();
            let Some(stem) = path.file_stem() else {
                continue;
            };
            // Even an excluded audio symlink reserves its paired sidecar from deletion.
            if files::is_audio(&path) {
                reserved.insert(stem.to_owned());
            }
            if !files::eligible(&self.inner.config, &path) || !path.is_file() {
                continue;
            }
            if files::is_audio(&path) {
                audio.entry(stem.to_owned()).or_default().push(path);
            } else if files::is_lrc(&path) {
                lrc.entry(stem.to_owned()).or_default().push(path);
            }
        }
        if self.inner.config.orphan_action == OrphanAction::Reconcile {
            self.reconcile(&audio, &mut lrc)?;
        }
        let mut changed = false;
        for (stem, mut paths) in audio {
            if self.inner.shutdown.is_cancelled() {
                break;
            }
            paths.sort();
            let sidecars = lrc.remove(&stem).unwrap_or_default();
            if sidecars.len() > 1 {
                warn!(directory = %directory.display(), stem = ?stem, "multiple sidecars share a stem; preserving all candidates");
                continue;
            }
            match self.process_group(&paths, sidecars.first().map(PathBuf::as_path)) {
                Ok(retry) => changed |= retry,
                Err(err) => {
                    error!(path = %paths[0].display(), error = %format!("{err:#}"), "track processing failed; will retry on a later scan")
                }
            }
        }
        for (stem, paths) in lrc {
            if reserved.contains(&stem) {
                continue;
            }
            for path in paths {
                if self.inner.shutdown.is_cancelled() {
                    return Ok(changed);
                }
                // Imports may have added audio after the snapshot; classify again before mutation.
                if has_audio(&path)? || fs::symlink_metadata(&path)?.file_type().is_symlink() {
                    continue;
                }
                match self.inner.config.orphan_action {
                    OrphanAction::Keep | OrphanAction::Reconcile => {
                        debug!(path = %path.display(), "preserving unmatched lyric file")
                    }
                    OrphanAction::Quarantine => {
                        let target = files::quarantine(&path)?;
                        info!(from = %path.display(), to = %target.display(), "archived unmatched lyric file");
                    }
                    OrphanAction::Delete => {
                        fs::remove_file(&path)
                            .with_context(|| format!("deleting {}", path.display()))?;
                        info!(path = %path.display(), "deleted unmatched lyric file");
                    }
                }
            }
        }
        Ok(changed)
    }

    fn process_group(&self, paths: &[PathBuf], existing: Option<&Path>) -> Result<bool> {
        let mut audio = Vec::new();
        for path in paths {
            let Some(item) = self.audio(path)? else {
                return Ok(true);
            };
            audio.push(item);
        }
        let tags = &audio[0].tags;
        if audio
            .iter()
            .any(|item| item.tags.normalized() != tags.normalized())
        {
            warn!(path = %audio[0].path.display(), "different recordings share one sidecar filename; refusing to choose a track");
            return Ok(false);
        }
        let destination = audio[0].path.with_extension("lrc");
        if !files::eligible(&self.inner.config, &destination) {
            return Ok(false);
        }
        if let Some(path) = existing {
            let lyrics = self.lyrics(path)?;
            let stale_generated = self
                .inner
                .db
                .sidecar_record(&audio[0].cache_path.with_extension("lrc"))?
                .is_some_and(|record| {
                    record.tags != tags.normalized()
                        && record.origin == Origin::Generated
                        && record.lyrics == lyrics
                });
            if !stale_generated {
                for item in &audio {
                    self.inner.db.ingest(&item.cache_path, tags, &lyrics)?;
                }
                return Ok(false);
            }
            if !self.still_current(&audio) {
                return Ok(true);
            }
            let archive = files::quarantine(path)?;
            info!(path = %path.display(), archive = %archive.display(), "archived generated lyrics for a replaced recording");
            if path.exists() {
                return Ok(true);
            }
        }
        let cached = self.inner.db.cached(&audio[0].cache_path, tags)?;
        let result = if matches!(cached, Some(CachedResult::Found { .. })) {
            cached
        } else {
            self.inner
                .db
                .find_cached_lyrics(tags)?
                .map(|lyrics| CachedResult::Found {
                    lyrics,
                    origin: Origin::Generated,
                })
                .or(cached)
        };
        let (result, looked_up) = match result {
            Some(result) => (result, false),
            None => {
                info!(artist = %tags.artist, title = %tags.title, "fetching lyrics");
                (
                    match self
                        .inner
                        .lrclib
                        .fetch(tags, self.inner.config.clean_fallback)
                    {
                        LyricsResult::Found(lyrics) => CachedResult::Found {
                            lyrics,
                            origin: Origin::Generated,
                        },
                        LyricsResult::NotFound => CachedResult::NotFound,
                        LyricsResult::Instrumental => CachedResult::Instrumental,
                        LyricsResult::Ambiguous => {
                            warn!(artist = %tags.artist, title = %tags.title, "ambiguous recording matches; leaving lyrics unchanged");
                            return Ok(false);
                        }
                        LyricsResult::TemporaryFailure(reason) => {
                            warn!(artist = %tags.artist, title = %tags.title, error = %reason, "lyrics lookup failed; will retry on a later scan");
                            return Ok(false);
                        }
                        LyricsResult::Cancelled => return Ok(false),
                    },
                    true,
                )
            }
        };
        if self.inner.shutdown.is_cancelled() {
            return Ok(false);
        }
        if !self.still_current(&audio) {
            return Ok(true);
        }
        if let CachedResult::Found { lyrics, .. } = &result {
            // The sidecar namespace includes uppercase extensions and nonregular collisions.
            let candidates = sidecars_for(&audio[0].path)?;
            if !candidates.is_empty() {
                self.ingest_collision(&audio, tags, &candidates)?;
                return Ok(false);
            }
            // Commit provenance first. A failed or interrupted publish can then be restored.
            for item in &audio {
                self.inner.db.store(&item.cache_path, tags, &result)?;
            }
            if !files::write_new(&destination, lyrics)? {
                self.ingest_collision(&audio, tags, &sidecars_for(&audio[0].path)?)?;
                return Ok(false);
            }
            let others: Vec<_> = sidecars_for(&audio[0].path)?
                .into_iter()
                .filter(|path| path != &destination)
                .collect();
            if !others.is_empty() {
                files::quarantine(&destination)?;
                self.ingest_collision(&audio, tags, &others)?;
                return Ok(false);
            }
            info!(artist = %tags.artist, title = %tags.title, path = %destination.display(), "published lyrics");
        } else if looked_up {
            for item in &audio {
                self.inner.db.store(&item.cache_path, tags, &result)?;
            }
        }
        Ok(!self.still_current(&audio))
    }

    fn ingest_collision(&self, audio: &[Audio], tags: &TrackTags, paths: &[PathBuf]) -> Result<()> {
        if paths.len() != 1 || !files::eligible(&self.inner.config, &paths[0]) {
            warn!(path = %audio[0].path.display(), "preserving ambiguous or excluded sidecar collision");
            return Ok(());
        }
        let lyrics = self.lyrics(&paths[0])?;
        for item in audio {
            self.inner.db.store(
                &item.cache_path,
                tags,
                &CachedResult::Found {
                    lyrics: lyrics.clone(),
                    origin: Origin::Curated,
                },
            )?;
        }
        Ok(())
    }

    fn still_current(&self, audio: &[Audio]) -> bool {
        audio.iter().all(|item| {
            files::eligible(&self.inner.config, &item.path)
                && FileStamp::read(&item.path).is_ok_and(|stamp| stamp == item.stamp)
                && self
                    .inner
                    .db
                    .file_version(&item.physical_path)
                    .is_ok_and(|version| version == item.version)
                && (cfg!(unix)
                    || read_tags(&item.path)
                        .is_ok_and(|tags| tags.normalized() == item.tags.normalized()))
        })
    }

    fn audio(&self, path: &Path) -> Result<Option<Audio>> {
        let stamp = FileStamp::read(path)?;
        let physical = path.canonicalize()?;
        let version = self.inner.db.file_version(&physical)?;
        let cached = self.inner.db.cached_tags(&physical, &stamp, version)?;
        let tags = match &cached {
            Some(tags) => tags.clone(),
            None => read_tags(path)?,
        };
        if FileStamp::read(path)? != stamp
            || path.canonicalize()? != physical
            || self.inner.db.file_version(&physical)? != version
        {
            return Ok(None);
        }
        if cached.is_none() {
            self.inner
                .db
                .store_tags(&physical, &stamp, version, &tags)?;
        }
        Ok(Some(Audio {
            path: path.to_owned(),
            physical_path: physical,
            cache_path: cache_path(path)?,
            tags,
            stamp,
            version,
        }))
    }

    fn lyrics(&self, path: &Path) -> Result<String> {
        let stamp = FileStamp::read_sidecar(path)?;
        let physical = cache_path(path)?;
        let version = self.inner.db.file_version(&physical)?;
        let cached = self
            .inner
            .db
            .cached_lyric_file(&physical, &stamp, version)?;
        let lyrics = match &cached {
            Some(lyrics) => lyrics.clone(),
            None => files::read_lyrics(path)?,
        };
        if FileStamp::read_sidecar(path)? != stamp
            || cache_path(path)? != physical
            || self.inner.db.file_version(&physical)? != version
        {
            bail!("sidecar changed while reading {}", path.display());
        }
        if cached.is_none() {
            self.inner
                .db
                .store_lyric_file(&physical, &stamp, version, &lyrics)?;
        }
        Ok(lyrics)
    }

    fn reconcile(
        &self,
        audio: &BTreeMap<OsString, Vec<PathBuf>>,
        lrc: &mut BTreeMap<OsString, Vec<PathBuf>>,
    ) -> Result<()> {
        let groups: Vec<_> = audio
            .iter()
            .filter_map(|(stem, paths)| {
                let tags = paths
                    .iter()
                    .map(|path| {
                        self.audio(path)
                            .map(|item| item.map(|item| item.tags.normalized()))
                    })
                    .collect::<Result<Vec<_>>>()
                    .ok()?
                    .into_iter()
                    .collect::<Option<Vec<_>>>()?;
                Some((stem, tags))
            })
            .collect();
        for (stem, candidates) in &groups {
            if lrc.contains_key(*stem)
                || candidates
                    .iter()
                    .any(|candidate| candidate != &candidates[0])
            {
                continue;
            }
            let tags = &candidates[0];
            let title = OsStr::new(&tags.title);
            if groups
                .iter()
                .filter(|(_, others)| {
                    others
                        .iter()
                        .any(|other| title_key(OsStr::new(&other.title)) == title_key(title))
                })
                .count()
                != 1
            {
                continue;
            }
            let Some(name) =
                unique_orphan_match(title, lrc.keys().filter(|name| !audio.contains_key(*name)))
            else {
                continue;
            };
            let sources = &lrc[&name];
            if sources.len() != 1 {
                continue;
            }
            let source = sources[0].clone();
            let destination = audio[*stem][0].with_extension("lrc");
            if has_audio(&source)? {
                continue;
            }
            let Ok(lyrics) = self.lyrics(&source) else {
                continue;
            };
            if let Some(record) = self
                .inner
                .db
                .sidecar_record(&cache_path(&source)?.with_extension("lrc"))?
                && (record.tags != *tags
                    || (record.origin == Origin::Generated && record.lyrics == lyrics))
            {
                // Generated content uses exact cache reuse under its new owner.
                continue;
            }
            if audio[*stem].iter().any(|path| {
                !self
                    .audio(path)
                    .is_ok_and(|item| item.is_some_and(|item| item.tags.normalized() == *tags))
            }) {
                continue;
            }
            if files::move_new(&source, &destination)? {
                lrc.remove(&name);
                lrc.insert((*stem).clone(), vec![destination.clone()]);
                info!(from = %source.display(), to = %destination.display(), "reconciled uniquely tagged title");
            }
        }
        Ok(())
    }
}

pub fn queue_library(config: &Config, queue: &WorkQueue) -> Result<usize> {
    let directories = WalkDir::new(&config.music_dir)
        .follow_links(config.follow_symlinks)
        .into_iter()
        .filter_entry(|entry| files::eligible(config, entry.path()))
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_dir() => Some(entry.into_path()),
            Ok(_) => None,
            Err(err) => {
                warn!(error = %err, "music directory traversal failed");
                None
            }
        });
    Ok(queue_directories(directories, queue))
}

fn queue_directories(directories: impl IntoIterator<Item = PathBuf>, queue: &WorkQueue) -> usize {
    let mut count = 0;
    for directory in directories {
        match queue.submit(directory, true) {
            Admission::Accepted => count += 1,
            Admission::Unavailable => continue,
            Admission::Stopped => break,
            Admission::Full => unreachable!("blocking admission returned full"),
        }
    }
    count
}

fn cache_path(path: &Path) -> Result<PathBuf> {
    Ok(path
        .parent()
        .context("file has no parent")?
        .canonicalize()?
        .join(path.file_name().context("file has no filename")?))
}

fn has_audio(sidecar: &Path) -> Result<bool> {
    let parent = sidecar.parent().context("sidecar has no parent")?;
    for entry in fs::read_dir(parent)? {
        let path = entry?.path();
        if files::is_audio(&path) && path.file_stem() == sidecar.file_stem() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sidecars_for(audio: &Path) -> Result<Vec<PathBuf>> {
    fs::read_dir(audio.parent().context("audio has no parent")?)?
        .map(|entry| entry.map(|entry| entry.path()))
        .filter(|entry| match entry {
            Ok(path) => files::is_lrc(path) && path.file_stem() == audio.file_stem(),
            Err(_) => true,
        })
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn title_key(value: &OsStr) -> Option<String> {
    let value: String = value
        .to_str()?
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    if value.is_empty() { None } else { Some(value) }
}

fn unique_orphan_match<'a>(
    title: &OsStr,
    names: impl Iterator<Item = &'a OsString>,
) -> Option<OsString> {
    let wanted = title_key(title)?;
    let mut matches = names.filter(|name| {
        if title_key(name).is_some_and(|candidate| candidate == wanted) {
            return true;
        }
        let Some(name) = name.to_str() else {
            return false;
        };
        let numbered = TRACK_PREFIX.replace(name, "");
        title_key(OsStr::new(numbered.as_ref())).is_some_and(|candidate| candidate == wanted)
    });
    let result = matches.next()?.clone();
    if matches.next().is_some() {
        None
    } else {
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Response, TestServer, config, lyrics, tags, write_flac};
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use tempfile::TempDir;

    fn processor(config: Config, server: &TestServer) -> Processor {
        let db = CacheDb::open(config.db_file.clone(), config.retry_not_found_days).unwrap();
        let shutdown = Shutdown::default();
        Processor::new(
            config,
            db,
            LrclibClient::test_client(server.url.clone(), shutdown.clone()),
            shutdown,
        )
    }

    #[test]
    fn actual_metadata_fixture_parses_the_declared_recording() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("song.flac");
        write_flac(&path, &tags());
        assert_eq!(read_tags(&path).unwrap(), tags());
    }

    #[test]
    fn disappeared_directory_admission_does_not_stop_later_scan_work() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("disappeared");
        let later = root.path().join("later");
        fs::create_dir(&later).unwrap();
        let queue = WorkQueue::new(2, Shutdown::default());
        assert_eq!(queue_directories([missing, later.clone()], &queue), 1);
        let item = queue.take().unwrap();
        assert_eq!(item.physical_directory(), later);
        queue.finish(item, false);
        assert!(queue.idle());
    }

    #[test]
    fn uppercase_sidecars_are_paired_in_every_orphan_mode() {
        for action in [
            OrphanAction::Keep,
            OrphanAction::Reconcile,
            OrphanAction::Quarantine,
            OrphanAction::Delete,
        ] {
            let dir = TempDir::new().unwrap();
            let audio = dir.path().join("song.flac");
            write_flac(&audio, &tags());
            let sidecar = dir.path().join("song.LRC");
            fs::write(&sidecar, "curated lyrics").unwrap();
            let server = TestServer::new(|_| panic!("paired sidecar must not request lyrics"));
            let mut config = config(dir.path());
            config.orphan_action = action;
            processor(config, &server)
                .process_directory(dir.path())
                .unwrap();
            assert_eq!(files::read_lyrics(&sidecar).unwrap(), "curated lyrics");
            assert!(!dir.path().join(files::QUARANTINE).exists());
        }
    }

    #[test]
    fn edited_lyrics_are_restored_after_deletion() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("song.flac");
        write_flac(&audio, &tags());
        let lrc = audio.with_extension("lrc");
        fs::write(&lrc, "original lyrics").unwrap();
        let server = TestServer::new(|_| panic!("restoration uses the edited cache"));
        let processor = processor(config(dir.path()), &server);
        processor.process_directory(dir.path()).unwrap();
        fs::write(&lrc, "corrected lyrics").unwrap();
        processor.process_directory(dir.path()).unwrap();
        fs::remove_file(&lrc).unwrap();
        processor.process_directory(dir.path()).unwrap();
        assert_eq!(files::read_lyrics(&lrc).unwrap(), "corrected lyrics");
    }

    #[cfg(unix)]
    #[test]
    fn directory_aliases_preserve_provenance_when_audio_is_retagged() {
        let root = TempDir::new().unwrap();
        let album = root.path().join("album");
        let alias = root.path().join("alias");
        let audio = album.join("song.flac");
        write_flac(&audio, &tags());
        std::os::unix::fs::symlink(&album, &alias).unwrap();
        let server = TestServer::new(|request| {
            let mut track = tags();
            track.artist = request.query["artist_name"].clone();
            let text = if track.artist == "Corrected Artist" {
                "new recording"
            } else {
                "old recording"
            };
            Response::json(200, lyrics(&track, text))
        });
        let mut settings = config(root.path());
        settings.follow_symlinks = true;
        let processor = processor(settings, &server);
        processor.process_directory(&album).unwrap();
        processor.process_directory(&alias).unwrap();
        let mut corrected = tags();
        corrected.artist = "Corrected Artist".into();
        write_flac(&audio, &corrected);
        processor.process_directory(&alias).unwrap();
        assert_eq!(
            files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
            "new recording"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(
            files::read_lyrics(&album.join(files::QUARANTINE).join("song.lrc")).unwrap(),
            "old recording"
        );
    }

    #[test]
    fn retagging_a_cached_miss_reaches_the_http_lookup() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("song.flac");
        write_flac(&audio, &tags());
        let server = TestServer::new(|request| {
            if request
                .query
                .get("artist_name")
                .is_some_and(|artist| artist == "Corrected Artist")
            {
                let mut tags = tags();
                tags.artist = "Corrected Artist".into();
                Response::json(200, lyrics(&tags, "correct recording"))
            } else {
                Response::json(404, serde_json::json!({}))
            }
        });
        let processor = processor(config(dir.path()), &server);
        processor.process_directory(dir.path()).unwrap();
        let mut corrected = tags();
        corrected.artist = "Corrected Artist".into();
        write_flac(&audio, &corrected);
        processor.process_directory(dir.path()).unwrap();
        assert_eq!(
            files::read_lyrics(&audio.with_extension("lrc")).unwrap(),
            "correct recording"
        );
    }

    #[test]
    fn ambiguous_search_never_writes_or_caches_a_miss() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("song.flac");
        write_flac(&audio, &tags());
        let server = TestServer::new(|request| {
            if request.path == "/get" {
                return Response::json(404, serde_json::json!({}));
            }
            let mut record = tags();
            record.album = "Other Album".into();
            Response::json(
                200,
                serde_json::json!([lyrics(&record, "first"), lyrics(&record, "second")]),
            )
        });
        let processor = processor(config(dir.path()), &server);
        processor.process_directory(dir.path()).unwrap();
        assert!(!audio.with_extension("lrc").exists());
        assert!(processor.inner.db.record(&audio).unwrap().is_none());
    }

    #[test]
    fn artist_changes_during_lookup_cannot_publish_stale_lyrics() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("song.flac");
        write_flac(&audio, &tags());
        let path = audio.clone();
        let barrier = Arc::new(Barrier::new(2));
        let reached = barrier.clone();
        let server = TestServer::new(move |_| {
            reached.wait();
            reached.wait();
            Response::json(200, lyrics(&tags(), "stale recording"))
        });
        let processor = processor(config(dir.path()), &server);
        let worker = processor.clone();
        let directory = dir.path().to_owned();
        let handle = std::thread::spawn(move || worker.process_directory(&directory).unwrap());
        barrier.wait();
        let mut updated = tags();
        updated.artist = "Replacement Artist".into();
        write_flac(&path, &updated);
        barrier.wait();
        assert!(handle.join().unwrap());
        assert!(!audio.with_extension("lrc").exists());
        assert!(processor.inner.db.record(&audio).unwrap().is_none());
    }

    #[test]
    fn a_curated_file_created_during_lookup_is_preserved() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("song.flac");
        write_flac(&audio, &tags());
        let lrc = audio.with_extension("lrc");
        let path = lrc.clone();
        let server = TestServer::new(move |_| {
            fs::write(&path, "curated while fetching").unwrap();
            Response::json(200, lyrics(&tags(), "downloaded"))
        });
        let processor = processor(config(dir.path()), &server);
        processor.process_directory(dir.path()).unwrap();
        assert_eq!(files::read_lyrics(&lrc).unwrap(), "curated while fetching");
    }

    #[test]
    fn conflicting_formats_sharing_a_stem_are_not_silently_assigned_one_recording() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("song.flac");
        write_flac(&first, &tags());
        let second = dir.path().join("song.mp3");
        fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/whitespace-artist.mp3"
            ),
            &second,
        )
        .unwrap();
        let server =
            TestServer::new(|_| panic!("conflicting recording identity must not be fetched"));
        processor(config(dir.path()), &server)
            .process_directory(dir.path())
            .unwrap();
        assert!(!first.with_extension("lrc").exists());
    }

    #[test]
    fn repeated_quarantine_scans_do_not_walk_the_archive() {
        let dir = TempDir::new().unwrap();
        let orphan = dir.path().join("orphan.lrc");
        fs::write(&orphan, "first").unwrap();
        let server = TestServer::new(|_| panic!("no audio"));
        let mut config = config(dir.path());
        config.orphan_action = OrphanAction::Quarantine;
        let processor = processor(config.clone(), &server);
        processor.process_directory(dir.path()).unwrap();
        let queue = WorkQueue::new(4, Shutdown::default());
        assert_eq!(queue_library(&config, &queue).unwrap(), 1);
        let item = queue.take().unwrap();
        processor.process_directory(&item.directory).unwrap();
        queue.finish(item, false);
        assert!(
            !dir.path()
                .join(files::QUARANTINE)
                .join(files::QUARANTINE)
                .exists()
        );
        fs::write(&orphan, "second").unwrap();
        processor.process_directory(dir.path()).unwrap();
        assert_eq!(
            fs::read_dir(dir.path().join(files::QUARANTINE))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn reconciliation_requires_unique_title_agreement_and_preserves_variants() {
        let names = [
            OsString::from("01 - Unrelated Song"),
            OsString::from("01 - Correct Song"),
        ];
        assert_eq!(
            unique_orphan_match(OsStr::new("Correct Song"), names.iter()),
            Some(names[1].clone())
        );
        assert_eq!(
            unique_orphan_match(OsStr::new("01 - Other Song"), names.iter()),
            None
        );
        assert_eq!(
            unique_orphan_match(OsStr::new("Correct Song (Live)"), names.iter()),
            None
        );
        let duplicate = [
            OsString::from("01 - Correct Song"),
            OsString::from("Correct Song"),
        ];
        assert_eq!(
            unique_orphan_match(OsStr::new("Correct Song"), duplicate.iter()),
            None
        );
    }

    #[test]
    fn reconciliation_runs_only_for_one_recording_and_one_title() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("Correct Song.flac");
        let mut track = tags();
        track.title = "Correct Song".into();
        write_flac(&first, &track);
        let orphan = dir.path().join("01 - Correct Song.lrc");
        fs::write(&orphan, "curated match").unwrap();
        let unrelated = dir.path().join("01 - Unrelated Song.lrc");
        fs::write(&unrelated, "wrong title").unwrap();
        let server = TestServer::new(|_| panic!("reconciled lyrics are curated"));
        let mut settings = config(dir.path());
        settings.orphan_action = OrphanAction::Reconcile;
        processor(settings.clone(), &server)
            .process_directory(dir.path())
            .unwrap();
        assert_eq!(
            files::read_lyrics(&first.with_extension("lrc")).unwrap(),
            "curated match"
        );
        assert!(unrelated.exists());
        assert!(!orphan.exists());
        fs::remove_file(first.with_extension("lrc")).unwrap();
        fs::write(&orphan, "ambiguous match").unwrap();
        let mut other = track;
        other.artist = "Different Artist".into();
        write_flac(&dir.path().join("Correct Song.mp3"), &other);
        processor(settings, &server)
            .process_directory(dir.path())
            .unwrap();
        assert!(orphan.exists());
        assert!(!first.with_extension("lrc").exists());
    }

    #[test]
    fn bad_metadata_does_not_delay_a_ready_track() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a-broken.mp3"), "unfinished").unwrap();
        let good = dir.path().join("z-good.flac");
        write_flac(&good, &tags());
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let server = TestServer::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Response::json(200, lyrics(&tags(), "ready track"))
        });
        processor(config(dir.path()), &server)
            .process_directory(dir.path())
            .unwrap();
        assert_eq!(
            files::read_lyrics(&good.with_extension("lrc")).unwrap(),
            "ready track"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
