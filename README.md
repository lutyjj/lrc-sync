# lrc-sync

lrc-sync reads audio metadata, finds matching lyrics on [LRCLib](https://lrclib.net), and writes `.lrc` sidecars. It supports MP3, FLAC, M4A, OGG, and Opus. Existing sidecars are preserved and cached, including manual corrections.

## Run

The Compose service runs as UID/GID 1000 and mounts `./config` and `./music`. Give that user write access to both directories, then run:

```sh
docker compose up -d --build
```

To use other host directories:

```sh
LRCSYNC_CONFIG_DIR=/path/to/config \
LRCSYNC_MUSIC_VOLUME=/path/to/music \
docker compose up -d --build
```

Those two variables configure host mounts. The process inside the container reads `/music` and stores its cache at `/config/cache.sqlite3`.

For a native run with Rust 1.99 or later:

```sh
cargo build --release --locked
LRCSYNC_MUSIC_DIR=/path/to/music \
LRCSYNC_DB_PATH=/path/to/cache.sqlite3 \
./target/release/lrc-sync
```

## Matching and cache

Every response must agree with the artist, title, known album, and duration (within two seconds). Unknown durations prevent HTTP lookup and reuse from another track; curated sidecars remain available. Tracks longer than one hour use album-constrained search because LRCLib's exact lookup rejects that duration. The optional fallback removes release packaging labels such as remaster years and deluxe editions. It preserves live, remix, acoustic, and other recording variants.

If an album lookup fails, fallback searches by artist and title and requires a known duration. Tracks without an album also use search so that a single API result cannot hide conflicting recordings. Conflicting lyric results or a search that reaches LRCLib's result limit are rejected. Album relaxation is disabled for recording variants such as live albums. A blank synced result can use nonempty plain lyrics; an instrumental result is stored separately from a miss.

SQLite records the normalized artist, title, album, duration, path, and lyric content. Audio paths own lookup results; the physical sidecar path owns lyric provenance across audio format changes. These records commit together. Metadata changes invalidate path-based results immediately. Reuse across tracks requires the exact recording signature and agreement between cached lyric contents. Misses expire during lookups. HTTP failures and ambiguous matches are not cached as misses.

Scans ingest existing sidecars, so manual edits replace cached content and can be restored after deletion. Directory aliases share the same cache history. Generated sidecars, including automatic copies from another track, are archived when their audio metadata changes. Lookup results and provenance are committed before publication, so an interrupted write can be restored without treating generated lyrics as curated. Untracked sidecars are treated as curated. The cache is disposable and has no legacy-format import; resetting it never removes sidecars.

## Scheduling and shutdown

The startup scan, watcher, and periodic scan submit directories to one bounded queue. Each directory has one worker owner, including all audio formats sharing a sidecar stem. Repeated events coalesce; changes during processing request one more pass. Queue overflow, watcher overflow, and discarded stale directory routes request a repair scan. Invalid metadata in one track does not delay other tracks in its directory.

Polling is the default watcher because network filesystems may not deliver native events for writes on another machine. Native watching is available for local filesystems. Both watcher modes and scans obey the same symlink policy and ignore `.lrcsync-orphans` directories.

All workers share one HTTP owner, which serializes requests through response completion and delays the next request. HTTP 429 applies its `Retry-After` cooldown to every worker without truncating it. Logs include lookup failures and HTTP status codes.

SIGINT and SIGTERM cancel queue admission and interrupt cooldowns. Shutdown waits for the watcher and workers to finish. An active HTTP request can take up to the request timeout. Set the Compose stop grace period above that timeout; its default is 45 seconds for a 30-second request timeout.

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `PUID`, `PGID` | `1000` | Compose container user and group. |
| `TZ` | `UTC` | Compose timezone. |
| `LRCSYNC_CONFIG_DIR` | `./config` | Compose host directory mounted at `/config`. |
| `LRCSYNC_MUSIC_VOLUME` | `./music` | Compose host directory mounted at `/music`. |
| `LRCSYNC_CONTAINER_NAME` | `lrc-sync` | Compose container name. |
| `LRCSYNC_STOP_GRACE_PERIOD` | `45s` | Compose shutdown grace period. Increase with the request timeout. |
| `LRCSYNC_MUSIC_DIR` | `/music` | Native process music root; fixed to `/music` in Compose. |
| `LRCSYNC_DB_PATH` | `/config/cache.sqlite3` | Native process database file; fixed to this path in Compose. |
| `LRCSYNC_CONCURRENCY` | `8` | Directory workers, 1 to 128. HTTP requests remain serialized. |
| `LRCSYNC_MAX_PENDING_DIRS` | `128` | Maximum queued and running directories, 1 to 4096. |
| `LRCSYNC_WATCH_MODE` | `poll` | `poll` for network filesystems or `native` for local filesystem events. |
| `LRCSYNC_POLL_INTERVAL_SECONDS` | `30` | Polling interval, 1 to 300 seconds. |
| `LRCSYNC_FALLBACK_SCAN_SECONDS` | `300` | Full scan interval, 60 to 604800 seconds. |
| `LRCSYNC_FOLLOW_SYMLINKS` | `false` | Allow scans and events through symlinks, including targets outside the root. |
| `LRCSYNC_CLEAN_FALLBACK` | `true` | Allow packaging cleanup and validated album relaxation. |
| `LRCSYNC_RETRY_NOT_FOUND_DAYS` | `7` | Miss lifetime, 1 to 3650 days. |
| `LRCSYNC_REQUEST_INTERVAL_MS` | `750` | Delay after an HTTP response, 0 to 60000 milliseconds. |
| `LRCSYNC_REQUEST_TIMEOUT_SECONDS` | `30` | HTTP timeout, 1 to 300 seconds. |
| `LRCSYNC_ORPHAN_ACTION` | `keep` | Unmatched sidecar handling described below. |
| `RUST_LOG` | `info` | Process log filter. |

## Orphan handling

Sidecars are paired by exact filename stem and case-insensitive `.lrc` extension. Multiple sidecars or different recordings sharing a stem are preserved without choosing one. Orphan classification checks for newly added audio again before changing a file.

- `keep`: Preserve unmatched sidecars.
- `reconcile`: Move a sidecar only when its filename, with optional track-number prefix and punctuation removed, uniquely agrees with the audio title tag and its stored recording identity agrees with the destination. Untracked sidecars require only the unique title match. Known unchanged generated sidecars remain intact; exact cache reuse publishes generated lyrics under the new owner. Numeric words in the audio title remain significant. Track numbers alone never establish a match. Recording variants remain part of the title.
- `quarantine`: Archive unmatched files in their parent's `.lrcsync-orphans` directory. Colliding names receive a suffix; repeated scans exclude the archive. Empty and non-UTF-8 files retain their original bytes.
- `delete`: Permanently remove unmatched sidecars.

Reconciliation and quarantine use atomic moves without replacing an existing destination. If the filesystem cannot perform that move, the source remains intact. Ingestion and quarantine preserve files larger than 2 MiB with an error; reconciliation skips unreadable candidates. Delete mode removes unmatched files regardless of size. Sidecar symlinks and special files are never ingested. Keep the default when other software imports audio and lyrics in separate steps; a scan cannot prove that an unmatched file is abandoned.

## Development

```sh
make verify   # Formatting, strict Clippy, and behavioral tests
make build    # Locked container build, including tests
make update   # Resolve the latest allowed crate versions and verify
```

Tests use temporary libraries, synthetic metadata, and a loopback HTTP server. They cover lookup ambiguity, cache corrections, publication races, orphan handling, native and polling watchers, queue bounds, and shutdown.

CI verifies code with the latest stable Rust toolchain and builds the container. Daily Dependabot checks propose crate, Docker image, and GitHub Actions updates. These proposals require review and merge; they do not update a running service.

## License

[Apache License 2.0](LICENSE).
