use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::config::Config;

pub const QUARANTINE: &str = ".lrcsync-orphans";
const MAX_LYRICS_BYTES: u64 = 2 * 1024 * 1024;

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            ["mp3", "flac", "m4a", "ogg", "opus"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

pub fn is_lrc(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lrc"))
}

/// Apply the same root, archive, and symlink policy to scans and watcher events.
pub fn eligible(config: &Config, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(&config.music_dir) else {
        return false;
    };
    if config
        .music_dir
        .canonicalize()
        .is_ok_and(|root| within_archive(&root))
    {
        return false;
    }
    let mut current = config.music_dir.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return false;
        };
        if name == QUARANTINE {
            return false;
        }
        current.push(name);
        if config.follow_symlinks
            && current
                .canonicalize()
                .is_ok_and(|physical| within_archive(&physical))
        {
            return false;
        }
        if !config.follow_symlinks {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => return false,
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return false,
            }
        }
    }
    true
}

fn within_archive(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == QUARANTINE)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStamp {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileStamp {
    pub fn identity(&self) -> Option<[u8; 16]> {
        #[cfg(unix)]
        {
            let mut identity = [0; 16];
            identity[..8].copy_from_slice(&self.device.to_be_bytes());
            identity[8..].copy_from_slice(&self.inode.to_be_bytes());
            Some(identity)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("reading metadata for {}", path.display()))?;
        Self::from_metadata(path, metadata)
    }

    pub fn read_sidecar(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("reading sidecar metadata for {}", path.display()))?;
        Self::from_metadata(path, metadata)
    }

    fn from_metadata(path: &Path, metadata: fs::Metadata) -> Result<Self> {
        if !metadata.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("sidecar is not a regular file: {}", path.display());
    }
    let mut bytes = Vec::new();
    file.take(MAX_LYRICS_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LYRICS_BYTES {
        bail!(
            "lyric file exceeds {MAX_LYRICS_BYTES} bytes: {}",
            path.display()
        );
    }
    Ok(bytes)
}

pub fn read_lyrics(path: &Path) -> Result<String> {
    let lyrics = String::from_utf8(read_bytes(path)?)
        .with_context(|| format!("invalid UTF-8 lyrics in {}", path.display()))?;
    if lyrics.trim().is_empty() {
        bail!("empty lyric file: {}", path.display());
    }
    Ok(lyrics)
}

/// Publish a complete file without replacing a sidecar created during lookup.
pub fn write_new(path: &Path, lyrics: &str) -> Result<bool> {
    if lyrics.trim().is_empty() {
        bail!("refusing to publish empty lyrics");
    }
    write_bytes_new(path, lyrics.as_bytes())
}

fn write_bytes_new(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path.parent().context("sidecar has no parent directory")?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary lyrics in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => {
            #[cfg(unix)]
            File::open(parent)?.sync_all()?;
            Ok(true)
        }
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err.error).with_context(|| format!("publishing {}", path.display())),
    }
}

/// Move the source atomically without replacing an existing destination.
pub fn move_new(source: &Path, destination: &Path) -> Result<bool> {
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        bail!("refusing to move a symbolic-link sidecar");
    }
    read_bytes(source)?;
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    let moved = rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from);
    #[cfg(windows)]
    let moved = fs::rename(source, destination);
    #[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
    let moved = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace moves are unavailable on this platform",
    ));
    match moved {
        Ok(()) => {
            #[cfg(unix)]
            {
                File::open(destination.parent().context("destination has no parent")?)?
                    .sync_all()?;
                File::open(source.parent().context("sidecar has no parent")?)?.sync_all()?;
            }
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "atomically moving {} to {}",
                source.display(),
                destination.display()
            )
        }),
    }
}

pub fn quarantine(source: &Path) -> Result<PathBuf> {
    let directory = source
        .parent()
        .context("orphan has no parent")?
        .join(QUARANTINE);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => bail!(
            "quarantine path is not a real directory: {}",
            directory.display()
        ),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&directory)?,
        Err(err) => return Err(err.into()),
    }
    let name = source.file_name().context("orphan has no filename")?;
    for number in 0u64.. {
        let mut candidate = name.to_os_string();
        if number > 0 {
            candidate.push(format!(".{number}"));
        }
        let destination = directory.join(candidate);
        if move_new(source, &destination)? {
            return Ok(destination);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Barrier},
        thread,
    };
    use tempfile::TempDir;

    #[test]
    fn concurrent_writers_publish_one_complete_file_without_colliding() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("song.lrc");
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = ["first content", "second content"]
            .into_iter()
            .map(|text| {
                let barrier = barrier.clone();
                let path = path.clone();
                thread::spawn(move || {
                    barrier.wait();
                    write_new(&path, text).unwrap()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|created| *created)
                .count(),
            1
        );
        assert!(
            ["first content", "second content"]
                .contains(&fs::read_to_string(&path).unwrap().as_str())
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn quarantine_preserves_colliding_versions() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("song.lrc");
        fs::write(&source, "first version").unwrap();
        let first = quarantine(&source).unwrap();
        fs::write(&source, "second version").unwrap();
        let second = quarantine(&source).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_lyrics(&first).unwrap(), "first version");
        assert_eq!(read_lyrics(&second).unwrap(), "second version");
        assert!(!source.exists());
    }

    #[test]
    fn quarantine_preserves_empty_and_non_utf8_files() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("song.lrc");
        for bytes in [b"".as_slice(), b"\xff\x00".as_slice()] {
            fs::write(&source, bytes).unwrap();
            let archived = quarantine(&source).unwrap();
            assert_eq!(fs::read(archived).unwrap(), bytes);
            assert!(!source.exists());
        }
    }

    #[test]
    fn moving_never_replaces_a_curated_destination() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("old.lrc");
        let target = dir.path().join("new.lrc");
        fs::write(&source, "source").unwrap();
        fs::write(&target, "curated").unwrap();
        assert!(!move_new(&source, &target).unwrap());
        assert_eq!(read_lyrics(&target).unwrap(), "curated");
        assert_eq!(read_lyrics(&source).unwrap(), "source");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_move_preserves_edits_through_an_already_open_file() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("old.lrc");
        let destination = dir.path().join("new.lrc");
        fs::write(&source, "original lyrics").unwrap();
        let mut editor = fs::OpenOptions::new().write(true).open(&source).unwrap();
        let inode = editor.metadata().unwrap().ino();
        assert!(move_new(&source, &destination).unwrap());
        editor.set_len(0).unwrap();
        editor.write_all(b"corrected lyrics").unwrap();
        editor.sync_all().unwrap();
        assert_eq!(fs::metadata(&destination).unwrap().ino(), inode);
        assert_eq!(read_lyrics(&destination).unwrap(), "corrected lyrics");
        assert!(!source.exists());
    }
}
