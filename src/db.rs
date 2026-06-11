use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tracing::info;

use crate::tags::TrackTags;

#[derive(Clone)]
pub struct CacheDb {
    db_path: Arc<PathBuf>,
    conn: Arc<Mutex<Connection>>,
    not_found: Arc<RwLock<HashSet<String>>>,
    success: Arc<RwLock<HashSet<String>>>,
    retry_not_found_days: u64,
}

impl CacheDb {
    pub fn open(path: PathBuf, retry_not_found_days: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS track_cache (
                path TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                artist TEXT,
                title TEXT,
                album TEXT,
                duration INTEGER,
                lyrics TEXT,
                updated_at REAL NOT NULL
            );
            "#,
        )?;

        let mut stmt = conn.prepare("PRAGMA table_info(track_cache)")?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for (name, ddl) in [
            ("artist", "ALTER TABLE track_cache ADD COLUMN artist TEXT"),
            ("title", "ALTER TABLE track_cache ADD COLUMN title TEXT"),
            ("album", "ALTER TABLE track_cache ADD COLUMN album TEXT"),
            (
                "duration",
                "ALTER TABLE track_cache ADD COLUMN duration INTEGER",
            ),
            ("lyrics", "ALTER TABLE track_cache ADD COLUMN lyrics TEXT"),
        ] {
            if !columns.contains(name) {
                info!("adding missing track_cache column {name}");
                conn.execute(ddl, [])?;
            }
        }
        drop(stmt);

        Ok(Self {
            db_path: Arc::new(path),
            conn: Arc::new(Mutex::new(conn)),
            not_found: Arc::new(RwLock::new(HashSet::new())),
            success: Arc::new(RwLock::new(HashSet::new())),
            retry_not_found_days,
        })
    }

    pub fn migrate_legacy_json(&self) -> Result<()> {
        let Some(config_dir) = self.db_path.parent() else {
            return Ok(());
        };
        let old_path = config_dir.join("cache.json");
        if !old_path.exists() {
            return Ok(());
        }

        let paths: Vec<String> = serde_json::from_slice(&fs::read(&old_path)?)?;
        let now = unix_now();
        let conn = self.conn.lock().expect("db lock poisoned");
        for path in paths {
            conn.execute(
                "INSERT OR IGNORE INTO track_cache (path, status, updated_at) VALUES (?1, 'not_found', ?2)",
                params![path, now],
            )?;
        }
        fs::remove_file(&old_path).with_context(|| format!("removing {}", old_path.display()))?;
        info!("migrated legacy cache.json to SQLite");
        Ok(())
    }

    pub fn load_memory_cache(&self) -> Result<()> {
        let retry_threshold = unix_now() - (self.retry_not_found_days as f64 * 86_400.0);
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare("SELECT path, status, updated_at FROM track_cache")?;
        let mut rows = stmt.query([])?;

        let mut not_found = HashSet::new();
        let mut success = HashSet::new();
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let status: String = row.get(1)?;
            let updated_at: f64 = row.get(2)?;
            match status.as_str() {
                "success" => {
                    success.insert(path);
                }
                "not_found" if updated_at >= retry_threshold => {
                    not_found.insert(path);
                }
                _ => {}
            }
        }

        *self.not_found.write().expect("not_found cache poisoned") = not_found;
        *self.success.write().expect("success cache poisoned") = success;
        Ok(())
    }

    pub fn is_not_found(&self, rel_path: &str) -> bool {
        self.not_found
            .read()
            .expect("not_found cache poisoned")
            .contains(rel_path)
    }

    pub fn is_success(&self, rel_path: &str) -> bool {
        self.success
            .read()
            .expect("success cache poisoned")
            .contains(rel_path)
    }

    pub fn mark_success(
        &self,
        rel_path: &str,
        tags: Option<&TrackTags>,
        lyrics: Option<&str>,
    ) -> Result<()> {
        let empty = TrackTags::default();
        let tags = tags.unwrap_or(&empty);
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.execute(
            r#"
            INSERT OR REPLACE INTO track_cache
                (path, status, artist, title, album, duration, lyrics, updated_at)
            VALUES (?1, 'success', ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                rel_path,
                opt_nonempty(&tags.artist),
                opt_nonempty(&tags.title),
                opt_nonempty(&tags.album),
                tags.duration_secs,
                lyrics,
                unix_now()
            ],
        )?;
        self.success
            .write()
            .expect("success cache poisoned")
            .insert(rel_path.to_string());
        self.not_found
            .write()
            .expect("not_found cache poisoned")
            .remove(rel_path);
        Ok(())
    }

    pub fn mark_not_found(&self, rel_path: &str, tags: &TrackTags) -> Result<()> {
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.execute(
            r#"
            INSERT OR REPLACE INTO track_cache
                (path, status, artist, title, album, duration, lyrics, updated_at)
            VALUES (?1, 'not_found', ?2, ?3, ?4, ?5, NULL, ?6)
            "#,
            params![
                rel_path,
                opt_nonempty(&tags.artist),
                opt_nonempty(&tags.title),
                opt_nonempty(&tags.album),
                tags.duration_secs,
                unix_now()
            ],
        )?;
        self.not_found
            .write()
            .expect("not_found cache poisoned")
            .insert(rel_path.to_string());
        self.success
            .write()
            .expect("success cache poisoned")
            .remove(rel_path);
        Ok(())
    }

    pub fn find_cached_lyrics(&self, tags: &TrackTags) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT lyrics FROM track_cache
            WHERE LOWER(artist) = LOWER(?1)
              AND LOWER(title) = LOWER(?2)
              AND (duration IS NULL OR duration = 0 OR abs(duration - ?3) <= 5)
              AND lyrics IS NOT NULL
            LIMIT 1
            "#,
        )?;
        let mut rows = stmt.query(params![tags.artist, tags.title, tags.duration_secs])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }
}

fn opt_nonempty(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs_f64()
}
