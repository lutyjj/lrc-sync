# lrcget-cli

Containerized lyrics sync worker for a music library.

It scans `/music`, fetches lyrics from LRCLib, writes sidecar `.lrc` files next to audio files, watches for new files, and keeps lookup/cache state in `/config/lyrics.db`.

## Run

```sh
docker compose up -d --build
```

By default the compose file uses local `./config` and `./music` folders. Override paths with environment variables:

```sh
LRCGET_CONFIG_DIR=/path/to/config \
LRCGET_MUSIC_DIR=/path/to/music \
docker compose up -d --build
```

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `PUID` | `1000` | Container user id. |
| `PGID` | `1000` | Container group id. |
| `TZ` | `UTC` | Timezone. |
| `LRCGET_CONCURRENCY` | `8` | Worker threads used during scans. Must be at least `1`. |
| `LRCGET_CLEAN_FALLBACK` | `true` | Retry lookups with cleaned title/album metadata. |
| `LRCGET_RETRY_NOT_FOUND_DAYS` | `7` | Days to wait before retrying tracks previously not found. |
| `LRCGET_REQUEST_INTERVAL_MS` | `750` | Shared minimum delay between LRCLib requests across all workers. |
| `LRCGET_ORPHAN_ACTION` | `keep` | What to do with unmatched `.lrc` files during scans: `keep`, `reconcile`, `quarantine`, or `delete`. |
| `LRCGET_CONFIG_DIR` | `./config` | Host path mounted as `/config`. |
| `LRCGET_MUSIC_DIR` | `./music` | Host path mounted as `/music`. |

`keep` is intentionally the default orphan behavior. It avoids deleting manually curated lyrics or files that appear while a scan is running. Use `quarantine` or `delete` only after checking logs.

## Checks

```sh
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

