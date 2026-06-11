use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use rayon::ThreadPool;
use regex::Regex;
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use crate::{
    config::{Config, OrphanAction},
    db::{rel_path, CacheDb},
    lrclib::{LrclibClient, LyricsResult},
    tags::read_tags,
};

const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus"];

#[derive(Clone)]
pub struct Processor {
    inner: Arc<ProcessorInner>,
}

struct ProcessorInner {
    config: Config,
    db: CacheDb,
    lrclib: LrclibClient,
    active: Mutex<HashSet<PathBuf>>,
}

#[derive(Debug, Clone)]
struct TrackJob {
    audio_path: PathBuf,
    rel_path: String,
    lrc_path: PathBuf,
    source: JobSource,
}

#[derive(Debug, Clone, Copy)]
pub enum JobSource {
    Scan,
    Watch,
}

impl Processor {
    pub fn new(config: Config, db: CacheDb, lrclib: LrclibClient) -> Self {
        Self {
            inner: Arc::new(ProcessorInner {
                config,
                db,
                lrclib,
                active: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn process_watch_path(&self, path: PathBuf) {
        if !is_audio_file(&path) {
            return;
        }
        let rel_path = rel_path(&path, &self.inner.config.music_dir);
        if self.inner.db.is_not_found(&rel_path) {
            debug!(path = %path.display(), "skipping cached not_found watch event");
            return;
        }
        let lrc_path = path.with_extension("lrc");
        if lrc_path.exists() {
            return;
        }
        self.process_track(TrackJob {
            audio_path: path,
            rel_path,
            lrc_path,
            source: JobSource::Watch,
        });
    }

    fn process_track(&self, job: TrackJob) {
        {
            let mut active = self.inner.active.lock().expect("active set poisoned");
            if !active.insert(job.audio_path.clone()) {
                return;
            }
        }

        let result = self.process_track_inner(&job);
        self.inner
            .active
            .lock()
            .expect("active set poisoned")
            .remove(&job.audio_path);

        if let Err(err) = result {
            error!(path = %job.audio_path.display(), error = %err, "track processing failed");
        }
    }

    fn process_track_inner(&self, job: &TrackJob) -> Result<()> {
        let tags = match read_tags(&job.audio_path) {
            Ok(tags) => Some(tags),
            Err(err) if matches!(job.source, JobSource::Watch) => {
                debug!(path = %job.audio_path.display(), error = %err, "tag read failed for watch event; retrying once");
                thread::sleep(Duration::from_secs(3));
                Some(read_tags(&job.audio_path)?)
            }
            Err(err) => {
                warn!(path = %job.audio_path.display(), error = %err, "skipping file with unreadable tags");
                None
            }
        };
        let Some(tags) = tags else {
            return Ok(());
        };

        if job.lrc_path.exists() {
            let lyrics = fs::read_to_string(&job.lrc_path).ok();
            self.inner
                .db
                .mark_success(&job.rel_path, Some(&tags), lyrics.as_deref())?;
            return Ok(());
        }

        if let Some(lyrics) = self.inner.db.find_cached_lyrics(&tags)? {
            write_lyrics(&job.lrc_path, &lyrics)?;
            info!(artist = %tags.artist, title = %tags.title, "restored lyrics from local cache");
            self.inner
                .db
                .mark_success(&job.rel_path, Some(&tags), Some(&lyrics))?;
            return Ok(());
        }

        let source = match job.source {
            JobSource::Scan => "scan",
            JobSource::Watch => "watch",
        };
        info!(source, artist = %tags.artist, title = %tags.title, "fetching lyrics");

        let mut lookup_tags = tags.clone();
        let mut result = self.inner.lrclib.fetch(&lookup_tags);
        if matches!(result, LyricsResult::NotFound) && self.inner.config.clean_fallback {
            let cleaned = tags.cleaned();
            if cleaned.title != tags.title || cleaned.album != tags.album {
                info!(artist = %cleaned.artist, title = %cleaned.title, "retrying with cleaned metadata");
                lookup_tags = cleaned;
                result = self.inner.lrclib.fetch(&lookup_tags);
            }
        }

        match result {
            LyricsResult::Found(lyrics) => {
                write_lyrics(&job.lrc_path, &lyrics)?;
                info!(artist = %tags.artist, title = %tags.title, path = %job.lrc_path.display(), "saved lyrics");
                self.inner
                    .db
                    .mark_success(&job.rel_path, Some(&tags), Some(&lyrics))?;
            }
            LyricsResult::NotFound => {
                info!(artist = %tags.artist, title = %tags.title, "lyrics not found");
                self.inner.db.mark_not_found(&job.rel_path, &tags)?;
            }
            LyricsResult::TemporaryFailure => {
                warn!(artist = %lookup_tags.artist, title = %lookup_tags.title, "lyrics lookup failed temporarily; will retry later");
            }
        }

        Ok(())
    }
}

pub fn sync_library(pool: &ThreadPool, processor: Processor) -> Result<()> {
    info!("starting lyrics synchronization scan");
    processor.inner.db.load_memory_cache()?;

    let mut by_dir: HashMap<PathBuf, DirectoryFiles> = HashMap::new();
    for entry in WalkDir::new(&processor.inner.config.music_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(err) => {
                warn!(error = %err, "failed walking music directory entry");
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(parent) = path.parent() else {
            continue;
        };
        let dir = by_dir.entry(parent.to_path_buf()).or_default();
        if is_audio_file(path) {
            dir.audio.push(path.to_path_buf());
        } else if path
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lrc"))
        {
            dir.lrc.insert(file_name(path), path.to_path_buf());
        }
    }

    let mut jobs = Vec::new();
    let mut skipped_success = 0usize;
    let mut skipped_not_found = 0usize;
    let mut reconciled = 0usize;
    let mut orphaned = 0usize;

    for (dir, mut files) in by_dir {
        let mut missing = Vec::new();

        for audio_path in files.audio.drain(..) {
            let lrc_name = audio_path
                .file_stem()
                .map(|stem| format!("{}.lrc", stem.to_string_lossy()))
                .unwrap_or_else(|| "unknown.lrc".to_string());
            let rel = rel_path(&audio_path, &processor.inner.config.music_dir);

            if let Some(lrc_path) = files.lrc.remove(&lrc_name) {
                if !processor.inner.db.is_success(&rel) {
                    let tags = read_tags(&audio_path).ok();
                    let lyrics = fs::read_to_string(&lrc_path).ok();
                    processor
                        .inner
                        .db
                        .mark_success(&rel, tags.as_ref(), lyrics.as_deref())?;
                }
                skipped_success += 1;
            } else {
                missing.push(audio_path);
            }
        }

        if processor.inner.config.orphan_action == OrphanAction::Reconcile {
            reconcile_orphans(&processor, &mut files.lrc, &mut missing, &mut reconciled)?;
        }

        orphaned += handle_remaining_orphans(&processor.inner.config, &dir, files.lrc)?;

        for audio_path in missing {
            let rel = rel_path(&audio_path, &processor.inner.config.music_dir);
            if processor.inner.db.is_not_found(&rel) {
                skipped_not_found += 1;
                continue;
            }
            jobs.push(TrackJob {
                lrc_path: audio_path.with_extension("lrc"),
                audio_path,
                rel_path: rel,
                source: JobSource::Scan,
            });
        }
    }

    let total = jobs.len();
    info!(
        total,
        skipped_not_found, skipped_success, reconciled, orphaned, "scan planning complete"
    );

    pool.scope(|scope| {
        for job in jobs {
            let processor = processor.clone();
            scope.spawn(move |_| processor.process_track(job));
        }
    });

    info!("scan complete");
    Ok(())
}

fn reconcile_orphans(
    processor: &Processor,
    lrc_files: &mut HashMap<String, PathBuf>,
    missing: &mut Vec<PathBuf>,
    reconciled: &mut usize,
) -> Result<()> {
    let mut still_missing = Vec::new();

    for audio_path in missing.drain(..) {
        let Some(match_name) = find_orphan_match(&audio_path, lrc_files.keys()) else {
            still_missing.push(audio_path);
            continue;
        };
        let Some(src) = lrc_files.remove(&match_name) else {
            still_missing.push(audio_path);
            continue;
        };
        let dest = audio_path.with_extension("lrc");
        fs::rename(&src, &dest)
            .with_context(|| format!("renaming {} to {}", src.display(), dest.display()))?;
        let rel = rel_path(&audio_path, &processor.inner.config.music_dir);
        let tags = read_tags(&audio_path).ok();
        let lyrics = fs::read_to_string(&dest).ok();
        processor
            .inner
            .db
            .mark_success(&rel, tags.as_ref(), lyrics.as_deref())?;
        *reconciled += 1;
        info!(from = %src.display(), to = %dest.display(), "reconciled orphaned lyric file");
    }

    *missing = still_missing;
    Ok(())
}

fn handle_remaining_orphans(
    config: &Config,
    dir: &Path,
    lrc_files: HashMap<String, PathBuf>,
) -> Result<usize> {
    let count = lrc_files.len();
    match config.orphan_action {
        OrphanAction::Keep | OrphanAction::Reconcile => {
            for path in lrc_files.values() {
                debug!(path = %path.display(), "leaving unmatched lyric file in place");
            }
        }
        OrphanAction::Quarantine => {
            let quarantine_dir = dir.join(".lrcget-orphans");
            fs::create_dir_all(&quarantine_dir)?;
            for (name, path) in lrc_files {
                let dest = quarantine_dir.join(name);
                fs::rename(&path, &dest).with_context(|| {
                    format!(
                        "quarantining orphan {} to {}",
                        path.display(),
                        dest.display()
                    )
                })?;
                warn!(from = %path.display(), to = %dest.display(), "quarantined unmatched lyric file");
            }
        }
        OrphanAction::Delete => {
            for path in lrc_files.values() {
                fs::remove_file(path)
                    .with_context(|| format!("deleting orphan {}", path.display()))?;
                warn!(path = %path.display(), "deleted unmatched lyric file");
            }
        }
    }
    Ok(count)
}

fn write_lyrics(path: &Path, lyrics: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("lrc.tmp");
    fs::write(&tmp, lyrics).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn find_orphan_match<'a>(
    audio_path: &Path,
    lrc_names: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let audio_name = file_name(audio_path);
    let audio_track = track_number(&audio_name);
    let audio_clean = clean_name(&audio_name);
    let names: Vec<&String> = lrc_names.collect();

    if let Some(audio_track) = audio_track {
        if let Some(found) = names
            .iter()
            .copied()
            .find(|name| track_number(name).is_some_and(|candidate| candidate == audio_track))
        {
            return Some(found.clone());
        }
    }

    names
        .iter()
        .copied()
        .find(|name| clean_name(name) == audio_clean)
        .cloned()
}

fn track_number(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn clean_name(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_default();
    let leading = Regex::new(r"^\d+\s*[-._]?\s*").expect("valid regex");
    let bracketed = Regex::new(r"[\(\[\{].*?[\)\]\}]").expect("valid regex");
    let featured = Regex::new(r"(?i)\b(feat|ft)\b.*").expect("valid regex");
    let non_word = Regex::new(r"[\W_]+").expect("valid regex");
    let value = leading.replace(&stem, "");
    let value = bracketed.replace_all(&value, "");
    let value = featured.replace_all(&value, "");
    non_word.replace_all(&value, "").to_ascii_lowercase()
}

#[derive(Default)]
struct DirectoryFiles {
    audio: Vec<PathBuf>,
    lrc: HashMap<String, PathBuf>,
}
