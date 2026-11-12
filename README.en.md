# Mediares for Total Commander

[Русский](README.md) | **English**

[![Platform](https://img.shields.io/badge/Platform-Windows%20x64-0078D6?logo=windows&logoColor=white)](https://www.microsoft.com/windows)
[![Release](https://img.shields.io/github/v/release/tatarinovs/mediares_wlx)](https://github.com/tatarinovs/mediares_wlx/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Mediares opens photos, video and audio with **F3**, displays metadata in Total Commander columns and helps find duplicates.

Two versions are available:

- **WLX + WDX** — viewer and content plugin in a single file, `mediares.wlx64`.
- **WDX** — metadata and duplicate search only, without the viewer (`mediares.wdx64`).

## Contents

- [System requirements](#system-requirements)
- [Installation](#installation)
- [Viewer](#viewer)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Metadata and columns](#metadata-and-columns)
- [Ready-made column sets](#ready-made-column-sets)
- [Duplicate search](#duplicate-search)
- [Settings](#settings)
- [Building](#building)
- [License](#license)

## System requirements

- Windows 10 (1607) or later, 64-bit. SVG requires Windows 10 (1703) or later.
- 64-bit Total Commander.
- Windows N / KN requires the **Media Feature Pack**.

HEIC, AVIF and JPEG XL may require codec extensions from Microsoft Store or [libmpv](#additional-codecs-libmpv).

## Installation

1. Download an archive from [Releases](https://github.com/tatarinovs/mediares_wlx/releases/latest): `mediares-wlx-*.zip` for the viewer or `mediares-wdx-*.zip` for content features only.
2. Open the archive in Total Commander and confirm installation.
3. Select a media file and press **F3** to view it.

In the WLX version, content features need separate registration: open a media file with F3, press **S**, click **“Register WDX”** and restart Total Commander.

You can also register them manually: **Configuration → Options → Plugins → Content plugins → Configure → Add**. Select “All files” and choose the installed `mediares.wlx64`.

## Viewer

### Photos

Supports JPEG, PNG, GIF, WebP, BMP, ICO, TGA, HDR, TIFF, JPEG XR, DDS, PSD/PSB and RAW from popular cameras. RAW files use their embedded JPEG preview; PSD/PSB files use the saved composite image. All pages of multipage TIFF files are available.

HEIC/HEIF, AVIF and JPEG XL open when the corresponding codecs are available. SVG/SVGZ are supported without text, filters or masks.

Features:

- Zoom, magnifier, rotation, two-photo comparison and slideshow.
- EXIF display and shooting location on a map.
- Save as JPEG/PNG, copy, print and open in an external editor.
- Lossless JPEG rotation and metadata removal.
- Skip RAW files when a matching JPEG exists.

### Video and audio

**Video:** MP4, MKV, AVI, MOV, WMV, WebM, M4V, 3GP, FLV, TS, MTS, MPEG, VOB and other formats when codecs are available.

**Audio:** MP3, MP2, FLAC, WAV, OGG, M4A/M4B, AAC, AIFF, CAF, MKA, WMA, Opus and AC3. Displays tags, cover art and lyrics when present in the file.

Features:

- Playlist from the current folder or an M3U/M3U8 file, repeat and shuffle.
- Seeking, frame stepping, video playback speed and audio track selection.
- Video timeline previews, chapters and A–B repeat.
- Save a frame or create a contact sheet.
- Resume video from the last position.
- Gapless audio between compatible tracks and ReplayGain volume adjustment.

The viewer also provides thumbnails for Total Commander: photos, video frames or audio cover art.

### Additional codecs: libmpv

For broader format support, place a 64-bit **`libmpv-2.dll`** next to the plugin or `totalcmd64.exe` and restart Total Commander. The library is not included.

Download it from [shinchiro releases](https://github.com/shinchiro/mpv-winbuild-cmake/releases) — archive **`mpv-dev-x86_64-<date>.7z`** — or [SourceForge](https://sourceforge.net/projects/mpv-player-windows/files/libmpv/). The `x86_64-v3` variant requires Intel Haswell / AMD Excavator or later.

## Keyboard shortcuts

### General

| Action | Keys |
|---|---|
| Next / previous file | PgDn / PgUp, N / P; Backspace — previous |
| Fullscreen | Enter, F, F11 or double-click; Esc — exit |
| On-screen information (OSD) | O or I |
| Settings | S |
| Copy image, frame or cover art | Ctrl+C |
| Print preview and printing | Ctrl+P |
| Open in editor / show in folder | F4 / Ctrl+Enter |
| Move to Recycle Bin | Del |

### Photos

| Action | Keys / mouse |
|---|---|
| Zoom | + / −, Ctrl+wheel |
| 100% / fit to window | Ctrl+1 / Ctrl+0 |
| Pan a zoomed photo | Arrow keys or drag |
| Magnifier in fit mode | Hold the left mouse button |
| TIFF pages | ← / →; when zoomed — Ctrl+PgUp / Ctrl+PgDn |
| Rotate left / right | L / R |
| Save JPEG rotation losslessly | Ctrl+R |
| Keep zoom when browsing | Z |
| Compare two photos | C; press again to exit |
| Slideshow | F5 |
| EXIF / shooting location on map | E / G |
| Save as JPEG or PNG | Ctrl+S |

By default, the mouse wheel browses photos and Ctrl+wheel zooms. You can swap this behavior in settings. In multipage TIFF files, the wheel browses pages first.

### Video and audio

| Action | Keys / mouse |
|---|---|
| Pause / play | Space, K, click video or cover art |
| Seek | ← / → or playback timeline |
| Next / previous keyframe; ±1 s in audio | ↑ / ↓ |
| Previous / next frame | , / .; forward only for MPG/MPEG/VOB |
| Video speed / reset | [ / ] / \ |
| Volume / mute | + / −, Ctrl+wheel / M |
| Save frame / create contact sheet | Shift+S / Ctrl+Shift+S |
| A–B repeat: start, end, disable | Press A in sequence |
| Next audio track | B |
| Lyrics / cover art | Y |

## Metadata and columns

Both versions provide fields in the **`mediares`** group for columns, tooltips, search and multi-rename.

| Type | Available information |
|---|---|
| Photos | Dimensions, aspect ratio, sharpness, camera, lens, shooting date, exposure, aperture, ISO, GPS |
| Video | Duration, dimensions, frame rate, codecs, bitrate, HDR, rotation, audio track and tags |
| Audio | Artist, title, album, year, genre, track number, duration, codec and audio properties |

For the full field list, see the [user guide](docs/readme_eng.txt).

### Ready-made column sets

Below are three sets for **Show → Custom columns mode → Configure custom columns → New**. Use `Headers` (column headings) and `Contents` (field expressions) when editing a set, or add the lines directly to `wincmd.ini`
(in sections such as `[Columns0]`, `[Columns1]`, with `Headers0`/`Contents0`, etc.; adjust the numbers to match your sets).
`mediares` is the plugin's registered name in Total Commander (by default, its filename). If you chose another name during installation, replace the prefix. `tc.size` is a built-in Total Commander field.

**Photos** (EXIF):

```
Headers=Dimensions\nMake\nModel\nExposure\nAperture\nISO\nGPS Latitude\nGPS Longitude\nDate taken\nSize
Contents=[=mediares.Image_Dimensions]\n[=mediares.Photo_Make]\n[=mediares.Photo_Model]\n[=mediares.Photo_Exposure]\n[=mediares.Photo_FNumber]\n[=mediares.Photo_ISO]\n[=mediares.Photo_GPS_Latitude]\n[=mediares.Photo_GPS_Longitude]\n[=mediares.Photo_Date_Taken]\n[=tc.size]
```

**Audio** (tags and stream properties):

```
Headers=Artist\nTitle\nAlbum\nYear\nGenre\nTrack\nBitrate\nSample rate\nChannels\nCodec\nDuration\nSize
Contents=[=mediares.Audio_Artist]\n[=mediares.Audio_Title]\n[=mediares.Audio_Album]\n[=mediares.Audio_Year]\n[=mediares.Audio_Genre]\n[=mediares.Audio_Track]\n[=mediares.Audio_Bitrate_kbps]\n[=mediares.Audio_Sample_Rate_Hz]\n[=mediares.Audio_Channels]\n[=mediares.Audio_Codec]\n[=mediares.Audio_Length]\n[=tc.size]
```

**Video** (stream properties):

```
Headers=Title\nWidth\nHeight\nBitrate\nFrame rate\nCodec\nDuration\nSize
Contents=[=mediares.Video_Title]\n[=mediares.Video_Width]\n[=mediares.Video_Height]\n[=mediares.Video_Bitrate_kbps]\n[=mediares.Video_Frame_Rate]\n[=mediares.Video_Codec]\n[=mediares.Video_Length]\n[=tc.size]
```

## Duplicate search

In Total Commander's duplicate search, choose comparison by plugin fields and select a suitable field:

| Field | Use |
|---|---|
| `Image_pHash` | Re-saved and resized photo copies |
| `Image_dHash` | Photo copies with minor changes |
| `Image_CoarseHash` | Photos with contrast changes, slight cropping, RAW + JPEG pairs; more prone to false matches |
| `Video_Fingerprint` | Video copies and container changes without re-encoding |
| `Video_dHash_Mid` | Compare videos by a frame in the middle |
| `Audio_Fingerprint` | Audio copies with a different bitrate or codec |
| `Audio_PCM_Hash` | Identical decoded audio, such as WAV and FLAC |
| `Audio_Artist_Title` | Match artist and title tags |

Total Commander compares values exactly, so some modified copies may not be found. Photo rotation, cropping, video edits or different audio mastering can change the result. Audio analysis reads the entire file.

**Check matches before deleting files.**

## Settings

Press **S** in the viewer or choose **“Settings...”** from the context menu. Options include language, background, zoom, slideshow, fullscreen, playback, external editors and on-screen information (OSD).

The interface is available in Russian and English; additional translations can be added through `mediares_ui.lng`.

For a portable installation, place `mediares.ini` next to the plugin. Otherwise, Total Commander's plugin settings folder is used. Video resume positions and the local cache are stored in `%LOCALAPPDATA%\mediares`.

Hashes, fingerprints, sharpness and metadata (EXIF, tags, stream properties, dimensions and pixel format) survive restarts in `content-cache-v1.sqlite3` in that folder. Results are reread when the file size or modification time changes. To clear the cache, close all TC instances and delete the database; to disable it, set the `MEDIARES_CONTENT_CACHE=off` environment variable before starting TC. An absolute database path on a local drive can be used instead of `off`.

## Building

Requires Rust and Windows build tools.

```powershell
cargo test             # tests
cargo build --release  # build
.\build_release.bat    # tests, build and installation archives in dist/
python scripts/test_content_cache.py # check SQLite caching in the built DLL (Python 3)
```

## License

[MIT](LICENSE).
