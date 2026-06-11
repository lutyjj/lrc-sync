use std::path::Path;

use anyhow::{anyhow, Context, Result};
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::Accessor,
    read_from_path,
};
use regex::Regex;

#[derive(Debug, Clone)]
pub struct TrackTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: i64,
}

impl TrackTags {
    pub fn empty() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration_secs: 0,
        }
    }

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
    let pattern = Regex::new(
        r"(?i)\s*[\(\[\{](?:[^\)\]\}]*?\b)?(?:remaster|remastered|mix|remix|live|edit|version|session|deluxe|anniversary|edition|mono|stereo|re-recorded|digitally|reissue|restored)\b[^\)\]\}]*?[\)\]\}]\s*$",
    )
    .expect("valid clean metadata regex");
    pattern.replace(value, "").trim().to_string()
}
