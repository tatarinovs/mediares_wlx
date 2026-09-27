//! Audio tags and stream properties, read by symphonia while it opens the container (ID3v1/v2,
//! APE, Vorbis comments, MP4 ilst, RIFF INFO, Matroska tags...). Only headers and tags are read —
//! cheap enough for TC's columns (WDX) and the viewer.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::codecs::audio::well_known::*;
use symphonia::core::codecs::audio::AudioCodecId;
use symphonia::core::formats::TrackType;
use symphonia::core::io::ReadBytes;
use symphonia::core::meta::{
    MetadataBuilder, MetadataInfo, MetadataRevision, RawValue, StandardTag, StandardVisualKey, Tag,
    METADATA_ID_NULL,
};
use symphonia::default::meta::embedded::riff;

use crate::audio_decode::{open_format, track_duration};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub duration_sec: f64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub composer: Option<String>,
    pub track_total: Option<u32>,
    pub disc_total: Option<u32>,
    /// "MP3", "FLAC", "AAC", "ALAC"...
    pub codec: Option<&'static str>,
    pub lossless: Option<bool>,
    /// An embedded picture exists (even if `cover` was not kept).
    pub has_cover: bool,
    /// Encoded front cover (JPEG/PNG...), or the first embedded picture.
    pub cover: Option<Vec<u8>>,
}

/// Reads tags and stream properties; `None` if the file can't be parsed at all.
/// `with_cover`: keep the picture bytes (the viewer shows them; WDX fields only need `has_cover`).
pub fn read_tags(path: &Path, with_cover: bool) -> Option<AudioTags> {
    let mut format = open_format(path)?;
    let track = format.default_track(TrackType::Audio)?;
    let track_id = u64::from(track.id);
    let params = track.codec_params.as_ref()?.audio()?.clone();
    let duration_sec = params
        .sample_rate
        .and_then(|rate| track_duration(track, rate))
        .unwrap_or(0.0);
    let codec = codec_name(params.codec);
    let lossless = codec.map(|(_, lossless)| lossless);
    let mut tags = AudioTags {
        duration_sec,
        sample_rate: params.sample_rate.filter(|&r| r > 0),
        channels: params
            .channels
            .as_ref()
            .and_then(|c| u8::try_from(c.count()).ok())
            .filter(|&c| c > 0),
        // Lossy decoders report their output format, not something the file has.
        bit_depth: params
            .bits_per_sample
            .filter(|_| lossless == Some(true))
            .and_then(|b| u8::try_from(b).ok())
            .filter(|&b| b > 0),
        codec: codec.map(|(name, _)| name),
        lossless,
        ..Default::default()
    };

    // The log holds trailing tags (APE, ID3v1, an ID3v2 with a footer) before the leading ones and
    // the container's own. Newest first, so the container's tags and the ID3v2 at the start of the
    // file lead; then APE; ID3v1 last — its fields are cut to 30 characters. Garbled text from one
    // tag is replaced by a clean value from another (see `fill`).
    let mut revisions: Vec<MetadataRevision> = Vec::new();
    let mut log = format.metadata();
    while let Some(rev) = log.pop() {
        revisions.push(rev);
    }
    revisions.extend(log.current().cloned());
    revisions.reverse();
    let is_wav = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    if is_wav {
        revisions.extend(wav_trailing_info(path));
    }
    revisions.sort_by_key(|rev| match rev.info.short_name {
        "apev1" | "apev2" => 1,
        "id3v1" => 2,
        _ => 0,
    });
    let mut pictures = 0;
    // Comments with a description ("ID3v1 Comment", "iTunNORM"...) only when there is no plain one.
    let mut described_comment = None;
    for rev in &revisions {
        let per_track = rev
            .per_track
            .iter()
            .filter(|t| t.track_id == track_id)
            .map(|t| &t.metadata);
        for container in std::iter::once(&rev.media).chain(per_track) {
            for tag in &container.tags {
                tags.apply(tag, &mut described_comment);
            }
            let visuals = &container.visuals;
            pictures += visuals.iter().map(|v| v.data.len()).sum::<usize>();
            tags.has_cover |= !visuals.is_empty();
            if with_cover && tags.cover.is_none() {
                let front = visuals
                    .iter()
                    .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
                    .or(visuals.first());
                tags.cover = front.map(|v| v.data.to_vec()).filter(|d| !d.is_empty());
            }
        }
    }

    let mut mss = format.into_inner();
    let start = mss.pos();
    let audio_span = mss.seek(SeekFrom::End(0)).ok().map(|len| (start, len));
    // An empty Xing header leaves an MPEG stream without a length: estimate it from the bitrate
    // of the frame the audio starts with.
    let mpeg = matches!(codec, Some(("MP1" | "MP2" | "MP3", _)));
    if let (true, true, Some((start, len))) = (tags.duration_sec == 0.0, mpeg, audio_span) {
        let mut head = [0u8; 4];
        let kbps = mss.seek(SeekFrom::Start(start)).is_ok() && mss.read_exact(&mut head).is_ok();
        if let Some(kbps) = kbps.then(|| mpeg_frame_kbps(head)).flatten() {
            tags.duration_sec = (len - start) as f64 * 8.0 / (kbps as f64 * 1000.0);
        }
    }
    tags.bitrate_kbps = bitrate_kbps(&params, codec, tags.duration_sec, audio_span, pictures);
    tags.comment = tags.comment.take().or(described_comment);
    Some(tags)
}

/// Larger LIST chunks are not read.
const MAX_INFO_LIST: u32 = 1 << 20;

/// LIST/INFO chunks after the `data` chunk of a WAV file: symphonia's reader stops at the data,
/// while many programs append the tags. Values that aren't UTF-8 are taken in the ANSI code page.
fn wav_trailing_info(path: &Path) -> Option<MetadataRevision> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 12];
    file.read_exact(&mut head).ok()?;
    if &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return None;
    }
    let len = file.metadata().ok()?.len();
    let mut builder = MetadataBuilder::new(MetadataInfo {
        metadata: METADATA_ID_NULL,
        short_name: "riff-info",
        long_name: "RIFF INFO after the audio data",
    });
    let (mut pos, mut after_data, mut found) = (12u64, false, false);
    while pos + 8 <= len {
        let mut chunk = [0u8; 8];
        file.seek(SeekFrom::Start(pos)).ok()?;
        file.read_exact(&mut chunk).ok()?;
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        match &chunk[..4] {
            b"data" => after_data = true,
            b"LIST" if after_data && (4..=MAX_INFO_LIST).contains(&size) => {
                let mut list = vec![0u8; size as usize];
                file.read_exact(&mut list).ok()?;
                let mut items = if list.starts_with(b"INFO") {
                    &list[4..]
                } else {
                    &[][..]
                };
                while items.len() >= 8 {
                    let id = [items[0], items[1], items[2], items[3]];
                    let n = u32::from_le_bytes([items[4], items[5], items[6], items[7]]) as usize;
                    let Some(value) = items.get(8..8 + n) else {
                        break;
                    };
                    let value = value.split(|&b| b == 0).next().unwrap_or_default();
                    let text = match std::str::from_utf8(value) {
                        Ok(utf8) => utf8.to_string(),
                        Err(_) => crate::ffi::ansi_to_os_string(value)
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    };
                    found |= riff::parse_riff_info_chunk(id, text.as_bytes(), &mut builder).is_ok();
                    items = items.get(8 + n + (n & 1)..).unwrap_or_default();
                }
            }
            _ => {}
        }
        pos += 8 + size as u64 + (size & 1) as u64;
    }
    found.then(|| builder.build())
}

/// Uncompressed PCM from its format; otherwise the size of the audio data over the duration. The
/// data runs from where the container reader stopped (past the leading tags) to the end of the
/// file; when the reader stopped far from the start (e.g. MP4 with the index at the end), the
/// whole file minus the pictures is taken instead.
fn bitrate_kbps(
    params: &symphonia::core::codecs::audio::AudioCodecParameters,
    codec: Option<(&str, bool)>,
    duration_sec: f64,
    audio_span: Option<(u64, u64)>,
    pictures: usize,
) -> Option<u32> {
    let kbps = |bits_per_sec: f64| (bits_per_sec / 1000.0).round() as u32;
    if matches!(codec, Some(("PCM" | "PCM float", _))) {
        let (rate, channels, bits) = (
            params.sample_rate?,
            params.channels.as_ref()?.count(),
            params.bits_per_sample?,
        );
        return Some(kbps(rate as f64 * channels as f64 * bits as f64)).filter(|&k| k > 0);
    }
    let (start, len) = audio_span?;
    if duration_sec <= 0.0 {
        return None;
    }
    let data = if start <= len / 2 {
        len - start
    } else {
        len.saturating_sub(pictures as u64)
    };
    Some(kbps(data as f64 * 8.0 / duration_sec)).filter(|&k| k > 0)
}

/// Bitrate (kbit/s) in an MPEG audio frame header; `None` for free-format or invalid headers.
fn mpeg_frame_kbps(h: [u8; 4]) -> Option<u32> {
    const V1_L1: [u16; 14] = [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [u16; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [u16; 14] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [u16; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [u16; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    // Version: 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5. Layer: 3 = I, 2 = II, 1 = III.
    let (version, layer, index) = ((h[1] >> 3) & 3, (h[1] >> 1) & 3, (h[2] >> 4) as usize);
    if version == 1 || layer == 0 || !(1..=14).contains(&index) {
        return None;
    }
    let table = match (version == 3, layer) {
        (true, 3) => &V1_L1,
        (true, 2) => &V1_L2,
        (true, _) => &V1_L3,
        (false, 3) => &V2_L1,
        (false, _) => &V2_L23,
    };
    Some(table[index - 1] as u32)
}

/// Codec name and whether it is lossless.
fn codec_name(id: AudioCodecId) -> Option<(&'static str, bool)> {
    let within = |first, last| (first..=last).contains(&id);
    Some(match id {
        _ if within(CODEC_ID_PCM_F32LE, CODEC_ID_PCM_F64BE_PLANAR) => ("PCM float", true),
        CODEC_ID_PCM_ALAW | CODEC_ID_PCM_MULAW => ("PCM", false),
        _ if within(CODEC_ID_PCM_S32LE, CODEC_ID_PCM_U8_PLANAR) => ("PCM", true),
        _ if within(CODEC_ID_ADPCM_G722, CODEC_ID_ADPCM_IMA_QT) => ("ADPCM", false),
        CODEC_ID_MP1 => ("MP1", false),
        CODEC_ID_MP2 => ("MP2", false),
        CODEC_ID_MP3 => ("MP3", false),
        CODEC_ID_AAC => ("AAC", false),
        CODEC_ID_VORBIS => ("Vorbis", false),
        CODEC_ID_OPUS => ("Opus", false),
        CODEC_ID_SPEEX => ("Speex", false),
        CODEC_ID_MUSEPACK => ("Musepack", false),
        CODEC_ID_AC3 => ("AC-3", false),
        CODEC_ID_EAC3 => ("E-AC-3", false),
        CODEC_ID_DCA => ("DTS", false),
        CODEC_ID_WMA => ("WMA", false),
        CODEC_ID_FLAC => ("FLAC", true),
        CODEC_ID_WAVPACK => ("WavPack", true),
        CODEC_ID_MONKEYS_AUDIO => ("Monkey's Audio", true),
        CODEC_ID_ALAC => ("ALAC", true),
        CODEC_ID_TTA => ("TTA", true),
        CODEC_ID_TRUEHD => ("TrueHD", true),
        _ => return None,
    })
}

/// Keeps the first non-blank value found for a field (repaired if garbled), unless it stays
/// unreadable and a readable one turns up.
fn fill(slot: &mut Option<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let value = repair(value).unwrap_or_else(|| value.to_string());
    if slot
        .as_deref()
        .is_none_or(|old| unreadable(old) && !unreadable(&value))
    {
        *slot = Some(value);
    }
}

fn unreadable(s: &str) -> bool {
    looks_garbled(s) || s.contains('\u{FFFD}')
}

/// Garbled text decoded again from its original bytes: as UTF-8 if they are valid UTF-8,
/// otherwise in the system ANSI code page (cp1251 on a Russian Windows).
fn repair(s: &str) -> Option<String> {
    if !looks_garbled(s) {
        return None;
    }
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    let text = match String::from_utf8(bytes) {
        Ok(utf8) => utf8,
        Err(e) => crate::ffi::ansi_to_os_string(e.as_bytes())?
            .to_string_lossy()
            .into_owned(),
    };
    (!looks_garbled(&text)).then_some(text)
}

/// Text decoded with the wrong code page: Latin-1 letters where cp1251 or UTF-8 bytes were meant
/// ("Àðèÿ", "ÐÐ»ÐµÐºÑ"). Most letters are non-ASCII and nothing is above U+00FF.
fn looks_garbled(s: &str) -> bool {
    let (mut letters, mut accented) = (0, 0);
    for c in s.chars() {
        if c as u32 > 0xFF {
            return false;
        }
        if c.is_alphabetic() {
            letters += 1;
            accented += !c.is_ascii() as usize;
        }
    }
    accented * 2 > letters
}

fn fill_num(slot: &mut Option<u32>, value: u64) {
    if slot.is_none() && value > 0 {
        *slot = u32::try_from(value).ok();
    }
}

/// Years outside 1000..=9999 are typos ("206") or placeholders.
fn fill_year(slot: &mut Option<u32>, year: u64) {
    if (1000..=9999).contains(&year) {
        fill_num(slot, year);
    }
}

/// The year a date starts with ("2019", "2019-07-10...").
fn year_of(date: &str) -> Option<u64> {
    let digits = date.trim().get(..4)?;
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse().ok())
        .flatten()
}

/// A bare ID3 genre number ("(255)", "12") that didn't resolve to a name.
fn is_genre_number(genre: &str) -> bool {
    let n = genre.trim().trim_start_matches('(').trim_end_matches(')');
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

/// Text value of a tag's sub-field (ID3v2 descriptions).
fn sub_field<'a>(tag: &'a Tag, name: &str) -> Option<&'a str> {
    tag.raw
        .sub_fields
        .as_deref()?
        .iter()
        .find_map(|f| match &f.value {
            RawValue::String(s) if f.field == name => Some(s.as_str()),
            _ => None,
        })
}

impl AudioTags {
    fn apply(&mut self, tag: &Tag, described_comment: &mut Option<String>) {
        use StandardTag::*;
        let Some(std) = &tag.std else {
            self.apply_unmapped(tag);
            return;
        };
        match std {
            TrackTitle(v) => fill(&mut self.title, v),
            Artist(v) => fill(&mut self.artist, v),
            Album(v) => fill(&mut self.album, v),
            AlbumArtist(v) => fill(&mut self.album_artist, v),
            Genre(v) if !is_genre_number(v) => fill(&mut self.genre, v),
            Comment(v) if sub_field(tag, "SHORT_DESCRIPTION").is_some() => {
                fill(described_comment, v)
            }
            Comment(v) => fill(&mut self.comment, v),
            // MP4 keeps the composer in ©wrt.
            Composer(v) | Writer(v) => fill(&mut self.composer, v),
            RecordingYear(y) | ReleaseYear(y) => fill_year(&mut self.year, u64::from(*y)),
            RecordingDate(d) | ReleaseDate(d) => {
                if let Some(y) = year_of(d) {
                    fill_year(&mut self.year, y);
                }
            }
            TrackNumber(n) => fill_num(&mut self.track, *n),
            TrackTotal(n) => fill_num(&mut self.track_total, *n),
            DiscNumber(n) => fill_num(&mut self.disc, *n),
            DiscTotal(n) => fill_num(&mut self.disc_total, *n),
            _ => {}
        }
    }

    /// Text tags symphonia leaves without a standard meaning.
    fn apply_unmapped(&mut self, tag: &Tag) {
        let text = match &tag.raw.value {
            RawValue::String(v) => v.to_string(),
            // ID3v2 frames padded with NULs come out as a list of one value and empty strings.
            RawValue::StringList(v) => v
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("; "),
            _ => return,
        };
        let slot = match tag.raw.key.as_str() {
            "TT2" | "TIT2" => &mut self.title,
            "TP1" | "TPE1" => &mut self.artist,
            "TAL" | "TALB" => &mut self.album,
            "TP2" | "TPE2" => &mut self.album_artist,
            "TCO" | "TCON" => &mut self.genre,
            "TCM" | "TCOM" => &mut self.composer,
            // foobar2000 and others write the album artist as TXXX:ALBUM ARTIST.
            "TXXX"
                if sub_field(tag, "DESCRIPTION").is_some_and(|d| {
                    d.eq_ignore_ascii_case("album artist") || d.eq_ignore_ascii_case("albumartist")
                }) =>
            {
                &mut self.album_artist
            }
            _ => return,
        };
        fill(slot, &text);
    }

    /// The track's artist, or the album artist.
    pub fn any_artist(&self) -> Option<&str> {
        self.artist.as_deref().or(self.album_artist.as_deref())
    }

    /// "Artist — Title", falling back to whichever is known.
    pub fn display_title(&self) -> Option<String> {
        match (self.any_artist(), &self.title) {
            (Some(a), Some(t)) => Some(format!("{} — {}", a, t)),
            (None, Some(t)) => Some(t.clone()),
            (Some(a), None) => Some(a.to_string()),
            (None, None) => None,
        }
    }

    /// "artist - title" lower-cased, punctuation dropped, whitespace collapsed: equal for the same
    /// song tagged slightly differently (duplicate search).
    pub fn normalized_artist_title(&self) -> Option<String> {
        let norm = |s: &str| {
            let mut out = String::with_capacity(s.len());
            for word in s
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
            {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.extend(word.chars().flat_map(char::to_lowercase));
            }
            out
        };
        let (artist, title) = (norm(self.any_artist()?), norm(self.title.as_deref()?));
        (!artist.is_empty() && !title.is_empty()).then(|| format!("{} - {}", artist, title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
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
        assert_eq!(AudioTags::default().display_title(), None);
    }

    #[test]
    fn codec_names() {
        assert_eq!(codec_name(CODEC_ID_MP3), Some(("MP3", false)));
        assert_eq!(codec_name(CODEC_ID_MP2), Some(("MP2", false)));
        assert_eq!(codec_name(CODEC_ID_FLAC), Some(("FLAC", true)));
        assert_eq!(codec_name(CODEC_ID_PCM_S16LE), Some(("PCM", true)));
        assert_eq!(codec_name(CODEC_ID_PCM_F32LE), Some(("PCM float", true)));
        assert_eq!(codec_name(CODEC_ID_PCM_MULAW), Some(("PCM", false)));
        assert_eq!(codec_name(CODEC_ID_OPUS), Some(("Opus", false)));
    }

    #[test]
    fn garbled_text_is_replaced() {
        assert!(looks_garbled("Àðèÿ"));
        assert!(looks_garbled("Ð»ÐµÐºÑ"));
        assert!(!looks_garbled("Motörhead"));
        assert!(!looks_garbled("Ария"));
        // UTF-8 bytes shown as Latin-1 come back whatever the system code page.
        assert_eq!(repair("Ð\u{90}Ñ\u{80}Ð¸Ñ\u{8f}").as_deref(), Some("Ария"));
        let mut slot = None;
        fill(&mut slot, " Ð\u{90}Ñ\u{80}Ð\u{FFFD} ");
        fill(&mut slot, "Ария");
        fill(&mut slot, "Другое");
        assert_eq!(slot.as_deref(), Some("Ария"));
    }

    #[test]
    fn mpeg_header_bitrates() {
        assert_eq!(mpeg_frame_kbps([0xFF, 0xFB, 0x90, 0x44]), Some(128)); // MPEG-1 Layer III
        assert_eq!(mpeg_frame_kbps([0xFF, 0xF3, 0x40, 0x00]), Some(32)); // MPEG-2 Layer III
        assert_eq!(mpeg_frame_kbps([0xFF, 0xFD, 0xA0, 0x00]), Some(192)); // MPEG-1 Layer II
        assert_eq!(mpeg_frame_kbps([0xFF, 0xFB, 0x00, 0x00]), None); // free format
        assert_eq!(mpeg_frame_kbps([0x49, 0x44, 0x33, 0x03]), None);
    }

    #[test]
    fn genre_numbers() {
        assert!(is_genre_number("(255)"));
        assert!(is_genre_number("12"));
        assert!(!is_genre_number("Rock"));
        assert!(!is_genre_number("()"));
    }

    #[test]
    fn years_from_dates() {
        assert_eq!(year_of("2019-07-10T12:00"), Some(2019));
        assert_eq!(year_of(" 1987"), Some(1987));
        assert_eq!(year_of("87"), None);
        assert_eq!(year_of("весна"), None);
    }

    #[test]
    fn reads_wav_properties() {
        let path = crate::test_util::pcm16_wav("tags", 8000, 1, 16000, |_| 0);

        let tags = read_tags(&path, false).expect("wav");
        let _ = std::fs::remove_file(&path);
        assert_eq!(tags.codec, Some("PCM"));
        assert_eq!(tags.lossless, Some(true));
        assert_eq!(
            (tags.sample_rate, tags.channels, tags.bit_depth),
            (Some(8000), Some(1), Some(16))
        );
        assert_eq!(tags.bitrate_kbps, Some(128));
        assert!((tags.duration_sec - 2.0).abs() < 0.01);
    }

    #[test]
    fn normalized_artist_title() {
        let t = AudioTags {
            artist: Some("AC/DC".into()),
            title: Some("  Highway to  Hell! ".into()),
            ..Default::default()
        };
        assert_eq!(
            t.normalized_artist_title().as_deref(),
            Some("ac dc - highway to hell")
        );
        let album_only = AudioTags {
            album_artist: Some("Пикник".into()),
            title: Some("Остров".into()),
            ..Default::default()
        };
        assert_eq!(
            album_only.normalized_artist_title().as_deref(),
            Some("пикник - остров")
        );
        assert_eq!(
            AudioTags {
                title: Some("x".into()),
                ..Default::default()
            }
            .normalized_artist_title(),
            None
        );
    }
}
