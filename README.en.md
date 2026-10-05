# Mediares — Total Commander Multimedia Viewer and Content Plugin (WLX + WDX)

[Русский](README.md) | **English**

[![Rust](https://img.shields.io/badge/Rust-stable-DE6E39?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Windows%20x64-0078D6?logo=windows&logoColor=white)](https://www.microsoft.com/windows)
[![Total Commander](https://img.shields.io/badge/Total%20Commander-WDX%20%2B%20WLX-1f6feb)](https://www.ghisler.com/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/tatarinovs/mediares_wlx)](https://github.com/tatarinovs/mediares_wlx/releases/latest)
[![Last commit](https://img.shields.io/github/last-commit/tatarinovs/mediares_wlx)](https://github.com/tatarinovs/mediares_wlx/commits)
[![Repo size](https://img.shields.io/github/repo-size/tatarinovs/mediares_wlx)](https://github.com/tatarinovs/mediares_wlx)

A high-performance 64-bit plugin for **Total Commander**, written in **Rust**. Ready-to-use archives are on the
[Releases](https://github.com/tatarinovs/mediares_wlx/releases/latest) page.

1. **`mediares_wlx`** — a 2-in-1 plugin (WLX + WDX) in one file: all the WDX fields (see #2) **plus** a fast built-in viewer for **images, audio and video** on **F3** (Lister) with GDI double buffering and a dark UI.
2. **`mediares_wdx`** — a lightweight content plugin (WDX), no viewer: EXIF, tag and stream-property fields for columns, search and multi-rename (ready-made sets are [below](#ready-made-column-sets)), plus perceptual hashes (dHash, pHash, CoarseHash, Video/Audio Fingerprint) for duplicate search.
---

## Workspace layout

```
mediares_wlx/
├── Cargo.toml                  # Workspace root
├── core/                       # Shared library (lib), no Win32 windows:
│   ├── tc_api.rs               # WDX/WLX C ABI structs and constants
│   ├── ffi.rs                  # catch_unwind wrapper and string conversion at the TC boundary
│   ├── probe.rs                # Media type by extension + the single source of detect strings
│   ├── image_decode.rs         # Photo decoding (`image`, WIC, SVG + RAW/PSD previews) with memory limits
│   ├── wic_decode.rs           # HEIC/HEIF, AVIF, JPEG XL, JPEG XR, DDS via Windows codecs (WIC)
│   ├── svg.rs                  # SVG/SVGZ via Direct2D (ID2D1SvgDocument)
│   ├── svg_css.rs              # Copies <style> rules into style attributes (Direct2D ignores style sheets)
│   ├── heif.rs                 # EXIF from the HEIF container (HEIC/AVIF): the Exif item via iinf/iloc
│   ├── exif.rs                 # EXIF parsing (JPEG, TIFF/RAW, HEIF) and writing core fields for "Save as"
│   ├── orientation.rs          # Lossless JPEG rotation: writing the EXIF Orientation tag
│   ├── jpeg.rs                 # Finding/validating embedded JPEG streams, replacing the EXIF segment
│   ├── raw_preview.rs          # Embedded JPEG from RAW (IFD/SubIFD/strip + signature fallback)
│   ├── psd_preview.rs          # PSD/PSB composite (8/16-bit) or the 0x0409/0x0410 thumbnail
│   ├── hashing.rs              # dHash (64-bit), pHash (DCT 64-bit), CoarseHash (32-bit), aspect ratio
│   ├── mf_init.rs              # COM / Media Foundation lifetime
│   ├── video_frame.rs          # Frames at 25/50/75% via IMFSourceReader and the fingerprint
│   ├── audio_decode.rs         # Audio decoding to PCM (symphonia), accurate seeking — the audio-decode feature
│   ├── audio_tags.rs           # Tags and cover art (symphonia: ID3v1/v2, APE, Vorbis, MP4, RIFF; cp1251 repair) — the tags feature
│   ├── audio_fingerprint.rs    # Audio duplicate-search fields: fingerprint, PCM hash, duration
│   ├── mf_audio.rs             # Audio via Media Foundation (WMA, AC3, Opus): PCM and stream properties
│   ├── cache.rs                # Thread-safe LRU cache of analysis and metadata (key: path + size + mtime)
│   ├── video_tags.rs           # Video tags: own parsing of Matroska/WebM (EBML) and MP4/MOV (ilst, QuickTime, 3GPP)
│   └── wdx_api.rs              # WDX field logic + the export_content_plugin! macro
├── wdx/                        # Lightweight WDX plugin (cdylib): one line — export_content_plugin!()
├── combo/                      # Full 2-in-1 WDX + WLX plugin (cdylib)
│   ├── window.rs               # Lister window, message handling, commands and hotkeys
│   ├── state.rs                # Per-HWND state, the folder's file list (natural sort)
│   ├── image_view.rs           # Zoom/pan/loupe, GDI double buffering, OSD
│   ├── smooth.rs               # Bicubic upscaling via Direct2D into the GDI buffer
│   ├── osd_template.rs         # OSD templates: fields, collapsible blocks, EXIF/video values
│   ├── osd_template_dialog.rs  # OSD template editor with a field menu
│   ├── image_cache.rs          # BGRA cache (LRU by bytes) + a pool of background decoders, 90° rotation
│   ├── file_actions.rs         # Recycle bin, external editor, Explorer, desktop wallpaper
│   ├── print.rs                # Printing a picture (ListPrint): printer dialog, fitting to margins
│   ├── snapshot.rs             # Clipboard, saving a frame (PNG/JPEG), TC thumbnails
│   ├── save_as.rs              # "Save as" for photos: copy, RAW preview with EXIF, or re-encode
│   ├── fullscreen.rs           # Fullscreen mode: a separate window on the Lister's monitor
│   ├── overlay.rs              # Floating panels in fullscreen mode, cursor hiding
│   ├── media_view.rs           # Shared audio/video layer: control panel, mouse, events, timers
│   ├── playback_video.rs       # IMFMediaEngine (frame-server) + D3D11 swap chain
│   ├── video_view.rs           # Video surface (letterbox), speed, frame stepping, events
│   ├── resume.rs               # Stop positions for long videos (mediares_resume.txt)
│   ├── playback_audio.rs       # Audio: symphonia decoder thread → WASAPI stream (shared mode, format conversion by Windows)
│   ├── audio_view.rs           # Cover art + tags; falls back to Media Foundation (WMA, Opus, AC3)
│   ├── playlist.rs             # Queue: auto-advance, repeat, shuffle, reading M3U
│   ├── transport_bar.rs        # Control panel: ⏮ play/pause ⏭, time, timeline, volume
│   ├── config.rs               # mediares.ini settings
│   ├── i18n.rs                 # UI language (Russian / English), tr("…", "…")
│   ├── dialog.rs               # Shared modal dialog helpers
│   ├── gdi.rs                  # Shared GDI helpers: double buffering, fonts, text, fills, DPI
│   ├── exif_dialog.rs          # EXIF metadata window
│   ├── settings_dialog.rs      # Settings window
│   └── examples/lister_harness.rs  # Dev harness: a host window like TC's, a scenario and screenshots
└── pluginst/                   # TC install scripts
    ├── pluginst-wdx.inf        # WDX-only archive
    └── pluginst-wlx.inf        # WLX archive: the combo file installs as a Lister plugin (fields connect from settings)
```

## Viewer (combo, F3)

### Photos

JPEG (JPG, JPEG, JPE; THM — Canon thumbnails), PNG, GIF, WEBP, BMP, ICO, TGA, HDR (Radiance); RAW (CR2, CR3,
CRW, NEF, ARW, ORF, RW2, DNG, RAF, PEF, RAW) — via the embedded JPEG preview; PSD/PSB — via the composite.

Through Windows codecs (WIC), adding nothing to the plugin size: **HEIC/HEIF/HIF** and **AVIF** — need the
Microsoft Store extensions "HEIF Image Extensions" plus "HEVC Video Extensions" (for HEIC) or "AV1 Video Extension"
(for AVIF); **JXL** — "JPEG XL Image Extension"; **JXR/WDP/HDP** (JPEG XR) and **DDS** (BC1–BC3, uncompressed) and
**TIFF/TIF** (any compression, old-style JPEG included) — built into Windows. The HEIF container's rotation (`irot`/`imir`) is applied and its EXIF (date, camera, GPS) is
read. A file the standard decoder fails on (e.g. an unusual BMP variant) gets a second try via WIC.

**SVG/SVGZ** is drawn by the Direct2D engine (Windows 10 1703+): shapes, paths, gradients, clipping, `use`, styles —
including `<style>` sheets with simple selectors (`.class`, `#id`, `tag`). `<text>`, filters and masks are not drawn.
Small drawings are rasterized at 1024 px so they stay sharp when fitted to the window.

OpenEXR is behind the `exr` cargo feature of `mediares_core` and off by default: it adds ≈0.5 MB to the plugin.

**Speed.** The window opens instantly (`ListLoad` ≈ 3 ms), the photo decodes in the background. While a new
frame is being prepared, the previous one stays on screen. Neighbouring photos (two ahead in the paging
direction and one behind) are pre-decoded by a pool of 1–3 threads and show instantly; the cache holds up to
512 MB. If a file's header is bad, `ListLoad` returns NULL and TC tries another plugin; if the file fails to
decode later, the window shows a message.

| Action | Keys / mouse |
|---|---|
| Next / previous photo | Space, →, ↓, PgDn, N, wheel / ←, ↑, PgUp, Backspace, P; mouse side buttons |
| Zoom | + / −, Ctrl+wheel (towards the cursor; steps stop at 100%); Ctrl+1 — 100%, Ctrl+2 — 200%, Ctrl+3 — 300%; Ctrl+0, *, / — fit to window |
| Pan / loupe | drag while zoomed in / hold left mouse button in fit mode |
| Rotate left / right (view only, the file is not changed) | L / R |
| Write the rotation into the JPEG losslessly | Ctrl+R (or the menu) |
| Fullscreen | Enter, F, F11, double-click; Esc to exit |
| OSD on/off / slideshow | O or I / F5 |
| Delete to recycle bin and show the next one | Del (confirmation can be turned off in settings) |
| EXIF / settings | E / S |
| Where the photo was taken, on OpenStreetMap (photos with GPS) | G (or the menu; a button in the EXIF window) |
| Copy the picture | Ctrl+C |
| Save as JPEG / PNG | Ctrl+S (or the menu) |
| Print | Ctrl+P (or the menu) |
| Open in editor / show in folder | F4 / Ctrl+Enter |

The right-click menu also has: **open in external editor**, **show in folder**, **set as desktop wallpaper**
(JPEG/PNG/BMP — the file itself; RAW, PSD, rotated ones — saved to `mediares_wallpaper.png`).
The editor is set separately for photos, video and audio in settings: a path to the program (quoted or not)
and, if needed, arguments, e.g. `"C:\Program Files\App\app.exe" -n` or `code -n --wait %1`; `%1` is where the
file goes, and without it the path is appended at the end. If the field is empty, Windows' "Edit" program for
that file type is used, or the default program if there is none. Fullscreen mode turns off before launching
the editor or Explorer, so its window doesn't end up underneath.

**Save as** — JPEG (quality 92) or PNG next to the source. Lossless where possible: a file of the same format
is copied, and RAW → JPEG is the embedded RAW preview as is, with EXIF from the RAW (date, camera, lens,
exposure, GPS, orientation). Otherwise the file is re-decoded at full resolution, rotated as shown on screen
(L / R) and encoded with the core EXIF fields; transparency in JPEG is flattened onto white.

**Background color** around the photo (and under PNG/GIF transparency) is picked in settings; the palette has
black, dark, mid-gray, light and white presets. Also there — **"Don't upscale small images"**: in fit mode a
picture smaller than the window is shown at 100% instead of being enlarged.

**Smoothing when enlarging** (on by default): an enlarged photo and audio cover art are drawn bicubically via
Direct2D straight into the GDI buffer, instead of square pixels. In fit mode — at any scale; with manual zoom
and the loupe — up to 400%, beyond that real pixels are shown to judge sharpness. Shrinking still uses GDI
HALFTONE, as before. If Direct2D is unavailable, the picture is drawn with plain GDI.

### Video and audio

**Video** — MP4, MKV, AVI, MOV/QT, WMV/ASF, WEBM, M4V, 3GP/3G2, FLV, TS, MTS, MPG/MPEG, VOB (including DVD rips
in MPEG-2 PS), decoded via the system's Media Foundation codecs.

**Audio** — MP3, MP2, FLAC, WAV, OGG/OGA (Vorbis), M4A/M4B (AAC, ALAC), AAC, AIFF/AIFC, CAF, MKA decode in pure
Rust (`symphonia`, output goes straight to WASAPI) and don't depend on installed codecs. WMA, Opus and AC3,
which symphonia doesn't have, play through Media Foundation. Shown: cover art (embedded, or
`cover`/`folder`/`front.jpg` from the folder), title, artist, album, year and stream properties (`symphonia`).

If a file can't be opened, `ListLoad` returns NULL and TC tries another plugin.

**Playlist** — either the current folder's files (photos are skipped) or the entries of an open
`.m3u`/`.m3u8`. The next file starts automatically when one ends (auto-advance can be turned off); there's
repeat-list or repeat-one and shuffle — in the right-click menu and in settings.

| Action | Keys / mouse |
|---|---|
| Pause / play | Space, K, click on the video / cover art, media key |
| Seek ±5 s (step in settings) | ← / →, click or drag on the timeline |
| Next / previous key frame (audio: ±1 s) | ↑ / ↓ |
| Video speed 0.25×–2× / normal | [ / ] / \ |
| Frame back / forward (pauses; hold for several frames). In MPG/MPEG/VOB — forward only: their Media Foundation source can't seek precisely | , / . |
| Volume | + / −, mouse wheel over the control panel or Ctrl+wheel, the slider; M — mute |
| Next / previous track (audio/video only) | ⏭ / ⏮ buttons on the panel, media keys |
| Next / previous file | N / P, PgDn / PgUp, Backspace |
| Fullscreen | Enter, F, F11, double-click; Esc to exit |
| Copy the photo / video frame / cover art to the clipboard (pastes as both a picture and a file) | Ctrl+C |
| OSD (name, resolution, size, position; time for video) on/off for the current type | O, I |
| Save the current video frame next to the file (`name_1-23.456.png`; PNG or JPEG — in settings) | Shift+S |
| Print the current frame / cover art | Ctrl+P |
| Open in editor / show in folder | F4 / Ctrl+Enter |
| Delete to recycle bin | Del |

**Resume playback.** Videos longer than 5 minutes reopen where you left them (the panel shows "Resuming from
12:34"); a stop within the first or last 30 s isn't remembered. Can be turned off in settings.

**Fullscreen mode.** The picture fills the whole monitor; the cursor hides after 2.5 s of inactivity.
- Video: the control panel sits semi-transparent over the frame (or, if disabled, underneath it).
- Photos: a ⏮ ⏯ ⏭ bar at the bottom center — previous photo, slideshow, next.
- Audio: the panel is docked at the bottom, as in the window.

With "Hide panel when idle", the panel only shows when the mouse nears the bottom edge of the screen, and
hides again after 2.5 s. All of this is in settings, under "Fullscreen mode".

**OSD** is set with a drop-down: off / on photos / on video / on both. For video the line is drawn right into
the frame.

OSD content is set by a template — the "For photos..." / "For video..." buttons in settings. The "Add
field..." button inserts a field from a menu (file, image, EXIF; for video — stream, audio, MKV/MP4 tags):

```
{name}< [ {index} / {count} ]>
<{camera}  >< {exposure}>< {aperture}>< ISO {iso}>< {focal}>
```

- `{field}` — a value; an unknown field is shown as is, so a typo is visible.
- `<...>` — the whole block disappears if it contains an empty field (e.g. ISO for a photo without EXIF).
- `{{ }} << >>` — the literal characters; a line break in the template is a new OSD line.

EXIF comes from the same read needed for auto-rotation, so paging isn't slowed down. Video codec, bitrate and
tags are read once per file, and only if the template uses them. In `mediares.ini` templates are stored in
`PhotoOSDTemplate` / `VideoOSDTemplate` (line break — `\n`); an empty value means the default template.

**Slideshow:** F5, the ⏯ button on the bar, or the right-click menu. Only the folder's photos are shown, in a
loop (audio and video are skipped); the interval is set in settings (4 s by default).

**TC thumbnails** (thumbnail view in panels): the plugin provides `ListGetPreviewBitmap` — a photo, a frame at
10% of a video's duration, or an audio cover (embedded, or `cover.jpg` from the folder).

## WDX fields (both builds)

Fields appear in TC as the `mediares` group, in this order: duplicate-search hashes first, then fields by
content type — `Media_Type`, photo, video, audio; `Plugin_Version` last. All fields are available for columns,
tooltips, search and multi-rename.

**Delayed fields.** Fields that need RAW/PSD decoding, video, audio, or a slow open via Media Foundation
return `FT_DELAYED`: TC fills them in in the background, without holding up panel browsing. Plain photos (JPG,
PNG...) decode fast and are computed right away. The result is cached (key: path + size + modification time),
so several columns of the same file are computed once.

### Duplicate-search hashes

TC's duplicate search compares fields for **exact equality** — there's no fuzzy comparison (Hamming distance).
So a hash is only as useful as how often a copy produces the exact same value while different files produce a
different one.

| Field | How it's computed | What it finds |
|---|---|---|
| `Image_pHash` | 64 bits: DCT of a 32×32 picture, sign of the 8×8 low frequencies relative to the median | Re-saved and downscaled copies. The most reliable for photos: no false matches were seen in testing |
| `Image_dHash` | 64 bits: a 9×8 picture, comparing brightness of horizontally adjacent pixels | Same idea, a bit more sensitive to heavy compression and downscaling |
| `Image_CoarseHash` | 32 bits: the same as dHash, on a 5×8 grid | The most tolerant: catches copies with contrast edits, minor cropping, camera RAW ↔ JPEG previews. The cost is occasional matches between adjacent burst frames |
| `Video_Fingerprint` | `<seconds>s_<h25>_<h50>_<h75>`: duration and the dHash of frames at 25/50/75% | Exact copies and repackaging into another container without re-encoding |
| `Video_dHash_Mid` | dHash of the frame at 50% | Softer than the fingerprint: doesn't depend on duration or the other two frames |
| `Audio_Fingerprint` | `<seconds>s_<h25>_<h50>_<h75>`: spectral hashes of three 30-second windows at 25/50/75% of the audible part, 18 bits | Re-encoded copies: a different bitrate, codec (MP3/AAC/Vorbis/FLAC), sample rate, mono, volume |
| `Audio_PCM_Hash` | Hash of the decoded samples with silence trimmed off the ends | The same sound in another container or with different tags (WAV ↔ FLAC of the same bit depth, a re-tagged MP3) |

**Photos.** Hashes are computed on the file's pixels as stored: EXIF rotation is not applied, so a photo
rotated by its tag and its "really" rotated copy won't match. RAW is hashed from its embedded JPEG preview,
PSD from the composite. Share of copies whose hash matched the original exactly (30 ARW+JPG pairs from a Sony
A7 IV; false matches — 37 consecutive frames of one shoot):

| Change made to the copy | dHash | pHash | CoarseHash |
|---|---|---|---|
| Re-saved as JPEG, quality 75 | 93% | 100% | 93% |
| Re-saved as JPEG, quality 40 | 70% | 90% | 80% |
| Downscaled by half | 90% | 93% | 97% |
| Downscaled to 1600 px | 83% | 90% | 93% |
| Brightness +12 | 97% | 97% | 100% |
| Contrast +15% | 30% | 27% | 53% |
| Cropped 2% at the edges | 10% | 0% | 33% |
| Rotated 90°, mirrored | ≤3% | 0% | ≤3% |
| RAW preview ↔ JPEG shot by the camera side by side | 29–57% | 26–70% | 53–80% |
| *Adjacent burst frames (false match)* | *0%* | *0%* | *3%* |

In practice: for copies that went through messengers, cloud storage and resizing — `Image_pHash`; for "the same
shot, differently processed" — `Image_CoarseHash`, eyeballing the results. Cropped and rotated copies aren't
found by exact comparison with any hash.

**Video.** Media Foundation seeks to the key frame before the requested point, and that's what gets hashed. A
copy repackaged without re-encoding has the same key frames, and the fingerprint matches; a re-encoded one
usually has different key frame placement, so don't expect a match. `Video_Fingerprint` also requires the
duration to match to the second.

**Audio.** The fingerprint is only 18 bits, tuned on re-encoded copies of real tracks: ~99% of copies give
exactly the same value as the original; no false matches were seen on a sample of ~400 tracks. A different
master, a live version, or a recording trimmed by a few seconds won't match. Analysis decodes the whole file
(~0.4 s for a 3–5 minute track). `Audio_PCM_Hash` is stable across lossy formats within one plugin version.
Files that `symphonia` can't open (WMA, Opus, AC3, and anything for which the system has a decoder) are
decoded via Media Foundation (`IMFSourceReader`); their `Audio_PCM_Hash` depends on the installed decoders —
stable on one machine, but may differ on another.

### General

| Field | Type | Value |
|---|---|---|
| `Media_Type` | string | `Image`, `Video` or `Audio` — by extension, without reading the file |
| `Plugin_Version` | string | Plugin version |

### Photos

EXIF is read from JPEG, TIFF and TIFF-like RAW (CR2, NEF, ARW, DNG, ORF, RW2, PEF). Only the metadata block is
read, so these fields are never delayed.

| Field | Type | Value |
|---|---|---|
| `Image_Width`, `Image_Height` | number | Size in pixels, EXIF rotation not applied. For JPG/PNG/... — from the header; for RAW — from EXIF (`PixelXDimension`/`PixelYDimension`: the embedded preview can be downscaled — 1616x1080 for a 6000x4000 Sony ARW shot), without it and for PSD — the preview or composite size from analysis (delayed) |
| `Image_Dimensions` | string | `WxH`, the source of the width and height above |
| `Image_AspectRatio` | string | Ratio: `3:2`, `16:9`, `4:3`, `1:1`, `21:9`, otherwise a reduced fraction up to 20 or `1.85:1` (from the same size) |
| `Photo_Make`, `Photo_Model`, `Photo_Lens` | string | Camera make, model, lens |
| `Photo_Date_Taken` | date/time | `DateTimeOriginal` (or `DateTime` if absent), in whatever time the camera's clock was set to |
| `Photo_Exposure` | string | Exposure time: `1/250`, `2.5` |
| `Photo_FNumber` | float | Aperture (2.8) |
| `Photo_ISO` | number | ISO sensitivity |
| `Photo_Focal_Length_mm` / `Photo_Focal_Length_35mm` | float / number | Focal length and its 35 mm equivalent |
| `Photo_Flash` | yes/no | Whether the flash fired |
| `Photo_Orientation` | number | EXIF orientation code 1–8 |
| `Photo_Software` | string | Program that saved the file |
| `Photo_GPS_Latitude`, `Photo_GPS_Longitude` | float | Coordinates in degrees (south/west are negative) |
| `Photo_Has_GPS` | yes/no | Whether there are coordinates (photos without EXIF — "no") |

### Video

Stream properties come from Media Foundation, without decoding frames. Opening the file still takes a
noticeable amount of time, so these fields are delayed.

| Field | Type | Value |
|---|---|---|
| `Video_Duration_Sec` | number | Duration rounded to whole seconds, as in `Video_Length` (from analysis; `Video_Fingerprint` drops the fraction) |
| `Video_Length` | time | Duration, h:mm:ss |
| `Video_Width`, `Video_Height` | number | Frame size |
| `Video_Dimensions` | string | `WxH` from analysis |
| `Video_Frame_Rate` | float | Frames per second (29.97, 25) |
| `Video_Codec` | string | H.264, HEVC, AV1, VP9, MPEG-4, MPEG-2, VC-1... or FOURCC |
| `Video_Bitrate_kbps` | number | Overall file bitrate (size / duration) |
| `Video_Audio_Codec` | string | AAC, AC-3, E-AC-3, MP3, DTS, Opus, FLAC...; empty if there's no audio |
| `Video_Audio_Channels`, `Video_Audio_Sample_Rate_Hz` | number | Channels and sample rate of the first audio track |

Tags — MKV/WebM (the title from the header and whole-file tags; per-track tags, such as mkvmerge statistics,
are skipped) and MP4/MOV (iTunes `ilst`, QuickTime `©xxx` atoms, 3GPP). Only headers are read, but for MP4
they're often at the end of the file: on a hard drive that's a head seek per file, so these fields are delayed
until the file's tags are read; after that they're served from the cache right away.

| Field | Type | Value |
|---|---|---|
| `Video_Title` | string | Movie title |
| `Video_Artist`, `Video_Director` | string | Artist / author, director |
| `Video_Date` | string | Date as written in the file (`2019`, `2023-02-17T02:42:42+10:00`) |
| `Video_Year` | number | The year from the date — for sorting and search |
| `Video_Genre`, `Video_Comment` | string | Genre; comment or description |

### Audio

Tags are read by the same `symphonia` that decodes: ID3v1/v2, Vorbis comments, MP4, APE, RIFF INFO, Matroska.
Text written in cp1251 or UTF-8 disguised as Latin-1 is repaired. These fields read only headers and tags, but
ID3v1/APE sit at the end of the file, so the fields are delayed until first read. A file's tags are cached —
several columns read them once, and repeat requests are answered right away.

| Field | Type | Value |
|---|---|---|
| `Audio_Duration_Sec` | number | Duration in whole seconds (from analysis, delayed) |
| `Audio_Length` | time | Duration, h:mm:ss |
| `Audio_Artist_Title` | string | "artist - title" in lower case, punctuation stripped — the key for tag-based duplicate search |
| `Audio_Artist`, `Audio_Title`, `Audio_Album`, `Audio_Album_Artist`, `Audio_Composer` | string | Artist, title, album, album artist, composer |
| `Audio_Genre`, `Audio_Comment` | string | Genre, comment |
| `Audio_Year` | number | Year |
| `Audio_Track`, `Audio_Track_Total`, `Audio_Disc`, `Audio_Disc_Total` | number | Track number / total tracks, disc number / total discs |
| `Audio_Has_Cover` | yes/no | Whether there's an embedded cover |
| `Audio_Codec` | string | MP3, AAC, ALAC, FLAC, Opus, Vorbis, PCM... |
| `Audio_Lossless` | yes/no | Lossless compression — handy for finding lossless versions of tracks |
| `Audio_Bitrate_kbps`, `Audio_Sample_Rate_Hz`, `Audio_Channels`, `Audio_Bit_Depth` | number | Stream properties |

If `symphonia` can't open the file (WMA, AC3), stream properties (`Audio_Length`, `Audio_Bitrate_kbps`,
`Audio_Sample_Rate_Hz`, `Audio_Channels`, `Audio_Bit_Depth`, `Audio_Codec`, `Audio_Lossless`) come from Media
Foundation, and these fields are delayed like video's.

### Ready-made column sets

Below are three sets for View → Change columns → New: paste them into the `Headers` and `Contents` fields via
the set's edit dialog, or directly as lines in `wincmd.ini` (`[Columns0]`, `[Columns1]`... sections with
`Headers0`/`Contents0` etc. — substitute the number for your own sets). `mediares` is the name the plugin is
registered under in TC (matches the file name by default); if you entered a different one during
installation, replace the prefix with it. `tc.size` is TC's own built-in field, not from this plugin.

**Photos** (EXIF):

```
Headers=Resolution\nCamera\nModel\nExposure time\nAperture\nISO\nGPS Latitude\nGPS Longitude\nDate taken\nSize
Contents=[=mediares.Image_Dimensions]\n[=mediares.Photo_Make]\n[=mediares.Photo_Model]\n[=mediares.Photo_Exposure]\n[=mediares.Photo_FNumber]\n[=mediares.Photo_ISO]\n[=mediares.Photo_GPS_Latitude]\n[=mediares.Photo_GPS_Longitude]\n[=mediares.Photo_Date_Taken]\n[=tc.size]
```

**Audio** (tags and stream properties):

```
Headers=Artist\nTitle\nAlbum\nDate\nGenre\nTrack\nBitrate\nSample rate\nChannels\nCodec\nDuration\nSize
Contents=[=mediares.Audio_Artist]\n[=mediares.Audio_Title]\n[=mediares.Audio_Album]\n[=mediares.Audio_Year]\n[=mediares.Audio_Genre]\n[=mediares.Audio_Track]\n[=mediares.Audio_Bitrate_kbps]\n[=mediares.Audio_Sample_Rate_Hz]\n[=mediares.Audio_Channels]\n[=mediares.Audio_Codec]\n[=mediares.Audio_Length]\n[=tc.size]
```

**Video** (stream properties):

```
Headers=Title\nWidth\nHeight\nBitrate\nFrame rate\nCodec\nDuration\nSize
Contents=[=mediares.Video_Title]\n[=mediares.Video_Width]\n[=mediares.Video_Height]\n[=mediares.Video_Bitrate_kbps]\n[=mediares.Video_Frame_Rate]\n[=mediares.Video_Codec]\n[=mediares.Video_Length]\n[=tc.size]
```

## Installation and settings

### Installation

`build_release.bat` builds the archives (into `dist/`); each one opens in TC and installs itself:

- **WDX-only** (`mediares-wdx-*.zip`): the lightweight content plugin `mediares.wdx64` — fields only, no viewer.
- **WLX** (`mediares-wlx-*.zip`, the combo build): one file, `mediares.wlx64`, which is both a Lister and a
  content plugin. The archive installs it as a Lister plugin (F3); `pluginst.inf` can only register a file as
  one type, so the content functions (duplicate-search and tag fields) are connected separately — using the
  same file, no second copy. The easiest way is the "Register WDX" button in the plugin's settings (F3 on any
  media file → S, or "Settings..." in the right-click menu): it adds a line to the `[ContentPlugins]` section
  of `wincmd.ini`, after which TC needs to be restarted. Manually:
  1. Close Total Commander (it rewrites `wincmd.ini` on exit).
  2. Open `wincmd.ini` (the path is in "Help → About", or the `%COMMANDER_INI%` variable).
  3. In the `[ContentPlugins]` section, add a line with the next free number, e.g. if `0=` and `1=` already
     exist:
     ```ini
     [ContentPlugins]
     2=%COMMANDER_PATH%\Plugins\wlx\mediares\mediares.wlx64
     ```
     (the path is wherever the Lister plugin was installed).
  4. Start TC: the fields appear in the `mediares` group (columns, search, multi-rename).

  The same result without editing the ini: "Configuration → Options → Plugins → Content plugins → Configure →
  Add", choose "All files" in the file picker and select `mediares.wlx64`.

### Settings

Everything is configured in the settings window (S); there's no need to edit the ini by hand. `mediares.ini`
is looked for next to the DLL (a portable install); if it's not there, TC's plugin settings folder is used
(the path from `ListSetDefaultParams`), since the plugin's own folder is often read-only. Data tied to this
particular computer doesn't travel with a portable TC and lives in `%LOCALAPPDATA%\mediares`:
`mediares_resume.txt` (video positions, up to 200 entries), `mediares_wallpaper.png` and `mpv_shader_cache`.

`[Settings]` keys with a non-obvious format: `Language` (`auto`, `en`, `ru`), `PhotoBackground` and
`OSDFontColor` (a COLORREF `0x00BBGGRR` as a decimal number), `OSDMode` (0 — off, 1 — on photos, 2 — on video,
3 — both), `Repeat` (0 — none, 1 — list, 2 — file), `SeekStep` (the ← / → step in seconds, 1–600), `FrameFormat` (`png` / `jpg`), `PhotoEditor` /
`VideoEditor` / `AudioEditor` (a program path, optionally with arguments and `%1`), `PhotoOSDTemplate` /
`VideoOSDTemplate` (see OSD). Flags — 0/1: `StartFullscreen`, `AutoRotateExif`, `NoUpscale`, `SmoothZoom`,
`ConfirmDelete`, `ResumeVideo`, `AutoAdvance`, `Shuffle`, `OverlayPhoto`, `OverlayVideo`, `OverlayAutoHide`.

The UI is in Russian and English. `auto` follows Total Commander's language (`LanguageIni` in `wincmd.ini`:
`*RUS*` means Russian, otherwise English), and without a `wincmd.ini` it follows the Windows UI language.
Strings are written inline as pairs, `tr("русский", "English")` (`combo/src/i18n.rs`). WDX field names are not
translated.

## Building and testing

```bash
cargo test                 # tests for the whole workspace
cargo build --release      # binaries in target/release/ (for a release use build_release.bat, see below)
build_release.bat          # tests, build and TC archives in dist/ (--no-test to skip tests)
```

Each archive ships with instructions for the user: `docs/readme_rus.txt` and `docs/readme_eng.txt` (UTF-8
with a BOM, so Lister shows Cyrillic correctly right away). A new WDX field needs to be described in both:
the `test_readmes_list_every_field` test checks that every field is mentioned there.

`build_release.bat` builds each DLL on its own as a plain `cdylib` (`cargo rustc --release -p … --lib
--crate-type cdylib`): with the `rlib` type alongside (tests and examples need it) LTO keeps more code, and the
DLLs come out about 230 KB larger.

After the build, in `target/release/`:
- `mediares_wdx.dll` → rename to `mediares.wdx64`
- `mediares_combo.dll` → rename to `mediares.wlx64` (also connected as the content plugin)

### Dev harness

```bash
# audio duplicate-search fields for a set of files
cargo run --release -p mediares_core --features audio-decode,tags --example audio_fp -- <files...>
# audio tag and codec fields
cargo run -p mediares_core --features tags --example audio_tags -- <files...>
# video properties for the Video_* fields (from Git Bash, pass paths as D:/..., not /d/...)
cargo run -p mediares_core --example video_meta -- <files...>
cargo run -p mediares_combo --example lister_harness -- <file> <screenshot_folder> key:4D wait:1500 shot:a key:0D wait:1000 shot:fs
```

## License

[MIT](LICENSE).
