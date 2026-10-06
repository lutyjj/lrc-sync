use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{files::FileStamp, tags::TrackTags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Generated,
    Curated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedResult {
    Found { lyrics: String, origin: Origin },
    NotFound,
    Instrumental,
}

#[derive(Debug)]
pub struct TrackRecord {
    pub tags: TrackTags,
    pub result: CachedResult,
    checked_at: i64,
}

pub struct SidecarRecord {
    pub tags: TrackTags,
    pub lyrics: String,
    pub origin: Origin,
}

#[derive(Clone)]
pub struct CacheDb {
    conn: Arc<Mutex<Connection>>,
    retry_seconds: i64,
}

impl CacheDb {
    pub fn open(path: PathBuf, retry_not_found_days: u64) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tracks (
                path BLOB PRIMARY KEY,
                artist TEXT NOT NULL,
                title TEXT NOT NULL,
                album TEXT NOT NULL,
                duration INTEGER NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('found','not_found','instrumental')),
                lyrics TEXT,
                origin TEXT CHECK(origin IN ('generated','curated')),
                checked_at INTEGER NOT NULL,
                CHECK((status = 'found' AND lyrics IS NOT NULL AND length(lyrics) > 0 AND origin IS NOT NULL)
                      OR (status != 'found' AND lyrics IS NULL AND origin IS NULL))
             );
             CREATE INDEX IF NOT EXISTS tracks_identity ON tracks(artist,title,album,duration);
             CREATE TABLE IF NOT EXISTS sidecars (
                path BLOB PRIMARY KEY,
                artist TEXT NOT NULL,
                title TEXT NOT NULL,
                album TEXT NOT NULL,
                duration INTEGER NOT NULL,
                lyrics TEXT NOT NULL CHECK(length(lyrics) > 0),
                origin TEXT NOT NULL CHECK(origin IN ('generated','curated'))
             );
             CREATE TABLE IF NOT EXISTS audio_tags (
                path BLOB PRIMARY KEY,
                stamp BLOB NOT NULL,
                artist TEXT NOT NULL,
                title TEXT NOT NULL,
                album TEXT NOT NULL,
                duration INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS lyric_files (
                path BLOB PRIMARY KEY,
                stamp BLOB NOT NULL,
                lyrics TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS file_versions (
                path BLOB PRIMARY KEY,
                version INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS file_identities (
                path BLOB PRIMARY KEY,
                identity BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS files_by_identity ON file_identities(identity);"
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            retry_seconds: i64::try_from(retry_not_found_days)?
                .checked_mul(86_400)
                .context("cache retry interval overflow")?,
        })
    }

    /// Native writes are stronger evidence than an unchanged metadata stamp.
    pub fn invalidate_files(&self, paths: &[PathBuf]) -> Result<()> {
        let mut conn = self.conn.lock().expect("cache lock poisoned");
        let tx = conn.transaction()?;
        for path in paths {
            tx.execute(
                "INSERT INTO file_versions(path,version) VALUES(?1,1)
                 ON CONFLICT(path) DO UPDATE SET version=version+1",
                [path.as_os_str().as_encoded_bytes()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn invalidate_file(&self, path: &Path) -> Result<()> {
        self.invalidate_files(&[path.to_owned()])
    }

    pub fn file_version(&self, path: &Path) -> Result<i64> {
        Ok(self.conn.lock().expect("cache lock poisoned").query_row(
            "SELECT coalesce(sum(version),0) FROM file_versions WHERE path=?1 OR path=X''",
            [path.as_os_str().as_encoded_bytes()],
            |row| row.get(0),
        )?)
    }

    pub fn invalidate_all_snapshots(&self) -> Result<()> {
        // Absolute file paths cannot collide with the empty key reserved for the library epoch.
        self.invalidate_file(Path::new(""))
    }

    pub fn paths_sharing_file(&self, path: &Path) -> Result<Vec<PathBuf>> {
        #[cfg(unix)]
        {
            use std::{ffi::OsString, os::unix::ffi::OsStringExt};
            let current = FileStamp::read(path)
                .ok()
                .and_then(|stamp| stamp.identity());
            let conn = self.conn.lock().expect("cache lock poisoned");
            let identity = match current {
                Some(identity) => Some(identity.to_vec()),
                None => conn
                    .query_row(
                        "SELECT identity FROM file_identities WHERE path=?1",
                        [path.as_os_str().as_encoded_bytes()],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?,
            };
            let Some(identity) = identity else {
                return Ok(Vec::new());
            };
            let mut query = conn.prepare("SELECT path FROM file_identities WHERE identity=?1")?;
            Ok(query
                .query_map([identity], |row| {
                    Ok(PathBuf::from(OsString::from_vec(row.get(0)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Vec::new())
        }
    }

    fn index_file(conn: &Connection, path: &Path, stamp: &FileStamp) -> Result<()> {
        if let Some(identity) = stamp.identity() {
            conn.execute(
                "INSERT INTO file_identities(path,identity) VALUES(?1,?2)
                ON CONFLICT(path) DO UPDATE SET identity=excluded.identity
                WHERE file_identities.identity!=excluded.identity",
                params![path.as_os_str().as_encoded_bytes(), identity.as_slice()],
            )?;
        }
        Ok(())
    }

    pub fn cached_tags(
        &self,
        path: &Path,
        stamp: &FileStamp,
        version: i64,
    ) -> Result<Option<TrackTags>> {
        if !cfg!(unix) {
            return Ok(None);
        }
        let stamp = serde_json::to_vec(&(stamp, version))?;
        let conn = self.conn.lock().expect("cache lock poisoned");
        Ok(conn
            .query_row(
                "SELECT artist,title,album,duration FROM audio_tags WHERE path=?1 AND stamp=?2",
                params![path.as_os_str().as_encoded_bytes(), stamp],
                |row| {
                    Ok(TrackTags {
                        artist: row.get(0)?,
                        title: row.get(1)?,
                        album: row.get(2)?,
                        duration_secs: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn store_tags(
        &self,
        path: &Path,
        stamp: &FileStamp,
        version: i64,
        tags: &TrackTags,
    ) -> Result<()> {
        if !cfg!(unix) {
            return Ok(());
        }
        let fingerprint = serde_json::to_vec(&(stamp, version))?;
        let mut conn = self.conn.lock().expect("cache lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO audio_tags(path,stamp,artist,title,album,duration) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(path) DO UPDATE SET stamp=excluded.stamp,artist=excluded.artist,
                 title=excluded.title,album=excluded.album,duration=excluded.duration",
            params![path.as_os_str().as_encoded_bytes(), fingerprint, tags.artist, tags.title, tags.album, tags.duration_secs],
        )?;
        Self::index_file(&tx, path, stamp)?;
        tx.commit()?;
        Ok(())
    }

    pub fn cached_lyric_file(
        &self,
        path: &Path,
        stamp: &FileStamp,
        version: i64,
    ) -> Result<Option<String>> {
        if !cfg!(unix) {
            return Ok(None);
        }
        let stamp = serde_json::to_vec(&(stamp, version))?;
        let conn = self.conn.lock().expect("cache lock poisoned");
        Ok(conn
            .query_row(
                "SELECT lyrics FROM lyric_files WHERE path=?1 AND stamp=?2",
                params![path.as_os_str().as_encoded_bytes(), stamp],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn store_lyric_file(
        &self,
        path: &Path,
        stamp: &FileStamp,
        version: i64,
        lyrics: &str,
    ) -> Result<()> {
        if !cfg!(unix) {
            return Ok(());
        }
        let fingerprint = serde_json::to_vec(&(stamp, version))?;
        let mut conn = self.conn.lock().expect("cache lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO lyric_files(path,stamp,lyrics) VALUES(?1,?2,?3)
             ON CONFLICT(path) DO UPDATE SET stamp=excluded.stamp,lyrics=excluded.lyrics",
            params![path.as_os_str().as_encoded_bytes(), fingerprint, lyrics],
        )?;
        Self::index_file(&tx, path, stamp)?;
        tx.commit()?;
        Ok(())
    }

    pub fn record(&self, path: &Path) -> Result<Option<TrackRecord>> {
        let conn = self.conn.lock().expect("cache lock poisoned");
        let row = conn.query_row(
            "SELECT artist,title,album,duration,status,lyrics,origin,checked_at FROM tracks WHERE path=?1",
            [path.as_os_str().as_encoded_bytes()],
            |row| Ok((
                TrackTags { artist: row.get(0)?, title: row.get(1)?, album: row.get(2)?, duration_secs: row.get(3)? },
                row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?, row.get::<_, i64>(7)?
            )),
        ).optional()?;
        row.map(|(tags, status, lyrics, origin, checked_at)| {
            let result = match status.as_str() {
                "found" => CachedResult::Found {
                    lyrics: lyrics.context("cached lyrics missing")?,
                    origin: match origin.as_deref() {
                        Some("generated") => Origin::Generated,
                        Some("curated") => Origin::Curated,
                        _ => bail!("invalid cached lyric origin"),
                    },
                },
                "not_found" => CachedResult::NotFound,
                "instrumental" => CachedResult::Instrumental,
                _ => bail!("invalid cached lookup status"),
            };
            Ok(TrackRecord {
                tags,
                result,
                checked_at,
            })
        })
        .transpose()
    }

    pub fn cached(&self, path: &Path, tags: &TrackTags) -> Result<Option<CachedResult>> {
        let Some(record) = self.record(path)? else {
            return Ok(None);
        };
        if record.tags != tags.normalized() {
            return Ok(None);
        }
        if record.result == CachedResult::NotFound
            && now()? - record.checked_at >= self.retry_seconds
        {
            return Ok(None);
        }
        Ok(Some(record.result))
    }

    pub fn sidecar_record(&self, path: &Path) -> Result<Option<SidecarRecord>> {
        let conn = self.conn.lock().expect("cache lock poisoned");
        let row = conn
            .query_row(
                "SELECT artist,title,album,duration,lyrics,origin FROM sidecars WHERE path=?1",
                [path.as_os_str().as_encoded_bytes()],
                |row| {
                    Ok((
                        TrackTags {
                            artist: row.get(0)?,
                            title: row.get(1)?,
                            album: row.get(2)?,
                            duration_secs: row.get(3)?,
                        },
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(tags, lyrics, origin)| {
            Ok(SidecarRecord {
                tags,
                lyrics,
                origin: match origin.as_str() {
                    "generated" => Origin::Generated,
                    "curated" => Origin::Curated,
                    _ => bail!("invalid cached lyric origin"),
                },
            })
        })
        .transpose()
    }

    pub fn find_cached_lyrics(&self, tags: &TrackTags) -> Result<Option<String>> {
        if tags.duration_secs <= 0 {
            return Ok(None);
        }
        let tags = tags.normalized();
        let conn = self.conn.lock().expect("cache lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT lyrics FROM tracks
             WHERE artist=?1 AND title=?2 AND album=?3 AND duration=?4 AND status='found'
             GROUP BY lyrics LIMIT 2",
        )?;
        let candidates = stmt
            .query_map(
                params![tags.artist, tags.title, tags.album, tags.duration_secs],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if candidates.len() == 1 {
            candidates.into_iter().next()
        } else {
            None
        })
    }

    pub fn store(&self, path: &Path, tags: &TrackTags, result: &CachedResult) -> Result<()> {
        let tags = tags.normalized();
        let (status, lyrics, origin) = match result {
            CachedResult::Found { lyrics, origin } => {
                if lyrics.trim().is_empty() {
                    bail!("refusing to cache empty lyrics");
                }
                (
                    "found",
                    Some(lyrics.as_str()),
                    Some(match origin {
                        Origin::Generated => "generated",
                        Origin::Curated => "curated",
                    }),
                )
            }
            CachedResult::NotFound => ("not_found", None, None),
            CachedResult::Instrumental => ("instrumental", None, None),
        };
        let mut conn = self.conn.lock().expect("cache lock poisoned");
        let transaction = conn.transaction()?;
        transaction.execute(
            "INSERT INTO tracks(path,artist,title,album,duration,status,lyrics,origin,checked_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(path) DO UPDATE SET artist=excluded.artist,title=excluded.title,album=excluded.album,
                 duration=excluded.duration,status=excluded.status,lyrics=excluded.lyrics,origin=excluded.origin,checked_at=excluded.checked_at",
            params![path.as_os_str().as_encoded_bytes(), tags.artist, tags.title, tags.album, tags.duration_secs, status, lyrics, origin, now()?],
        )?;
        if let (Some(lyrics), Some(origin)) = (lyrics, origin) {
            transaction.execute(
                "INSERT INTO sidecars(path,artist,title,album,duration,lyrics,origin)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(path) DO UPDATE SET artist=excluded.artist,title=excluded.title,album=excluded.album,
                     duration=excluded.duration,lyrics=excluded.lyrics,origin=excluded.origin",
                params![path.with_extension("lrc").as_os_str().as_encoded_bytes(), tags.artist, tags.title, tags.album, tags.duration_secs, lyrics, origin],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn ingest(&self, path: &Path, tags: &TrackTags, lyrics: &str) -> Result<()> {
        let normalized = tags.normalized();
        let origin = match self.sidecar_record(&path.with_extension("lrc"))? {
            Some(sidecar) if sidecar.tags == normalized && sidecar.lyrics == lyrics => {
                if self.record(path)?.is_some_and(|record| {
                    record.tags == normalized
                        && record.result
                            == CachedResult::Found {
                                lyrics: lyrics.to_owned(),
                                origin: sidecar.origin,
                            }
                }) {
                    return Ok(());
                }
                sidecar.origin
            }
            _ => Origin::Curated,
        };
        self.store(
            path,
            tags,
            &CachedResult::Found {
                lyrics: lyrics.to_owned(),
                origin,
            },
        )
    }
}

fn now() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_secs(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tags;
    use tempfile::TempDir;

    fn db() -> (TempDir, CacheDb) {
        let dir = TempDir::new().unwrap();
        let db = CacheDb::open(dir.path().join("cache.sqlite3"), 7).unwrap();
        (dir, db)
    }

    #[cfg(unix)]
    #[test]
    fn notified_writes_survive_restart_and_reject_late_snapshot_commits() {
        let (directory, cache) = db();
        let file = directory.path().join("song.flac");
        fs::write(&file, "metadata fixture").unwrap();
        let stamp = FileStamp::read(&file).unwrap();
        cache.store_tags(&file, &stamp, 0, &tags()).unwrap();
        cache
            .store_lyric_file(&file, &stamp, 0, "manual lyrics")
            .unwrap();
        cache.invalidate_file(&file).unwrap();
        drop(cache);
        let cache = CacheDb::open(directory.path().join("cache.sqlite3"), 7).unwrap();
        let version = cache.file_version(&file).unwrap();
        assert_eq!(cache.cached_tags(&file, &stamp, version).unwrap(), None);
        assert_eq!(
            cache.cached_lyric_file(&file, &stamp, version).unwrap(),
            None
        );
        cache.invalidate_file(&file).unwrap();
        // A reader that started before this write may finish afterward, using its old version.
        cache.store_tags(&file, &stamp, version, &tags()).unwrap();
        assert_eq!(
            cache
                .cached_tags(&file, &stamp, cache.file_version(&file).unwrap())
                .unwrap(),
            None
        );
        let unrelated = directory.path().join("unrelated.flac");
        cache.store_tags(&unrelated, &stamp, 0, &tags()).unwrap();
        assert!(cache.cached_tags(&unrelated, &stamp, 0).unwrap().is_some());
        cache.invalidate_all_snapshots().unwrap();
        assert_eq!(
            cache
                .cached_tags(&unrelated, &stamp, cache.file_version(&unrelated).unwrap())
                .unwrap(),
            None
        );
    }

    #[test]
    fn correcting_metadata_invalidates_a_miss_immediately() {
        let (_dir, db) = db();
        let path = Path::new("song.flac");
        let mut tags = tags();
        db.store(path, &tags, &CachedResult::NotFound).unwrap();
        assert_eq!(
            db.cached(path, &tags).unwrap(),
            Some(CachedResult::NotFound)
        );
        tags.artist = "Corrected Artist".into();
        assert_eq!(db.cached(path, &tags).unwrap(), None);
    }

    #[test]
    fn expiration_is_checked_on_every_lookup() {
        let (_dir, db) = db();
        let path = Path::new("song.flac");
        db.store(path, &tags(), &CachedResult::NotFound).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute("UPDATE tracks SET checked_at=0", [])
            .unwrap();
        assert_eq!(db.cached(path, &tags()).unwrap(), None);
    }

    #[test]
    fn reuse_requires_exact_recording_identity_and_unique_content() {
        let (_dir, db) = db();
        let first = tags();
        db.store(
            Path::new("a.flac"),
            &first,
            &CachedResult::Found {
                lyrics: "first lyrics".into(),
                origin: Origin::Generated,
            },
        )
        .unwrap();
        let mut other = first.clone();
        other.album = "Another Album".into();
        assert_eq!(db.find_cached_lyrics(&other).unwrap(), None);
        other = first.clone();
        other.duration_secs += 1;
        assert_eq!(db.find_cached_lyrics(&other).unwrap(), None);
        db.store(
            Path::new("b.flac"),
            &first,
            &CachedResult::Found {
                lyrics: "different lyrics".into(),
                origin: Origin::Generated,
            },
        )
        .unwrap();
        assert_eq!(db.find_cached_lyrics(&first).unwrap(), None);
    }

    #[test]
    fn unknown_duration_cannot_borrow_another_tracks_curated_lyrics() {
        let (_dir, db) = db();
        let mut track = tags();
        track.duration_secs = 0;
        db.store(
            Path::new("a.flac"),
            &track,
            &CachedResult::Found {
                lyrics: "curated original".into(),
                origin: Origin::Curated,
            },
        )
        .unwrap();
        assert_eq!(db.find_cached_lyrics(&track).unwrap(), None);
        assert!(matches!(
            db.cached(Path::new("a.flac"), &track).unwrap(),
            Some(CachedResult::Found { .. })
        ));
    }

    #[test]
    fn identical_ingestion_leaves_the_database_untouched() {
        let (_dir, db) = db();
        let path = Path::new("song.flac");
        db.ingest(path, &tags(), "curated lyrics").unwrap();
        let before = db.conn.lock().unwrap().total_changes();
        db.ingest(path, &tags(), "curated lyrics").unwrap();
        assert_eq!(db.conn.lock().unwrap().total_changes(), before);
        db.ingest(path, &tags(), "edited lyrics").unwrap();
        assert!(db.conn.lock().unwrap().total_changes() > before);
        assert_eq!(
            db.cached(path, &tags()).unwrap(),
            Some(CachedResult::Found {
                lyrics: "edited lyrics".into(),
                origin: Origin::Curated
            })
        );
    }

    #[test]
    fn edited_sidecar_replaces_cached_content_and_survives_reopening() {
        let (dir, db) = db();
        let path = Path::new("song.flac");
        db.store(
            path,
            &tags(),
            &CachedResult::Found {
                lyrics: "old lyrics".into(),
                origin: Origin::Generated,
            },
        )
        .unwrap();
        db.ingest(path, &tags(), "corrected lyrics").unwrap();
        drop(db);
        let db = CacheDb::open(dir.path().join("cache.sqlite3"), 7).unwrap();
        assert_eq!(
            db.cached(path, &tags()).unwrap(),
            Some(CachedResult::Found {
                lyrics: "corrected lyrics".into(),
                origin: Origin::Curated
            })
        );
    }

    #[test]
    fn sidecar_commit_failure_rolls_back_the_track_lookup() {
        let (_dir, db) = db();
        db.conn.lock().unwrap().execute_batch(
            "CREATE TRIGGER fail_sidecar BEFORE INSERT ON sidecars BEGIN SELECT RAISE(ABORT,'fixture provenance failure'); END;"
        ).unwrap();
        let path = Path::new("song.flac");
        assert!(
            db.store(
                path,
                &tags(),
                &CachedResult::Found {
                    lyrics: "uncommitted lyrics".into(),
                    origin: Origin::Generated,
                }
            )
            .is_err()
        );
        assert!(db.record(path).unwrap().is_none());
        assert!(
            db.sidecar_record(&path.with_extension("lrc"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unicode_identity_and_literal_path_separators_do_not_alias() {
        let (_dir, db) = db();
        let mut tags = tags();
        tags.artist = "Артист".into();
        db.store(Path::new("a\\b.flac"), &tags, &CachedResult::NotFound)
            .unwrap();
        tags.artist = "АРТИСТ".into();
        assert_eq!(
            db.cached(Path::new("a\\b.flac"), &tags).unwrap(),
            Some(CachedResult::NotFound)
        );
        assert_eq!(db.cached(Path::new("a/b.flac"), &tags).unwrap(), None);
    }
}
