//! Descriptive tags of video files (title, artist, date...), parsed here for Matroska/WebM and
//! MP4/MOV. Only the header and tag elements are read, never the frames.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub director: Option<String>,
    /// As written in the file ("2019", "2019-07-10T12:00:00+03:00"...).
    pub date: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
}

impl VideoTags {
    /// The year the date starts with ("2019", "2019-07-10...", "2022:12:31 ...").
    pub fn year(&self) -> Option<u32> {
        let date = self.date.as_deref()?.trim();
        let digits = date
            .get(..4)
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))?;
        let rest_is_separate = !date[4..].starts_with(|c: char| c.is_ascii_digit());
        digits
            .parse()
            .ok()
            .filter(|y| (1800..=2200).contains(y) && rest_is_separate)
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Copy)]
enum Container {
    Matroska,
    Mp4,
}

fn container(path: &Path) -> Option<Container> {
    use crate::probe::has_extension;
    if has_extension(path, &["mkv", "webm", "mka", "mk3d"]) {
        Some(Container::Matroska)
    } else if has_extension(path, &["mp4", "m4v", "mov", "qt", "3gp", "3g2"]) {
        Some(Container::Mp4)
    } else {
        None
    }
}

/// Whether tags are parsed for this file's container (by extension; nothing is read).
pub fn has_tag_reader(path: &Path) -> bool {
    container(path).is_some()
}

/// Tags of `path`; empty if the container has none or isn't supported.
pub fn read_video_tags(path: &Path) -> VideoTags {
    let Some(container) = container(path) else {
        return VideoTags::default();
    };
    let Ok(file) = File::open(path) else {
        return VideoTags::default();
    };
    let mut r = BufReader::new(file);
    let tags = match container {
        Container::Matroska => read_matroska(&mut r),
        Container::Mp4 => read_mp4(&mut r),
    };
    tags.unwrap_or_default()
}

fn clean(s: &str) -> Option<String> {
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty()).then(|| s.to_string())
}

/// Keeps the first value found for a field.
fn fill(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        *slot = value;
    }
}

// ---------------------------------------------------------------------------------------------
// MP4 / QuickTime
// ---------------------------------------------------------------------------------------------

/// Larger `udta` boxes are not read.
const MAX_UDTA: u64 = 4 << 20;

/// More boxes on one level means a damaged or crafted file; `moov`/`udta` come far earlier.
const MAX_BOXES: usize = 4096;

/// Boxes `(type, payload start, payload end)` between `start` and `end` of the file.
fn mp4_boxes<R: Read + Seek>(r: &mut R, start: u64, end: u64) -> Vec<([u8; 4], u64, u64)> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end && out.len() < MAX_BOXES {
        let mut hdr = [0u8; 8];
        if r.seek(SeekFrom::Start(pos)).is_err() || r.read_exact(&mut hdr).is_err() {
            break;
        }
        let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (size, header) = match u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) {
            0 => (end - pos, 8),
            1 => {
                let mut big = [0u8; 8];
                if r.read_exact(&mut big).is_err() {
                    break;
                }
                (u64::from_be_bytes(big), 16)
            }
            n => (n as u64, 8),
        };
        if size < header || pos.saturating_add(size) > end {
            break;
        }
        out.push((kind, pos + header, pos + size));
        pos += size;
    }
    out
}

/// Boxes of an in-memory payload.
fn mp4_children(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let size = if size == 0 { data.len() } else { size };
        let payload = data.get(8..size)?;
        data = &data[size..];
        Some((kind, payload))
    })
}

fn read_mp4<R: Read + Seek>(r: &mut R) -> Option<VideoTags> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    let top = mp4_boxes(r, 0, len);
    if !top
        .first()
        .is_some_and(|(kind, _, _)| matches!(kind, b"ftyp" | b"moov" | b"wide" | b"free" | b"mdat"))
    {
        return None;
    }
    let (_, moov_start, moov_end) = *top.iter().find(|(kind, _, _)| kind == b"moov")?;
    let mut tags = VideoTags::default();
    // Only the movie's own udta; tracks may have theirs, about the track.
    for (_, start, end) in mp4_boxes(r, moov_start, moov_end)
        .into_iter()
        .filter(|(kind, _, _)| kind == b"udta")
    {
        if end - start > MAX_UDTA {
            continue;
        }
        let mut udta = vec![0u8; (end - start) as usize];
        r.seek(SeekFrom::Start(start)).ok()?;
        r.read_exact(&mut udta).ok()?;
        parse_udta(&udta, &mut tags);
    }
    Some(tags)
}

/// `udta` holds iTunes-style `meta/ilst` items, QuickTime `©xxx` text atoms, or 3GPP `titl`/`auth`...
fn parse_udta(udta: &[u8], tags: &mut VideoTags) {
    for (kind, payload) in mp4_children(udta) {
        match &kind {
            b"meta" => {
                // A full box (version + flags) in MP4; QuickTime writes it without them.
                let body = if payload.get(4..8) == Some(b"hdlr") {
                    payload
                } else {
                    payload.get(4..).unwrap_or_default()
                };
                for (_, ilst) in mp4_children(body).filter(|(k, _)| k == b"ilst") {
                    for (item, data) in mp4_children(ilst) {
                        apply_mp4(&item, ilst_text(data), tags);
                    }
                }
            }
            [0xA9, ..] => apply_mp4(&kind, quicktime_text(payload), tags),
            b"titl" | b"auth" | b"perf" | b"dscp" | b"gnre" | b"yrrc" => {
                apply_mp4(&kind, threegpp_text(&kind, payload), tags)
            }
            _ => {}
        }
    }
}

fn apply_mp4(kind: &[u8; 4], value: Option<String>, tags: &mut VideoTags) {
    let slot = match kind {
        b"\xA9nam" | b"titl" => &mut tags.title,
        b"\xA9ART" | b"\xA9aut" | b"auth" | b"perf" => &mut tags.artist,
        b"\xA9dir" => &mut tags.director,
        b"\xA9day" | b"yrrc" => &mut tags.date,
        b"\xA9gen" | b"gnre" => &mut tags.genre,
        b"\xA9cmt" | b"\xA9des" | b"desc" | b"ldes" | b"dscp" | b"\xA9inf" => &mut tags.comment,
        _ => return,
    };
    fill(slot, value);
}

/// Value of an `ilst` item: its `data` box, if it is text (type 1, UTF-8).
fn ilst_text(item: &[u8]) -> Option<String> {
    let (_, data) = mp4_children(item).find(|(k, _)| k == b"data")?;
    let kind = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
    (kind == 1)
        .then(|| clean(&String::from_utf8_lossy(data.get(8..)?)))
        .flatten()
}

/// QuickTime text atom: 16-bit length, 16-bit language, text (the first of possibly several).
fn quicktime_text(payload: &[u8]) -> Option<String> {
    if payload.get(8..12) == Some(b"data") {
        // Some muxers put an ilst-style `data` box here instead.
        return ilst_text(payload);
    }
    let len = u16::from_be_bytes(payload.get(..2)?.try_into().ok()?) as usize;
    clean(&String::from_utf8_lossy(payload.get(4..4 + len)?))
}

/// 3GPP asset box: version/flags, then a 16-bit language and UTF-8 or UTF-16 (BOM) text;
/// `yrrc` holds a 16-bit year instead.
fn threegpp_text(kind: &[u8; 4], payload: &[u8]) -> Option<String> {
    if kind == b"yrrc" {
        let year = u16::from_be_bytes(payload.get(4..6)?.try_into().ok()?);
        return (year > 0).then(|| year.to_string());
    }
    let text = payload.get(6..)?;
    if let Some(utf16) = text.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return clean(&String::from_utf16_lossy(&units));
    }
    clean(&String::from_utf8_lossy(text))
}

// ---------------------------------------------------------------------------------------------
// Matroska (EBML)
// ---------------------------------------------------------------------------------------------

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TITLE: u32 = 0x7BA9;
const TAGS: u32 = 0x1254_C367;
const TAG: u32 = 0x7373;
const TARGETS: u32 = 0x63C0;
/// Tags aimed at one track, edition, chapter or attachment — not about the file as a whole.
const TARGET_UIDS: [u32; 4] = [0x63C5, 0x63C9, 0x63C4, 0x63C6];
const SIMPLE_TAG: u32 = 0x67C8;
const TAG_NAME: u32 = 0x45A3;
const TAG_STRING: u32 = 0x4487;
const CLUSTER: u32 = 0x1F43_B675;

/// Larger Info / Tags / SeekHead elements are not read.
const MAX_ELEMENT: u64 = 4 << 20;

/// Size of an element whose end isn't known (live recordings).
const UNKNOWN_SIZE: u64 = u64::MAX;

/// Reads an element ID (its length marker kept, as the spec writes IDs).
fn read_id(r: &mut impl Read) -> Option<u32> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first).ok()?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut id = first[0] as u32;
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest[..len - 1]).ok()?;
    for &b in &rest[..len - 1] {
        id = id << 8 | b as u32;
    }
    Some(id)
}

/// Reads an element size (length marker dropped); all-ones means [`UNKNOWN_SIZE`].
fn read_size(r: &mut impl Read) -> Option<u64> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first).ok()?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut value = (first[0] as u64) & (0xFF >> len);
    let mut all_ones = value == 0xFF >> len;
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..len - 1]).ok()?;
    for &b in &rest[..len - 1] {
        value = value << 8 | b as u64;
        all_ones &= b == 0xFF;
    }
    Some(if all_ones { UNKNOWN_SIZE } else { value })
}

/// Child elements `(id, payload)` of an element already in memory; stops at the first malformed one.
fn children(mut data: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let mut r = data;
        let id = read_id(&mut r)?;
        let size = read_size(&mut r)?;
        let size = usize::try_from(size).ok().filter(|&s| s <= r.len())?;
        let (payload, rest) = r.split_at(size);
        data = rest;
        Some((id, payload))
    })
}

fn uint(data: &[u8]) -> u64 {
    data.iter().take(8).fold(0, |v, &b| v << 8 | b as u64)
}

fn utf8(data: &[u8]) -> Option<String> {
    clean(&String::from_utf8_lossy(data))
}

fn read_matroska<R: Read + Seek>(r: &mut R) -> Option<VideoTags> {
    if read_id(r)? != EBML {
        return None;
    }
    let header = read_size(r)?;
    r.seek(SeekFrom::Current(i64::try_from(header).ok()?))
        .ok()?;
    if read_id(r)? != SEGMENT {
        return None;
    }
    let segment_size = read_size(r)?;
    let segment_start = r.stream_position().ok()?;
    let segment_end = segment_start.saturating_add(segment_size);

    let mut tags = VideoTags::default();
    let (mut info_seen, mut tags_seen) = (false, false);
    let (mut info_at, mut tags_at) = (None, None);

    // Top-level elements in order, up to the first cluster (the frames); Info and Tags usually
    // come before it, and SeekHead says where they are if not.
    while let Ok(pos) = r.stream_position() {
        if pos >= segment_end {
            break;
        }
        let (Some(id), Some(size)) = (read_id(r), read_size(r)) else {
            break;
        };
        if id == CLUSTER || size == UNKNOWN_SIZE {
            break;
        }
        match id {
            SEEK_HEAD | INFO | TAGS if size <= MAX_ELEMENT => {
                let mut data = vec![0u8; size as usize];
                if r.read_exact(&mut data).is_err() {
                    break;
                }
                match id {
                    SEEK_HEAD => {
                        for (seek_id, seek_pos) in seek_entries(&data) {
                            let at = segment_start.checked_add(seek_pos);
                            match seek_id {
                                INFO => info_at = info_at.or(at),
                                TAGS => tags_at = tags_at.or(at),
                                _ => {}
                            }
                        }
                    }
                    INFO => {
                        info_seen = true;
                        parse_info(&data, &mut tags);
                    }
                    _ => {
                        tags_seen = true;
                        parse_tags(&data, &mut tags);
                    }
                }
            }
            _ => {
                let Ok(skip) = i64::try_from(size) else { break };
                if r.seek(SeekFrom::Current(skip)).is_err() {
                    break;
                }
            }
        }
    }

    for (seen, at, want) in [(info_seen, info_at, INFO), (tags_seen, tags_at, TAGS)] {
        if seen {
            continue;
        }
        let Some(data) = at.and_then(|at| read_element_at(r, at, want)) else {
            continue;
        };
        if want == INFO {
            parse_info(&data, &mut tags);
        } else {
            parse_tags(&data, &mut tags);
        }
    }
    Some(tags)
}

fn read_element_at<R: Read + Seek>(r: &mut R, at: u64, want: u32) -> Option<Vec<u8>> {
    r.seek(SeekFrom::Start(at)).ok()?;
    if read_id(r)? != want {
        return None;
    }
    let size = read_size(r)?;
    if size > MAX_ELEMENT {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    r.read_exact(&mut data).ok()?;
    Some(data)
}

fn seek_entries(seek_head: &[u8]) -> Vec<(u32, u64)> {
    children(seek_head)
        .filter(|(id, _)| *id == SEEK)
        .filter_map(|(_, seek)| {
            let mut id = None;
            let mut pos = None;
            for (child, data) in children(seek) {
                match child {
                    SEEK_ID => id = Some(uint(data) as u32),
                    SEEK_POSITION => pos = Some(uint(data)),
                    _ => {}
                }
            }
            id.zip(pos)
        })
        .collect()
}

fn parse_info(info: &[u8], tags: &mut VideoTags) {
    if let Some((_, title)) = children(info).find(|(id, _)| *id == TITLE) {
        tags.title = tags.title.take().or_else(|| utf8(title));
    }
}

fn parse_tags(data: &[u8], tags: &mut VideoTags) {
    for (_, tag) in children(data).filter(|(id, _)| *id == TAG) {
        let about_part = children(tag)
            .filter(|(id, _)| *id == TARGETS)
            .flat_map(|(_, targets)| children(targets))
            .any(|(id, uid)| TARGET_UIDS.contains(&id) && uint(uid) != 0);
        if about_part {
            continue;
        }
        for (_, simple) in children(tag).filter(|(id, _)| *id == SIMPLE_TAG) {
            let mut name = None;
            let mut value = None;
            for (id, data) in children(simple) {
                match id {
                    TAG_NAME => name = utf8(data),
                    TAG_STRING => value = utf8(data),
                    _ => {}
                }
            }
            let (Some(name), Some(value)) = (name, value) else {
                continue;
            };
            let slot = match name.to_ascii_uppercase().as_str() {
                "TITLE" => &mut tags.title,
                "ARTIST" | "LEAD_PERFORMER" => &mut tags.artist,
                "DIRECTOR" => &mut tags.director,
                "DATE_RELEASED" | "DATE_RECORDED" | "DATE" => &mut tags.date,
                "GENRE" => &mut tags.genre,
                "COMMENT" | "DESCRIPTION" | "SUMMARY" | "SYNOPSIS" => &mut tags.comment,
                _ => continue,
            };
            fill(slot, Some(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An element with an 8-byte size (as muxers write for patchable sizes).
    fn el(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = id
            .to_be_bytes()
            .iter()
            .copied()
            .skip_while(|&b| b == 0)
            .collect();
        out.push(0x01);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(payload);
        out
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    fn simple(name: &str, value: &str) -> Vec<u8> {
        el(
            SIMPLE_TAG,
            &cat(&[
                el(TAG_NAME, name.as_bytes()),
                el(TAG_STRING, value.as_bytes()),
            ]),
        )
    }

    fn file(segment: &[u8]) -> Vec<u8> {
        cat(&[el(EBML, &el(0x4282, b"matroska")), el(SEGMENT, segment)])
    }

    #[test]
    fn sizes_and_ids() {
        assert_eq!(read_size(&mut &[0x81][..]), Some(1));
        assert_eq!(read_size(&mut &[0x40, 0x02][..]), Some(2));
        assert_eq!(
            read_size(&mut &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF][..]),
            Some(UNKNOWN_SIZE)
        );
        assert_eq!(read_id(&mut &[0x1A, 0x45, 0xDF, 0xA3][..]), Some(EBML));
        assert_eq!(read_id(&mut &[0x00][..]), None);
    }

    #[test]
    fn title_and_global_tags() {
        let info = el(INFO, &el(TITLE, "Мой фильм".as_bytes()));
        let global = el(
            TAG,
            &cat(&[
                el(TARGETS, &el(0x68CA, &[50])),
                simple("ARTIST", "Кто-то"),
                simple("DATE_RELEASED", "2019"),
            ]),
        );
        let per_track = el(
            TAG,
            &cat(&[el(TARGETS, &el(0x63C5, &[7])), simple("GENRE", "не то")]),
        );
        let data = file(&cat(&[
            info,
            el(TAGS, &cat(&[global, per_track])),
            el(CLUSTER, &[0; 16]),
        ]));

        let tags = read_matroska(&mut Cursor::new(data)).unwrap();
        assert_eq!(tags.title.as_deref(), Some("Мой фильм"));
        assert_eq!(tags.artist.as_deref(), Some("Кто-то"));
        assert_eq!(tags.date.as_deref(), Some("2019"));
        assert_eq!(tags.genre, None);
    }

    #[test]
    fn tags_after_clusters_found_via_seek_head() {
        let tags_el = el(TAGS, &el(TAG, &simple("TITLE", "В конце")));
        // SeekHead with a fixed-width position, filled in once the offset is known.
        let seek_head = |pos: u64| {
            el(
                SEEK_HEAD,
                &el(
                    SEEK,
                    &cat(&[
                        el(SEEK_ID, &TAGS.to_be_bytes()),
                        el(SEEK_POSITION, &pos.to_be_bytes()),
                    ]),
                ),
            )
        };
        let cluster = el(CLUSTER, &[0; 64]);
        let pos = (seek_head(0).len() + cluster.len()) as u64;
        let data = file(&cat(&[seek_head(pos), cluster, tags_el]));

        let tags = read_matroska(&mut Cursor::new(data)).unwrap();
        assert_eq!(tags.title.as_deref(), Some("В конце"));
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        cat(&[
            ((payload.len() + 8) as u32).to_be_bytes().to_vec(),
            kind.to_vec(),
            payload.to_vec(),
        ])
    }

    #[test]
    fn mp4_ilst_and_quicktime_atoms() {
        let data = |text: &str| {
            mp4_box(
                b"data",
                &cat(&[
                    1u32.to_be_bytes().to_vec(),
                    vec![0; 4],
                    text.as_bytes().to_vec(),
                ]),
            )
        };
        let ilst = mp4_box(
            b"ilst",
            &cat(&[
                mp4_box(b"\xA9nam", &data("Фильм")),
                mp4_box(b"\xA9day", &data("2015-04-16")),
            ]),
        );
        let meta = mp4_box(
            b"meta",
            &cat(&[vec![0; 4], mp4_box(b"hdlr", &[0; 25]), ilst]),
        );
        let qt = |text: &str| {
            cat(&[
                (text.len() as u16).to_be_bytes().to_vec(),
                vec![0x15, 0xC7],
                text.as_bytes().to_vec(),
            ])
        };
        let udta = mp4_box(
            b"udta",
            &cat(&[
                meta,
                mp4_box(b"\xA9ART", &qt("NIP")),
                mp4_box(b"\xA9des", &qt("Описание")),
            ]),
        );
        let trak = mp4_box(
            b"trak",
            &mp4_box(b"udta", &mp4_box(b"\xA9gen", &qt("не то"))),
        );
        let file = cat(&[
            mp4_box(b"ftyp", b"mp42"),
            mp4_box(b"mdat", &[0; 32]),
            mp4_box(b"moov", &cat(&[trak, udta])),
        ]);

        let tags = read_mp4(&mut Cursor::new(file)).unwrap();
        assert_eq!(tags.title.as_deref(), Some("Фильм"));
        assert_eq!(tags.date.as_deref(), Some("2015-04-16"));
        assert_eq!(tags.artist.as_deref(), Some("NIP"));
        assert_eq!(tags.comment.as_deref(), Some("Описание"));
        assert_eq!(tags.genre, None);
    }

    #[test]
    fn year_from_date() {
        let year = |d: &str| {
            VideoTags {
                date: Some(d.into()),
                ..Default::default()
            }
            .year()
        };
        assert_eq!(year("2019"), Some(2019));
        assert_eq!(year("2023-02-17T02:42:42+10:00"), Some(2023));
        assert_eq!(year("2022:12:31 03:00:00"), Some(2022));
        assert_eq!(year("20190710"), None);
        assert_eq!(year("весна"), None);
    }

    #[test]
    fn not_matroska() {
        assert_eq!(
            read_matroska(&mut Cursor::new(b"RIFF....AVI ".to_vec())),
            None
        );
        assert_eq!(read_matroska(&mut Cursor::new(Vec::new())), None);
        assert_eq!(read_mp4(&mut Cursor::new(b"RIFF....AVI ".to_vec())), None);
    }
}
