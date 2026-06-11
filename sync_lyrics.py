import os
import sys
import json
import time
import sqlite3
import requests
import threading
import re
from concurrent.futures import ThreadPoolExecutor
from mutagen import File as MutagenFile
from watchdog.observers import Observer
from watchdog.events import FileSystemEventHandler

MUSIC_DIR = "/music"
DB_FILE = "/config/lyrics.db"

# Get concurrency from environment variable, default to 8
env_concurrency = os.environ.get("LRCGET_CONCURRENCY")
CONCURRENCY = int(env_concurrency) if env_concurrency and env_concurrency.strip() else 8

# Clean fallback configuration
env_clean_fallback = os.environ.get("LRCGET_CLEAN_FALLBACK")
CLEAN_FALLBACK = env_clean_fallback.lower() in ('true', '1', 'yes') if env_clean_fallback else True

# Retry period for not_found tracks (in days), default to 7
env_retry_days = os.environ.get("LRCGET_RETRY_NOT_FOUND_DAYS")
RETRY_NOT_FOUND_DAYS = int(env_retry_days) if env_retry_days and env_retry_days.strip() else 7

# Global states and thread-safety locks
db_lock = threading.Lock()
not_found_cache = set()
success_cache = set()

active_tracks = set()
active_tracks_lock = threading.Lock()

def init_db():
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS track_cache (
                    path TEXT PRIMARY KEY,
                    status TEXT NOT NULL,
                    artist TEXT,
                    title TEXT,
                    album TEXT,
                    duration INTEGER,
                    lyrics TEXT,
                    updated_at REAL NOT NULL
                )
            """)
            
            # Check schema and run migrations if columns are missing
            cursor = conn.cursor()
            cursor.execute("PRAGMA table_info(track_cache)")
            columns = {row[1] for row in cursor.fetchall()}
            if 'lyrics' not in columns:
                print("[INFO] Upgrading track_cache schema to include lyrics and metadata columns...")
                conn.execute("ALTER TABLE track_cache ADD COLUMN artist TEXT")
                conn.execute("ALTER TABLE track_cache ADD COLUMN title TEXT")
                conn.execute("ALTER TABLE track_cache ADD COLUMN album TEXT")
                conn.execute("ALTER TABLE track_cache ADD COLUMN duration INTEGER")
                conn.execute("ALTER TABLE track_cache ADD COLUMN lyrics TEXT")
            conn.commit()
        finally:
            conn.close()

def migrate_old_cache():
    old_cache_file = "/config/cache.json"
    if os.path.exists(old_cache_file):
        print("[INFO] Migrating old cache.json to SQLite database...")
        try:
            with open(old_cache_file, 'r') as f:
                paths = json.load(f)
            
            with db_lock:
                conn = sqlite3.connect(DB_FILE)
                try:
                    conn.executemany(
                        "INSERT OR IGNORE INTO track_cache (path, status, updated_at) VALUES (?, 'not_found', ?)",
                        [(p, time.time()) for p in paths]
                    )
                    conn.commit()
                finally:
                    conn.close()
            
            os.remove(old_cache_file)
            print("[INFO] Migration complete. Deleted old cache.json")
        except Exception as e:
            print(f"[WARN] Migration failed: {e}")

def load_cache_to_memory():
    global not_found_cache, success_cache
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            cursor = conn.cursor()
            cursor.execute("SELECT path, status, updated_at FROM track_cache")
            rows = cursor.fetchall()
            
            retry_threshold = time.time() - (RETRY_NOT_FOUND_DAYS * 86400)
            not_found_cache = {r[0] for r in rows if r[1] == 'not_found' and (r[2] is None or r[2] >= retry_threshold)}
            success_cache = {r[0] for r in rows if r[1] == 'success'}
        finally:
            conn.close()

def repair_db_cache():
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            cursor = conn.cursor()
            cursor.execute("SELECT path FROM track_cache WHERE status = 'success' AND (lyrics IS NULL OR artist IS NULL)")
            rows = cursor.fetchall()
            if rows:
                print(f"[INFO] Found {len(rows)} database records missing lyrics or metadata. Repairing...")
                for (rel_path,) in rows:
                    audio_path = os.path.join(MUSIC_DIR, rel_path)
                    lrc_path = os.path.splitext(audio_path)[0] + ".lrc"
                    if os.path.exists(audio_path) and os.path.exists(lrc_path):
                        try:
                            with open(lrc_path, 'r', encoding='utf-8') as f:
                                lyrics = f.read()
                            tags = get_audio_tags(audio_path)
                            artist = tags['artist'] if tags else None
                            title = tags['title'] if tags else None
                            album = tags['album'] if tags else None
                            duration = tags['duration'] if tags else 0
                            
                            conn.execute(
                                """UPDATE track_cache 
                                   SET artist = ?, title = ?, album = ?, duration = ?, lyrics = ?, updated_at = ? 
                                   WHERE path = ?""",
                                (artist, title, album, duration, lyrics, time.time(), rel_path)
                            )
                        except Exception:
                            pass
                conn.commit()
                print("[INFO] Database repair complete.")
        finally:
            conn.close()

def db_mark_success(rel_path, artist, title, album, duration, lyrics):
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            conn.execute(
                """INSERT OR REPLACE INTO track_cache 
                   (path, status, artist, title, album, duration, lyrics, updated_at) 
                   VALUES (?, 'success', ?, ?, ?, ?, ?, ?)""",
                (rel_path, artist, title, album, duration, lyrics, time.time())
            )
            conn.commit()
            success_cache.add(rel_path)
            not_found_cache.discard(rel_path)
        finally:
            conn.close()

def db_mark_not_found(rel_path, artist=None, title=None, album=None, duration=None):
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            conn.execute(
                """INSERT OR REPLACE INTO track_cache 
                   (path, status, artist, title, album, duration, lyrics, updated_at) 
                   VALUES (?, 'not_found', ?, ?, ?, ?, NULL, ?)""",
                (rel_path, artist, title, album, duration, time.time())
            )
            conn.commit()
            not_found_cache.add(rel_path)
            success_cache.discard(rel_path)
        finally:
            conn.close()

def find_cached_lyrics(artist, title, duration):
    with db_lock:
        conn = sqlite3.connect(DB_FILE)
        try:
            cursor = conn.cursor()
            # Query for any success record with the same artist and title.
            # Filters with a tolerance of 5 seconds on duration.
            cursor.execute("""
                SELECT lyrics FROM track_cache 
                WHERE LOWER(artist) = LOWER(?) AND LOWER(title) = LOWER(?) 
                  AND (duration IS NULL OR duration = 0 OR abs(duration - ?) <= 5)
                  AND lyrics IS NOT NULL 
                LIMIT 1
            """, (artist, title, duration))
            row = cursor.fetchone()
            return row[0] if row else None
        finally:
            conn.close()

def get_audio_tags(file_path):
    try:
        audio = MutagenFile(file_path)
        if not audio:
            return None
        
        duration = int(audio.info.length) if hasattr(audio.info, 'length') else 0
        
        title, artist, album = None, None, None
        tags = audio.tags
        if not tags:
            return None
        
        def clean_tag(val):
            if isinstance(val, (list, tuple)):
                return str(val[0]) if val else None
            if hasattr(val, 'text'):  # ID3 frame
                return str(val.text[0]) if val.text else None
            return str(val) if val is not None else None

        if hasattr(tags, 'getall') or 'TIT2' in tags:  # ID3 (MP3)
            title = clean_tag(tags.get('TIT2'))
            artist = clean_tag(tags.get('TPE1')) or clean_tag(tags.get('TPE2'))
            album = clean_tag(tags.get('TALB'))
        else:  # FLAC, MP4, etc.
            tag_dict = {k.lower(): v for k, v in tags.items()}
            title = clean_tag(tag_dict.get('title') or tag_dict.get('\xa9nam') or tag_dict.get('tit2'))
            artist = clean_tag(tag_dict.get('artist') or tag_dict.get('\xa9art') or tag_dict.get('tpe1') or tag_dict.get('albumartist') or tag_dict.get('tpe2'))
            album = clean_tag(tag_dict.get('album') or tag_dict.get('\xa9alb') or tag_dict.get('talb'))
            
        if title and artist:
            return {
                'title': title.strip(),
                'artist': artist.strip(),
                'album': album.strip() if album else '',
                'duration': duration
            }
    except Exception as e:
        pass
    return None

def fetch_lyrics(artist, title, album, duration):
    url = "https://lrclib.net/api/get"
    headers = {
        "User-Agent": "lrcget-cli/1.0 (contact: codeberg.org/lutyjj/lrcget-cli)"
    }
    params = {
        "artist_name": artist,
        "track_name": title,
    }
    if album:
        params["album_name"] = album
    if duration > 0:
        params["duration"] = duration
        
    try:
        response = requests.get(url, headers=headers, params=params, timeout=10)
        if response.status_code == 200:
            data = response.json()
            return data.get('syncedLyrics') or data.get('plainLyrics')
        elif response.status_code == 404:
            return False  # Not found
    except Exception as e:
        print(f"[WARN] API request failed for '{artist} - {title}': {e}")
    return None  # Temporary failure / timeout

def get_track_number(filename):
    m = re.match(r'^(\d+)', filename)
    return int(m.group(1)) if m else None

def clean_metadata_value(val):
    if not val:
        return val
    # Pattern to match trailing parentheses/brackets containing remaster, mix, live, edit, version, deluxe, etc.
    pattern = r'(?i)\s*[\(\[\{](?:[^\)\]\}]*?\b)?(?:remaster|remastered|mix|remix|live|edit|version|session|deluxe|anniversary|edition|mono|stereo|re-recorded|digitally|reissue|restored)\b[^\)\]\}]*?[\)\]\}]\s*$'
    return re.sub(pattern, '', val).strip()

def clean_name(filename):
    name = os.path.splitext(filename)[0]
    # Remove leading track numbers and punctuation (e.g. '01 - ' or '01. ' or '01 ')
    name = re.sub(r'^\d+\s*[-._]?\s*', '', name)
    # Remove common trailing suffixes like (live), [live], feat. etc.
    name = re.sub(r'[\(\[\{].*?[\)\]\}]', '', name)
    name = re.sub(r'(?i)\b(feat|ft)\b.*', '', name)
    # Strip non-alphanumeric and lowercase
    name = re.sub(r'[\W_]+', '', name).lower()
    return name

def process_track(audio_path, rel_path, lrc_path, is_watch=False):
    # Prevent duplicate runs on the same track concurrently
    with active_tracks_lock:
        if audio_path in active_tracks:
            return
        active_tracks.add(audio_path)

    try:
        tags = get_audio_tags(audio_path)
        if not tags:
            if is_watch:
                # File might still be writing initially; retry in a few seconds.
                time.sleep(3)
                tags = get_audio_tags(audio_path)
            if not tags:
                return

        # Check if LRC was created while waiting/queuing
        if os.path.exists(lrc_path):
            is_cached = False
            with db_lock:
                is_cached = rel_path in success_cache
            if not is_cached:
                lyrics = None
                try:
                    with open(lrc_path, 'r', encoding='utf-8') as f:
                        lyrics = f.read()
                except Exception:
                    pass
                db_mark_success(rel_path, tags['artist'], tags['title'], tags['album'], tags['duration'], lyrics)
            return

        prefix = "[WATCH]" if is_watch else "[SCAN]"
        
        # 1. Try to find the lyric in our local SQLite database first
        cached_lyrics = find_cached_lyrics(tags['artist'], tags['title'], tags['duration'])
        if cached_lyrics:
            try:
                with open(lrc_path, 'w', encoding='utf-8') as f:
                    f.write(cached_lyrics)
                print(f"[LOCAL_CACHE] Restored lyrics for: {tags['artist']} - {tags['title']}")
                db_mark_success(rel_path, tags['artist'], tags['title'], tags['album'], tags['duration'], cached_lyrics)
                return
            except Exception as e:
                print(f"[ERROR] Writing LRC file from cache for {tags['artist']} - {tags['title']}: {e}")

        # 2. If not cached, fetch from LRCLib API
        print(f"{prefix} Fetching from API: {tags['artist']} - {tags['title']}")
        lyrics = fetch_lyrics(tags['artist'], tags['title'], tags['album'], tags['duration'])
        
        # Tier 2 Clean Fallback if strict lookup (Tier 1) failed (returned False/404)
        if lyrics is False and CLEAN_FALLBACK:
            cleaned_title = clean_metadata_value(tags['title'])
            cleaned_album = clean_metadata_value(tags['album'])
            if cleaned_title != tags['title'] or cleaned_album != tags['album']:
                print(f"[CLEAN_FALLBACK] Retrying lookup with cleaned tags: {tags['artist']} - {cleaned_title}")
                lyrics = fetch_lyrics(tags['artist'], cleaned_title, cleaned_album, tags['duration'])
        
        if lyrics:
            try:
                with open(lrc_path, 'w', encoding='utf-8') as f:
                    f.write(lyrics)
                print(f"[SUCCESS] Saved lyrics: {tags['artist']} - {tags['title']}")
                db_mark_success(rel_path, tags['artist'], tags['title'], tags['album'], tags['duration'], lyrics)
            except Exception as e:
                print(f"[ERROR] Writing LRC file for {tags['artist']} - {tags['title']}: {e}")
        elif lyrics is False:
            print(f"[INFO] Lyrics not found: {tags['artist']} - {tags['title']}")
            db_mark_not_found(rel_path, tags['artist'], tags['title'], tags['album'], tags['duration'])
        else:
            print(f"[WARN] Failed lookup (will retry): {tags['artist']} - {tags['title']}")
            
        # Polite API delay
        time.sleep(0.5)
    finally:
        with active_tracks_lock:
            active_tracks.discard(audio_path)

def sync(executor):
    print(f"[INFO] Starting lyrics synchronization scan...")
    # Refresh cache in memory
    load_cache_to_memory()
    
    supported_extensions = ('.mp3', '.flac', '.m4a', '.ogg', '.opus')
    
    tracks_to_process = []
    skipped_not_found = 0
    skipped_success = 0
    reconciled_count = 0
    cleaned_count = 0
    
    # First, gather all tracks that need processing
    for root, _, files in os.walk(MUSIC_DIR):
        audio_files = []
        lrc_files = set()
        
        # Categorize files in the current folder
        for file in files:
            if file.lower().endswith(supported_extensions):
                audio_files.append(file)
            elif file.lower().endswith('.lrc'):
                lrc_files.add(file)
                
        if not audio_files and not lrc_files:
            continue
            
        missing_audio = []
        
        # 1. Match exact pairs first
        for audio_file in audio_files:
            audio_path = os.path.join(root, audio_file)
            lrc_file = os.path.splitext(audio_file)[0] + ".lrc"
            rel_path = os.path.relpath(audio_path, MUSIC_DIR)
            
            if lrc_file in lrc_files:
                lrc_files.remove(lrc_file)  # Not an orphan
                is_cached = False
                with db_lock:
                    is_cached = rel_path in success_cache
                if not is_cached:
                    lyrics = None
                    lrc_path = os.path.join(root, lrc_file)
                    try:
                        with open(lrc_path, 'r', encoding='utf-8') as f:
                            lyrics = f.read()
                    except Exception:
                        pass
                    tags = get_audio_tags(audio_path)
                    artist = tags['artist'] if tags else None
                    title = tags['title'] if tags else None
                    album = tags['album'] if tags else None
                    duration = tags['duration'] if tags else 0
                    db_mark_success(rel_path, artist, title, album, duration, lyrics)
                skipped_success += 1
            else:
                missing_audio.append(audio_file)
                
        # 2. Try to reconcile missing audio files with orphaned .lrc files in the same folder
        if missing_audio and lrc_files:
            still_missing = []
            for audio_file in missing_audio:
                audio_path = os.path.join(root, audio_file)
                rel_path = os.path.relpath(audio_path, MUSIC_DIR)
                dest_lrc_file = os.path.splitext(audio_file)[0] + ".lrc"
                dest_lrc_path = os.path.join(root, dest_lrc_file)
                
                audio_track_num = get_track_number(audio_file)
                audio_clean = clean_name(audio_file)
                
                matched_lrc_file = None
                
                # A. Try matching by track number
                if audio_track_num is not None:
                    for lrc in lrc_files:
                        if get_track_number(lrc) == audio_track_num:
                            matched_lrc_file = lrc
                            break
                            
                # B. Try matching by clean name similarity
                if not matched_lrc_file:
                    for lrc in lrc_files:
                        if clean_name(lrc) == audio_clean:
                            matched_lrc_file = lrc
                            break
                            
                if matched_lrc_file:
                    lrc_files.remove(matched_lrc_file)
                    src_lrc_path = os.path.join(root, matched_lrc_file)
                    try:
                        os.rename(src_lrc_path, dest_lrc_path)
                        print(f"[RECONCILE] Paired orphaned '{matched_lrc_file}' to '{dest_lrc_file}'")
                        reconciled_count += 1
                        
                        # Read the lyric content and record as success
                        lyrics = None
                        try:
                            with open(dest_lrc_path, 'r', encoding='utf-8') as f:
                                lyrics = f.read()
                        except Exception:
                            pass
                        tags = get_audio_tags(audio_path)
                        artist = tags['artist'] if tags else None
                        title = tags['title'] if tags else None
                        album = tags['album'] if tags else None
                        duration = tags['duration'] if tags else 0
                        db_mark_success(rel_path, artist, title, album, duration, lyrics)
                        skipped_success += 1
                    except Exception as e:
                        print(f"[ERROR] Failed renaming reconciled file: {e}")
                        still_missing.append(audio_file)
                else:
                    still_missing.append(audio_file)
            missing_audio = still_missing
            
        # 3. Clean up remaining true orphaned .lrc files
        if lrc_files:
            for orphan in lrc_files:
                orphan_path = os.path.join(root, orphan)
                try:
                    os.remove(orphan_path)
                    print(f"[CLEANUP] Deleted orphaned lyric file: {orphan_path}")
                    cleaned_count += 1
                except Exception as e:
                    print(f"[ERROR] Failed deleting orphaned file: {e}")
                    
        # 4. Add remaining missing audio files to process queue
        for audio_file in missing_audio:
            audio_path = os.path.join(root, audio_file)
            rel_path = os.path.relpath(audio_path, MUSIC_DIR)
            lrc_file = os.path.splitext(audio_file)[0] + ".lrc"
            lrc_path = os.path.join(root, lrc_file)
            
            with db_lock:
                if rel_path in not_found_cache:
                    skipped_not_found += 1
                    continue
                    
            tracks_to_process.append((audio_path, rel_path, lrc_path))
            
    total_tracks = len(tracks_to_process)
    print(f"[INFO] Found {total_tracks} tracks requiring lookup. Skipped {skipped_not_found} cached 404s, {skipped_success} completed. Reconciled {reconciled_count} orphans, cleaned {cleaned_count} obsolete files.")
    
    if total_tracks == 0:
        return
        
    futures = [executor.submit(process_track, item[0], item[1], item[2], False) for item in tracks_to_process]
    for f in futures:
        try:
            f.result()
        except Exception as e:
            print(f"[ERROR] Task execution error: {e}")
            
    print(f"[INFO] Scan complete.")

class MusicWatchdogHandler(FileSystemEventHandler):
    def __init__(self, executor):
        self.executor = executor
        self.supported_extensions = ('.mp3', '.flac', '.m4a', '.ogg', '.opus')

    def on_created(self, event):
        if not event.is_directory:
            self.handle_path(event.src_path)

    def on_modified(self, event):
        if not event.is_directory:
            self.handle_path(event.src_path)

    def on_moved(self, event):
        if not event.is_directory:
            self.handle_move(event.src_path, event.dest_path)

    def handle_path(self, path):
        if path.lower().endswith(self.supported_extensions):
            lrc_path = os.path.splitext(path)[0] + ".lrc"
            rel_path = os.path.relpath(path, MUSIC_DIR)
            with db_lock:
                if rel_path in not_found_cache:
                    return
            if not os.path.exists(lrc_path):
                self.executor.submit(process_track, path, rel_path, lrc_path, True)

    def handle_move(self, src_path, dest_path):
        if dest_path.lower().endswith(self.supported_extensions):
            src_rel = os.path.relpath(src_path, MUSIC_DIR)
            dest_rel = os.path.relpath(dest_path, MUSIC_DIR)
            
            src_lrc = os.path.splitext(src_path)[0] + ".lrc"
            dest_lrc = os.path.splitext(dest_path)[0] + ".lrc"
            
            # If the old LRC file exists, rename it to the new destination path
            if os.path.exists(src_lrc):
                try:
                    os.rename(src_lrc, dest_lrc)
                    print(f"[WATCH] Renamed companion LRC file from '{src_lrc}' to '{dest_lrc}'")
                    
                    # Update database entry
                    with db_lock:
                        conn = sqlite3.connect(DB_FILE)
                        try:
                            conn.execute(
                                "UPDATE track_cache SET path = ?, updated_at = ? WHERE path = ?",
                                (dest_rel, time.time(), src_rel)
                            )
                            conn.commit()
                            
                            # Update in-memory caches
                            if src_rel in success_cache:
                                success_cache.add(dest_rel)
                                success_cache.discard(src_rel)
                            if src_rel in not_found_cache:
                                not_found_cache.add(dest_rel)
                                not_found_cache.discard(src_rel)
                        finally:
                            conn.close()
                    return
                except Exception as e:
                    print(f"[ERROR] Failed to rename companion LRC file: {e}")
            
            # If the old LRC file didn't exist, treat it as a new path (will search DB cache first)
            self.handle_path(dest_path)

if __name__ == "__main__":
    sys.stdout.reconfigure(line_buffering=True)
    sys.stderr.reconfigure(line_buffering=True)
    
    # Initialize SQLite database and run migration if old JSON exists
    init_db()
    migrate_old_cache()
    repair_db_cache()
    load_cache_to_memory()
        
    print(f"[INFO] Initializing shared ThreadPoolExecutor with {CONCURRENCY} workers.")
    executor = ThreadPoolExecutor(max_workers=CONCURRENCY)
    
    # Start the watchdog observer first to capture any imports happening during initial sync
    print("[INFO] Starting filesystem watchdog observer for /music...")
    event_handler = MusicWatchdogHandler(executor)
    observer = Observer()
    observer.schedule(event_handler, MUSIC_DIR, recursive=True)
    observer.start()
    
    # Run initial sync on startup to check for any drift or missing files
    try:
        sync(executor)
    except Exception as e:
        print(f"[ERROR] Initial sync failed: {e}")
    
    # Fallback scan every 12 hours
    FALLBACK_INTERVAL = 43200
    
    try:
        while True:
            time.sleep(FALLBACK_INTERVAL)
            print("[INFO] Running scheduled fallback sync scan...")
            try:
                sync(executor)
            except Exception as e:
                print(f"[ERROR] Scheduled fallback sync encountered error: {e}")
    except KeyboardInterrupt:
        print("[INFO] Stopping observer...")
    finally:
        observer.stop()
        observer.join()
        executor.shutdown(wait=True)
        print("[INFO] Exited cleanly.")
