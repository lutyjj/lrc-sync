# Metadata fixtures

These MP3 files contain generated silence and synthetic metadata. They contain no commercial audio or lyrics.

- `whitespace-artist.mp3` has a whitespace-only artist and a usable album artist.
- `incomplete-primary.mp3` has incomplete ID3v2 metadata and usable ID3v1 fallback fields.

The tests generate metadata-only FLAC files directly in temporary directories. HTTP tests use a loopback server and synthetic lyric text; they do not call LRCLib.
