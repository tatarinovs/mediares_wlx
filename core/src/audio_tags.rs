//! Audio tags and properties via `lofty` (ID3, Vorbis comments, MP4 ilst, APE, RIFF INFO...).

use std::path::Path;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::tag::{Accessor, ItemKey, Tag};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub duration_sec: f64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    /// Encoded front cover (JPEG/PNG...), or the first embedded picture.
    pub cover: Option<Vec<u8>>,
}

/// Reads tags and stream properties; `None` if the file can't be parsed at all.
pub fn read_tags(path: &Path) -> Option<AudioTags> {
    let file = lofty::read_from_path(path).ok()?;
    let props = file.properties();
    let mut tags = AudioTags {
        duration_sec: props.duration().as_secs_f64(),
        bitrate_kbps: props.audio_bitrate().filter(|&b| b > 0),
        sample_rate: props.sample_rate().filter(|&r| r > 0),
        bit_depth: props.bit_depth().filter(|&d| d > 0),
        channels: props.channels().filter(|&c| c > 0),
        ..Default::default()
    };

    // The primary tag first, then the others fill in what it lacks.
    let mut all: Vec<&Tag> = file.primary_tag().into_iter().collect();
    all.extend(file.tags().iter().filter(|t| Some(t.tag_type()) != file.primary_tag().map(|p| p.tag_type())));
    for tag in all {
        let text = |s: Option<std::borrow::Cow<'_, str>>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        tags.title = tags.title.take().or_else(|| text(tag.title()));
        tags.artist = tags.artist.take().or_else(|| text(tag.artist())).or_else(|| text(tag.get_string(ItemKey::AlbumArtist).map(Into::into)));
        tags.album = tags.album.take().or_else(|| text(tag.album()));
        tags.year = tags.year.or_else(|| tag.date().map(|d| d.year as u32));
        tags.track = tags.track.or_else(|| tag.track());
        if tags.cover.is_none() {
            let pictures = tag.pictures();
            let front = pictures.iter().find(|p| p.pic_type() == PictureType::CoverFront).or(pictures.first());
            tags.cover = front.map(|p| p.data().to_vec()).filter(|d| !d.is_empty());
        }
    }
    Some(tags)
}

impl AudioTags {
    /// "Artist — Title", falling back to whichever is known.
    pub fn display_title(&self) -> Option<String> {
        match (&self.artist, &self.title) {
            (Some(a), Some(t)) => Some(format!("{} — {}", a, t)),
            (None, Some(t)) => Some(t.clone()),
            (Some(a), None) => Some(a.clone()),
            (None, None) => None,
        }
    }

    /// e.g. "44.1 кГц · 16 бит · стерео · 320 кбит/с".
    pub fn format_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(rate) = self.sample_rate {
            let khz = rate as f64 / 1000.0;
            parts.push(if rate % 1000 == 0 { format!("{} кГц", rate / 1000) } else { format!("{:.1} кГц", khz).replace('.', ",") });
        }
        if let Some(bits) = self.bit_depth {
            parts.push(format!("{} бит", bits));
        }
        if let Some(ch) = self.channels {
            parts.push(match ch {
                1 => "моно".to_string(),
                2 => "стерео".to_string(),
                n => format!("{} кан.", n),
            });
        }
        if let Some(kbps) = self.bitrate_kbps {
            parts.push(format!("{} кбит/с", kbps));
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_format_line() {
        let t = AudioTags {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            bitrate_kbps: Some(1411),
            ..Default::default()
        };
        assert_eq!(t.display_title().as_deref(), Some("Band — Song"));
        assert_eq!(t.format_line(), "44,1 кГц · 16 бит · стерео · 1411 кбит/с");
        assert_eq!(AudioTags::default().display_title(), None);
        assert_eq!(AudioTags { sample_rate: Some(48000), ..Default::default() }.format_line(), "48 кГц");
    }
}
