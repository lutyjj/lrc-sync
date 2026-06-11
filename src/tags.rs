use std::path::Path;
use std::sync::LazyLock;

use anyhow::{anyhow, Context, Result};
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::Accessor,
    read_from_path,
};
use regex::Regex;

static RE_CLEAN_METADATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*[\(\[\{](?:[^\)\]\}]*?\b)?(?:remaster|remastered|mix|remix|live|edit|version|session|deluxe|anniversary|edition|mono|stereo|re-recorded|digitally|reissue|restored)\b[^\)\]\}]*?[\)\]\}]\s*$",
    )
    .expect("valid clean metadata regex")
});

#[derive(Debug, Clone, Default)]
pub struct TrackTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: i64,
}

impl TrackTags {

    pub fn cleaned(&self) -> Self {
        Self {
            title: clean_metadata_value(&self.title),
            artist: self.artist.clone(),
            album: clean_metadata_value(&self.album),
            duration_secs: self.duration_secs,
        }
    }
}

pub fn read_tags(path: &Path) -> Result<TrackTags> {
    let tagged =
        read_from_path(path).with_context(|| format!("reading tags from {}", path.display()))?;
    let duration_secs = tagged.properties().duration().as_secs() as i64;
    let tag = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .ok_or_else(|| anyhow!("no metadata tag found"))?;

    let title = tag
        .title()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing title tag"))?;
    let artist = tag
        .artist()
        .or_else(|| {
            tag.get_string(&lofty::tag::ItemKey::AlbumArtist)
                .map(Into::into)
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing artist tag"))?;
    let album = tag
        .album()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    Ok(TrackTags {
        title,
        artist,
        album,
        duration_secs,
    })
}

fn clean_metadata_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    RE_CLEAN_METADATA.replace(value, "").trim().to_string()
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
}
