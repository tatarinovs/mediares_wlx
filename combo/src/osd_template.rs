//! OSD templates: `{field}` is replaced by its value, `<...>` is dropped when a field inside is
//! empty, `{{ }} << >>` stand for the characters themselves. Line breaks are kept.

use std::path::Path;

use mediares_core::exif::{parse_exif_datetime, ExifInfo};
use mediares_core::video_frame::VideoMeta;
use mediares_core::video_tags::VideoTags;

use crate::i18n::{n, tr};

/// Photo OSD as it was before templates.
pub const DEFAULT_PHOTO: &str = "{name} ( {width} x {height} = {mp} MP , {size_kb} KB )< [ {index} / {count} ]><  {zoom}%><  [{preview}]>";
/// Video OSD as it was before templates.
pub const DEFAULT_VIDEO: &str =
    "{name} ( {width} x {height} , {size} )< [ {index} / {count} ]>   {time} / {duration}";

/// A field offered by the editor's "Add field" menu, with its (English) label.
pub struct Field {
    pub key: &'static str,
    label: &'static str,
}

impl Field {
    pub fn label(&self) -> &'static str {
        tr(self.label)
    }
}

const fn f(key: &'static str, label: &'static str) -> Field {
    Field { key, label }
}

/// Menu groups: (submenu title, fields); titles go through [`tr`].
pub type FieldGroups = &'static [(&'static str, &'static [Field])];

const FILE_FIELDS: &[Field] = &[
    f("name", n("File name")),
    f("stem", n("Name without extension")),
    f("ext", n("Extension")),
    f("folder", n("Folder")),
    f("path", n("Full path")),
    f("size", n("Size (812 KB, 45.3 MB)")),
    f("size_kb", n("Size in KB")),
    f("index", n("Position in the list")),
    f("count", n("Files in the list")),
];

pub const PHOTO_FIELDS: FieldGroups = &[
    (n("File"), FILE_FIELDS),
    (
        n("Image"),
        &[
            f("width", n("Width")),
            f("height", n("Height")),
            f("mp", n("Megapixels")),
            f("zoom", n("Zoom, %")),
            f("preview", n("\"RAW preview\" for an embedded preview")),
        ],
    ),
    (
        n("EXIF"),
        &[
            f("camera", n("Camera (make and model)")),
            f("make", n("Make")),
            f("model", n("Model")),
            f("lens", n("Lens")),
            f("taken", n("Date and time taken")),
            f("date", n("Date taken")),
            f("time", n("Time taken")),
            f("exposure", n("Exposure (1/250 s)")),
            f("aperture", n("Aperture (f/2.8)")),
            f("iso", n("ISO")),
            f("focal", n("Focal length")),
            f("focal35", n("Focal length, 35 mm equiv.")),
            f("flash", n("\"flash\" if it fired")),
            f("software", n("Software")),
            f("gps", n("GPS coordinates")),
        ],
    ),
];

pub const VIDEO_FIELDS: FieldGroups = &[
    (n("File"), FILE_FIELDS),
    (
        n("Playback"),
        &[
            f("time", n("Current position")),
            f("duration", n("Duration")),
        ],
    ),
    (
        n("Video and audio"),
        &[
            f("width", n("Width")),
            f("height", n("Height")),
            f("fps", n("Frames per second")),
            f("codec", n("Video codec")),
            f("bitrate", n("Bitrate, kbps")),
            f("audio_codec", n("Audio codec")),
            f("channels", n("Audio channels")),
            f("sample_rate", n("Sample rate, Hz")),
        ],
    ),
    (
        n("Tags"),
        &[
            f("title", n("Title")),
            f("artist", n("Artist")),
            f("director", n("Director")),
            f("year", n("Date / year")),
            f("genre", n("Genre")),
            f("comment", n("Comment")),
        ],
    ),
];

/// Fields every file has. `position`: (index from 0, count), `None` outside a list.
/// Outer `None`: not a file field.
pub fn file_field(
    path: &Path,
    size: u64,
    position: Option<(usize, usize)>,
    key: &str,
) -> Option<String> {
    let os = |s: Option<&std::ffi::OsStr>| {
        s.map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Some(match key {
        "name" => os(path.file_name()),
        "stem" => os(path.file_stem()),
        "ext" => os(path.extension()),
        "folder" => os(path.parent().and_then(Path::file_name)),
        "path" => path.to_string_lossy().into_owned(),
        "size" => crate::image_view::format_size(size),
        "size_kb" => crate::image_view::format_thousands(size.div_ceil(1024)),
        "index" => position
            .map(|(i, _)| (i + 1).to_string())
            .unwrap_or_default(),
        "count" => position.map(|(_, n)| n.to_string()).unwrap_or_default(),
        _ => return None,
    })
}

/// EXIF fields of a photo; `""` when the file lacks the value. `None`: not an EXIF field.
pub fn exif_field(exif: Option<&ExifInfo>, key: &str) -> Option<String> {
    let none;
    let e = match exif {
        Some(e) => e,
        None => {
            none = ExifInfo::default();
            &none
        }
    };
    let taken = e.taken().and_then(parse_exif_datetime);
    let text = |v: &Option<String>| v.as_deref().map(str::trim).unwrap_or_default().to_string();
    Some(match key {
        "camera" => camera(e.make.as_deref(), e.model.as_deref()),
        "make" => text(&e.make),
        "model" => text(&e.model),
        "lens" => text(&e.lens_model),
        "taken" => taken
            .map(|t| {
                format!(
                    "{:02}.{:02}.{} {:02}:{:02}",
                    t.day, t.month, t.year, t.hour, t.minute
                )
            })
            .unwrap_or_default(),
        "date" => taken
            .map(|t| format!("{:02}.{:02}.{}", t.day, t.month, t.year))
            .unwrap_or_default(),
        "time" => taken
            .map(|t| format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second))
            .unwrap_or_default(),
        "exposure" => e
            .exposure_time
            .as_ref()
            .map(|t| format!("{} {}", t, tr("s")))
            .unwrap_or_default(),
        "aperture" => e
            .f_number
            .map(|f| format!("f/{}", trim_float(f, 1)))
            .unwrap_or_default(),
        "iso" => e.iso.map(|v| v.to_string()).unwrap_or_default(),
        "focal" => e
            .focal_length
            .map(|f| format!("{} {}", trim_float(f, 1), tr("mm")))
            .unwrap_or_default(),
        "focal35" => e
            .focal_length_35mm
            .map(|f| format!("{} {}", f, tr("mm")))
            .unwrap_or_default(),
        "flash" => {
            if e.flash_fired == Some(true) {
                tr("flash").to_string()
            } else {
                String::new()
            }
        }
        "software" => text(&e.software),
        "gps" => e
            .gps()
            .map(|(la, lo)| format!("{:.5}, {:.5}", la, lo))
            .unwrap_or_default(),
        _ => return None,
    })
}

/// Video fields that need the file's tags (read on demand).
pub const VIDEO_TAG_KEYS: &[&str] = &["title", "artist", "director", "year", "genre", "comment"];

/// Stream and tag fields of a video; `""` when unknown. `None`: not such a field.
pub fn video_field(meta: Option<&VideoMeta>, tags: &VideoTags, key: &str) -> Option<String> {
    let text = |v: &Option<String>| v.clone().unwrap_or_default();
    let num = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_default();
    Some(match key {
        "codec" => meta.map(|m| text(&m.codec)).unwrap_or_default(),
        "bitrate" => num(meta.and_then(|m| m.bitrate_kbps)),
        "audio_codec" => meta.map(|m| text(&m.audio_codec)).unwrap_or_default(),
        "channels" => meta
            .and_then(|m| m.audio_channels)
            .map(channels)
            .unwrap_or_default(),
        "sample_rate" => num(meta.and_then(|m| m.audio_sample_rate)),
        "title" => text(&tags.title),
        "artist" => text(&tags.artist),
        "director" => text(&tags.director),
        "year" => tags.date.as_deref().map(format_date).unwrap_or_default(),
        "genre" => text(&tags.genre),
        "comment" => text(&tags.comment),
        _ => return None,
    })
}

/// A tag date for display: "2019", "16.04.2015", "17.02.2023 02:42" (fractions and time zone
/// dropped); anything unrecognised as written.
fn format_date(raw: &str) -> String {
    let raw = raw.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (date, time) = raw.split_once(['T', ' ']).unwrap_or((raw, ""));
    let parts: Vec<&str> = date.split(['-', ':', '.']).collect();
    let hm: Vec<&str> = time.split(':').take(2).collect();
    let has_time =
        hm.len() == 2 && hm.iter().all(|p| digits(p) && p.len() == 2) && time != "00:00:00";
    let time = if has_time {
        format!(" {}:{}", hm[0], hm[1])
    } else {
        String::new()
    };
    match parts.as_slice() {
        [y] if y.len() == 4 && digits(y) => y.to_string(),
        [y, m, d]
            if y.len() == 4
                && [y, m, d].iter().all(|p| digits(p))
                && m.len() <= 2
                && d.len() <= 2 =>
        {
            format!("{:0>2}.{:0>2}.{}{}", d, m, y, time)
        }
        _ => raw.to_string(),
    }
}

pub fn channels(n: u32) -> String {
    match n {
        1 => tr("mono").into(),
        2 => tr("stereo").into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => n.to_string(),
    }
}

/// Frames per second: "25", "29.97".
pub fn format_fps(fps: f64) -> String {
    if fps > 0.0 {
        trim_float(fps, 2)
    } else {
        String::new()
    }
}

/// "Canon EOS R5", not "Canon Canon EOS R5" (many cameras repeat the make in the model).
fn camera(make: Option<&str>, model: Option<&str>) -> String {
    let (make, model) = (make.unwrap_or("").trim(), model.unwrap_or("").trim());
    let brand = make.split_whitespace().next().unwrap_or("");
    if make.is_empty() || model.to_lowercase().starts_with(&brand.to_lowercase()) {
        model.to_string()
    } else if model.is_empty() {
        make.to_string()
    } else {
        format!("{} {}", make, model)
    }
}

/// "2.8", "50" — no trailing zeros.
fn trim_float(v: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, v);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[derive(Debug, PartialEq)]
enum Node {
    Text(String),
    Field(String),
    /// Hidden when a field inside is empty.
    Block(Vec<Node>),
}

/// Parses until the end or, inside a block, its closing `>`.
fn parse(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, in_block: bool) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, nodes: &mut Vec<Node>| {
        if !text.is_empty() {
            nodes.push(Node::Text(std::mem::take(text)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '{' | '<' | '>' | '}' if chars.peek() == Some(&c) => {
                chars.next();
                text.push(c);
            }
            '{' => {
                let mut key = String::new();
                let mut closed = false;
                while let Some(&k) = chars.peek() {
                    if k == '}' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    if !(k.is_ascii_alphanumeric() || k == '_') {
                        break;
                    }
                    key.push(k);
                    chars.next();
                }
                if closed && !key.is_empty() {
                    flush(&mut text, &mut nodes);
                    nodes.push(Node::Field(key));
                } else {
                    // Not a field: keep what was typed.
                    text.push('{');
                    text.push_str(&key);
                    if closed {
                        text.push('}');
                    }
                }
            }
            '<' => {
                flush(&mut text, &mut nodes);
                nodes.push(Node::Block(parse(chars, true)));
            }
            '>' if in_block => break,
            _ => text.push(c),
        }
    }
    flush(&mut text, &mut nodes);
    nodes
}

/// Appends the rendered `nodes`; false if a field among them (outside nested blocks) was empty.
fn render_nodes(nodes: &[Node], lookup: &dyn Fn(&str) -> Option<String>, out: &mut String) -> bool {
    let mut complete = true;
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Field(key) => match lookup(key) {
                Some(value) => {
                    complete &= !value.is_empty();
                    out.push_str(&value);
                }
                // A typo stays visible instead of silently disappearing.
                None => {
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            },
            Node::Block(inner) => {
                let mut block = String::new();
                if render_nodes(inner, lookup, &mut block) {
                    out.push_str(&block);
                }
            }
        }
    }
    complete
}

/// A template parsed once and rendered many times (the video OSD, on every frame).
pub struct Template(Vec<Node>);

impl Template {
    pub fn parse(template: &str) -> Self {
        Template(parse(&mut template.chars().peekable(), false))
    }

    /// `lookup(key)`: the value (`""` if the file has none), `None` for an unknown key.
    pub fn render(&self, lookup: impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        render_nodes(&self.0, &lookup, &mut out);
        out.trim_end().to_string()
    }
}

/// Fills `template` (see [`Template::render`]).
pub fn render(template: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    Template::parse(template).render(lookup)
}

/// Whether `template` uses any of `keys`.
pub fn uses_any(template: &str, keys: &[&str]) -> bool {
    fn walk(nodes: &[Node], keys: &[&str]) -> bool {
        nodes.iter().any(|n| match n {
            Node::Field(k) => keys.contains(&k.as_str()),
            Node::Block(inner) => walk(inner, keys),
            Node::Text(_) => false,
        })
    }
    walk(&parse(&mut template.chars().peekable(), false), keys)
}

/// One INI line: `\` → `\\`, line break → `\n`.
pub fn to_ini(template: &str) -> String {
    template
        .replace('\r', "")
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
}

pub fn from_ini(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(key: &str) -> Option<String> {
        match key {
            "name" => Some("a.jpg".into()),
            "iso" => Some("400".into()),
            "lens" | "index" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn fields_and_blocks() {
        assert_eq!(
            render("{name}< ISO {iso}>< {lens} мм>", lookup),
            "a.jpg ISO 400"
        );
        assert_eq!(render("<[{index}] >{name}", lookup), "a.jpg");
    }

    #[test]
    fn nested_block_hides_alone() {
        assert_eq!(render("<{name}< {lens}>!>", lookup), "a.jpg!");
    }

    #[test]
    fn escapes_and_unknown_fields() {
        assert_eq!(render("{{name}} <<{name}>>", lookup), "{name} <a.jpg>");
        assert_eq!(
            render("{nope} {not a field} {", lookup),
            "{nope} {not a field} {"
        );
        assert_eq!(render("{name} > 1", lookup), "a.jpg > 1");
    }

    #[test]
    fn line_breaks_kept() {
        assert_eq!(render("{name}\nISO {iso}\n", lookup), "a.jpg\nISO 400");
    }

    #[test]
    fn detects_used_keys() {
        assert!(uses_any("<{codec}>", &["codec", "fps"]));
        assert!(!uses_any("{name} {{codec}}", &["codec"]));
    }

    #[test]
    fn ini_round_trip() {
        let t = "{name}\r\nC:\\dir\\n";
        assert_eq!(to_ini(t), "{name}\\nC:\\\\dir\\\\n");
        assert_eq!(from_ini(&to_ini(t)), "{name}\nC:\\dir\\n");
    }

    #[test]
    fn exif_values() {
        let e = ExifInfo {
            make: Some("SONY".into()),
            model: Some("ILCE-7RM4".into()),
            f_number: Some(2.8),
            focal_length: Some(50.0),
            date_time_original: Some("2024:09:02 14:33:10".into()),
            ..Default::default()
        };
        assert_eq!(exif_field(Some(&e), "camera").unwrap(), "SONY ILCE-7RM4");
        assert_eq!(exif_field(Some(&e), "aperture").unwrap(), "f/2.8");
        assert_eq!(exif_field(Some(&e), "focal").unwrap(), "50 мм");
        assert_eq!(exif_field(Some(&e), "taken").unwrap(), "02.09.2024 14:33");
        assert_eq!(exif_field(Some(&e), "iso").unwrap(), "");
        assert_eq!(exif_field(None, "lens").unwrap(), "");
        assert_eq!(exif_field(None, "name"), None);
        assert_eq!(camera(Some("Canon"), Some("Canon EOS R5")), "Canon EOS R5");
        assert_eq!(
            camera(Some("NIKON CORPORATION"), Some("NIKON D750")),
            "NIKON D750"
        );
    }

    #[test]
    fn video_values() {
        let meta = VideoMeta {
            codec: Some("HEVC".into()),
            audio_channels: Some(2),
            ..Default::default()
        };
        let tags = VideoTags {
            title: Some("Фильм".into()),
            ..Default::default()
        };
        assert_eq!(video_field(Some(&meta), &tags, "codec").unwrap(), "HEVC");
        assert_eq!(
            video_field(Some(&meta), &tags, "channels").unwrap(),
            "стерео"
        );
        assert_eq!(video_field(None, &tags, "title").unwrap(), "Фильм");
        assert_eq!(video_field(None, &tags, "bitrate").unwrap(), "");
        assert_eq!(video_field(None, &tags, "name"), None);
        assert_eq!(format_date("2019"), "2019");
        assert_eq!(format_date("2015-04-16"), "16.04.2015");
        assert_eq!(
            format_date("2023-02-17T02:42:42.2592209+10:00"),
            "17.02.2023 02:42"
        );
        assert_eq!(format_date("2022:12:31 03:00:00"), "31.12.2022 03:00");
        assert_eq!(format_date("весна 2020"), "весна 2020");
        assert_eq!(format_fps(29.97), "29.97");
        assert_eq!(format_fps(25.0), "25");
        assert_eq!(format_fps(0.0), "");
    }

    #[test]
    fn file_values() {
        let p = Path::new(r"D:\фотки\2024\DSC01.JPG");
        assert_eq!(file_field(p, 0, None, "folder").unwrap(), "2024");
        assert_eq!(file_field(p, 0, None, "stem").unwrap(), "DSC01");
        assert_eq!(file_field(p, 0, Some((2, 10)), "index").unwrap(), "3");
        assert_eq!(file_field(p, 0, None, "count").unwrap(), "");
    }

    #[test]
    fn defaults_parse() {
        let all = |_: &str| Some("x".to_string());
        assert!(!render(DEFAULT_PHOTO, all).contains('{'));
        assert!(!render(DEFAULT_VIDEO, all).contains('{'));
    }
}
