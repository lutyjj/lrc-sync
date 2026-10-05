use std::sync::LazyLock;
use std::{borrow::Cow, path::Path};

use anyhow::{Context, Result, anyhow};
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::Accessor,
    read_from_path,
    tag::{ItemKey, Tag},
};
use regex::Regex;

static RE_CLEAN_METADATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)\s*[\(\[\{](?:(?:\d{4}\s+)?remaster(?:ed)?(?:\s+\d{4})?|(?:deluxe|expanded)(?:\s+edition)?|\d{1,3}(?:st|nd|rd|th)?\s+anniversary(?:\s+edition)?)[\)\]\}]\s*$",
    )
    .expect("valid clean metadata regex")
});

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: i64,
}

impl TrackTags {
    /// Strip release packaging labels while preserving recording variants.
    pub fn cleaned(&self) -> Self {
        Self {
            title: clean_metadata_value(&self.title),
            artist: self.artist.clone(),
            album: clean_metadata_value(&self.album),
            duration_secs: self.duration_secs,
        }
    }

    pub fn normalized(&self) -> Self {
        Self {
            artist: normalize(&self.artist),
            title: normalize(&self.title),
            album: normalize(&self.album),
            duration_secs: self.duration_secs,
        }
    }
}

pub fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn read_tags(path: &Path) -> Result<TrackTags> {
    let tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;
    let duration_secs = tagged.properties().duration().as_secs() as i64;
    let primary = tagged.primary_tag();
    let tags: Vec<&Tag> = primary
        .into_iter()
        .chain(
            tagged
                .tags()
                .iter()
                .filter(|tag| !primary.is_some_and(|primary| std::ptr::eq(primary, *tag))),
        )
        .collect();
    let title = first_nonempty(tags.iter().map(|tag| tag.title()))
        .ok_or_else(|| anyhow!("missing title tag"))?;
    let artist = first_nonempty(tags.iter().map(|tag| tag.artist()))
        .or_else(|| {
            first_nonempty(
                tags.iter()
                    .map(|tag| tag.get_string(ItemKey::AlbumArtist).map(Cow::Borrowed)),
            )
        })
        .ok_or_else(|| anyhow!("missing artist tag"))?;
    let album = first_nonempty(tags.iter().map(|tag| tag.album())).unwrap_or_default();

    Ok(TrackTags {
        title,
        artist,
        album,
        duration_secs,
    })
}

fn first_nonempty<'a>(values: impl Iterator<Item = Option<Cow<'a, str>>>) -> Option<String> {
    values
        .flatten()
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

fn clean_metadata_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    let mut value = value.trim().to_owned();
    loop {
        let cleaned = RE_CLEAN_METADATA.replace(&value, "").trim().to_owned();
        if cleaned == value {
            return value;
        }
        value = cleaned;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_metadata_strips_remaster() {
        assert_eq!(
            clean_metadata_value("Song Title (Remastered 2023)"),
            "Song Title"
        );
    }

    #[test]
    fn test_clean_metadata_strips_deluxe_edition() {
        assert_eq!(
            clean_metadata_value("Album Name [Deluxe Edition]"),
            "Album Name"
        );
    }

    #[test]
    fn test_clean_metadata_preserves_normal() {
        assert_eq!(
            clean_metadata_value("Normal Song Title"),
            "Normal Song Title"
        );
    }

    #[test]
    fn test_clean_metadata_empty() {
        assert_eq!(clean_metadata_value(""), "");
        assert_eq!(clean_metadata_value("  "), "  ");
    }

    #[test]
    fn test_cleaned_tags() {
        let tags = TrackTags {
            title: "Song (Remastered 2020)".to_string(),
            artist: "Artist".to_string(),
            album: "Album [Deluxe Edition]".to_string(),
            duration_secs: 200,
        };
        let cleaned = tags.cleaned();
        assert_eq!(cleaned.title, "Song");
        assert_eq!(cleaned.album, "Album");
        assert_eq!(cleaned.artist, "Artist");
        assert_eq!(cleaned.duration_secs, 200);
    }

    #[test]
    fn recording_variants_are_never_removed() {
        for variant in [
            "Live",
            "Remix",
            "Acoustic",
            "Instrumental",
            "Radio Edit",
            "Re-recorded",
            "Mono",
            "Stereo",
            "Session",
            "Demo",
        ] {
            let title = format!("Song ({variant})");
            assert_eq!(clean_metadata_value(&title), title);
        }
        assert_eq!(
            clean_metadata_value("Song (Live) (Remastered 2024)"),
            "Song (Live)"
        );
        assert_eq!(
            clean_metadata_value("Song (Live Remastered 2024)"),
            "Song (Live Remastered 2024)"
        );
    }
    #[test]
    fn whitespace_artist_uses_album_artist() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/whitespace-artist.mp3"
        ));
        let tags = read_tags(path).expect("a usable album artist exists");
        assert_eq!(tags.artist, "Fixture Album Artist");
    }

    #[test]
    fn incomplete_primary_tag_uses_secondary_fields() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/incomplete-primary.mp3"
        ));
        let tags = read_tags(path).expect("usable title and artist exist in the secondary tag");
        assert_eq!(tags.artist, "Fixture Artist");
        assert_eq!(tags.title, "Fixture Song");
    }
}
