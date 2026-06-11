# lrcget-cli

Small containerized lyrics sync worker for a music library.

It scans `/music`, fetches lyrics from LRCLib, writes sidecar `.lrc` files next to audio files, and keeps lookup/cache state in `/config/lyrics.db`.

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
| `LRCGET_CONCURRENCY` | `8` | Number of worker threads used during scans. |
| `LRCGET_CLEAN_FALLBACK` | `true` | Retry lookups with cleaned title/album metadata. |
| `LRCGET_RETRY_NOT_FOUND_DAYS` | `7` | Days to wait before retrying tracks previously not found. |
| `LRCGET_CONFIG_DIR` | `./config` | Host path mounted as `/config`. |
| `LRCGET_MUSIC_DIR` | `./music` | Host path mounted as `/music`. |

## Checks

```sh
make check
```

