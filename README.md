# lrc-sync

Containerized lyrics sync worker for a music library.

It scans `/music`, fetches lyrics from LRCLib, writes sidecar `.lrc` files next to audio files, watches for new files, and keeps lookup/cache state in `/config/lyrics.db`.

## Run

```sh
docker compose up -d --build
```

By default the compose file uses local `./config` and `./music` folders. Override paths with environment variables:

```sh
LRCSYNC_CONFIG_DIR=/path/to/config \
LRCSYNC_MUSIC_DIR=/path/to/music \
docker compose up -d --build
```

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `PUID` | `1000` | Container user id. |
| `PGID` | `1000` | Container group id. |
| `TZ` | `UTC` | Timezone. |
| `LRCSYNC_CONCURRENCY` | `8` | Worker threads used during scans. Must be at least `1`. |
| `LRCSYNC_CLEAN_FALLBACK` | `true` | Retry lookups with cleaned title/album metadata. |
| `LRCSYNC_RETRY_NOT_FOUND_DAYS` | `7` | Days to wait before retrying tracks previously not found. |
| `LRCSYNC_REQUEST_INTERVAL_MS` | `750` | Shared minimum delay between LRCLib requests across all workers. |
| `LRCSYNC_REQUEST_TIMEOUT_SECONDS` | `30` | Per-request timeout for LRCLib HTTP calls. |
| `LRCSYNC_ORPHAN_ACTION` | `keep` | What to do with unmatched `.lrc` files during scans: `keep`, `reconcile`, `quarantine`, or `delete`. |
| `LRCSYNC_FALLBACK_SCAN_SECONDS` | `43200` | Seconds between periodic full rescans (minimum `60`, maximum `604800`). Default is 12 hours. |
| `LRCSYNC_FOLLOW_SYMLINKS` | `false` | Follow symbolic links when scanning the music directory. |
| `LRCSYNC_DB_PATH` | `./config` | Host path mounted as `/config`. |
| `LRCSYNC_MUSIC_DIR` | `./music` | Host path mounted as `/music`. |

`keep` is intentionally the default orphan behavior. It avoids deleting manually curated lyrics or files that appear while a scan is running. Use `quarantine` or `delete` only after checking logs.

## Checks

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```
