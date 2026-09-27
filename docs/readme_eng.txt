MEDIARES — media viewer and duplicate finder for Total Commander
=================================================================

Mediares is a pair of plugins for Total Commander (64-bit):

  * A viewer (Lister, F3): photos, RAW, PSD, video and audio in one window.
  * A content plugin: fields for columns, search and duplicate search
    of photos, videos and music.

Which archive contains what:

  mediares-wlx-*.zip  — the viewer and the fields in one file (recommended).
  mediares-wdx-*.zip  — the fields only, no viewer.

Contents:
  1. Installation
  2. Viewing photos
  3. Playing video and audio
  4. Full screen, OSD, slideshow
  5. Fields and duplicate search
  6. Settings


1. INSTALLATION
---------------

Open the archive in Total Commander (Enter on the zip file) and confirm
the plugin installation.

For the wlx archive, connect the fields afterwards (it is the same
file, no second copy is needed):

  1. Press F3 on any photo or video; in the viewer press S (or choose
     "Settings..." in the right-click menu).
  2. Press "Register WDX".
  3. Restart Total Commander.

If the button does not work, add it manually: "Configuration →
Options → Plugins → Content plugins → Configure → Add", choose
"All files" in the file dialog and select mediares.wlx64.

The fields then appear in the "mediares" group.


2. VIEWING PHOTOS
-----------------

Formats: JPEG, PNG, GIF, WEBP, BMP, TIFF, ICO; RAW (CR2, CR3, CRW, NEF,
ARW, ORF, RW2, DNG, RAF, PEF, RAW) — the preview embedded in the file is
shown; PSD/PSB.

Neighbouring photos are loaded in advance, so paging is instant.

  Next photo ............... Space, →, ↓, PgDn, N, mouse wheel
  Previous photo ........... ←, ↑, PgUp, Backspace, P
                             (also the mouse side buttons)
  Zoom in / out ............ + / −, Ctrl+wheel (towards the cursor)
  100% / fit to window ..... 1 / 0 (also * and / on the numeric keypad)
  Move a zoomed photo ...... drag with the mouse
  Loupe .................... hold the left mouse button on the photo
  Rotate left / right ...... L / R (on screen only, the file is untouched)
  Full screen .............. Enter, F, F11, double click; Esc to leave
  Info line (OSD) .......... O or I
  Slideshow ................ F5
  EXIF info ................ E
  Copy image ............... Ctrl+C
  Save as .................. Ctrl+S
  Print .................... Ctrl+P
  Open in editor ........... F4
  Show in folder ........... Ctrl+Enter
  Move to Recycle Bin ...... Del
  Settings ................. S

The right-click menu also has "Set as desktop background".

Save as (Ctrl+S) saves the photo as JPEG or PNG. Without quality loss
where possible: a file already in that format is simply copied, and
a RAW is saved to JPEG as is, with all shooting details (date, camera,
lens, exposure, GPS). If the photo was rotated with L / R, the new file
is saved rotated.

Copy (Ctrl+C): the image can be pasted both into an editor and into
a folder as a file.

Open in editor (F4) uses the program Windows has for editing this file
type. You can choose your own program in the settings.


3. PLAYING VIDEO AND AUDIO
--------------------------

Video: MP4, MKV, AVI, MOV, WMV, WEBM, M4V, 3GP, FLV, TS, MTS, MPG, VOB
and more — whatever the Windows codecs can play.

Audio: MP3, FLAC, WAV, OGG, M4A, AAC, AIFF, WMA, Opus, AC3 and more.
Album art, title, artist, album and year are shown.

The files of the folder (or of an opened .m3u playlist) play one after
another. Repeat and shuffle are in the right-click menu and in the
settings.

  Play / pause ............. Space, K, click on the video
  Seek ±5 seconds .......... ← / →, click on the progress bar
  Next / previous key frame  ↑ / ↓ (audio: ±1 second)
  Previous / next frame .... , / .
  Slower / faster .......... [ / ]  (normal speed — \)
  Volume ................... + / −, wheel over the bar, Ctrl+wheel
  Mute ..................... M
  Next / previous file ..... N / P, PgDn / PgUp, Backspace
  Full screen .............. Enter, F, F11, double click; Esc to leave
  Save frame ............... Shift+S (next to the video; PNG or JPEG —
                             chosen in the settings)
  Copy frame / album art ... Ctrl+C
  Open in editor ........... F4
  Show in folder ........... Ctrl+Enter
  Move to Recycle Bin ...... Del

Videos longer than 5 minutes resume where they were closed.


4. FULL SCREEN, OSD, SLIDESHOW
------------------------------

Full screen fills the whole monitor; the cursor hides when the mouse
does not move. Videos get the control bar over the picture, photos get
⏮ ⏯ ⏭ buttons at the bottom. The panels can hide and reappear when the
mouse approaches the bottom edge of the screen (a setting).

The OSD is an info line over the photo or video: file name, size,
position in the folder, zoom, for videos the time. What it shows is set
by a template in the settings ("Photos..." and "Videos..." buttons);
"Add field..." inserts a field such as the camera, exposure, ISO or
the video codec.

The slideshow (F5) shows the photos of the folder in a loop, skipping
videos and music. The interval is set in the settings.


5. FIELDS AND DUPLICATE SEARCH
------------------------------

The plugin's fields can be shown in panel columns and used in file
search (Alt+F7) and in multi-rename (Ctrl+M). They are in the
"mediares" group.

Some fields (everything about video, music, RAW and PSD) need the whole
file to be read. Total Commander fills them in the background, so they
appear in the columns with a short delay.


5.1. Finding duplicates

Total Commander can find duplicates by a plugin field: in file search
(Alt+F7), on the "Advanced" tab, turn on duplicate search and choose one
of the fields below. Files count as duplicates only when the field
values are exactly equal.

Photos:

  Image_pHash       The best choice in most cases. Finds re-saved and
                    downscaled copies (from messengers, clouds, after
                    resizing). Different photos almost never match.
  Image_dHash       Similar to pHash, a little less likely to find
                    heavily compressed or downscaled copies.
  Image_CoarseHash  The most lenient: also finds copies with changed
                    contrast, slightly cropped ones, and a RAW + JPEG
                    pair of the same shot. Sometimes groups neighbouring
                    near-identical shots of a burst — check the results
                    by eye.

  No field finds a copy that is rotated, mirrored or noticeably
  cropped.

  How often a copy is found (measured on real photos):

    Change in the copy            dHash   pHash   CoarseHash
    JPEG re-saved                  ~90%   ~100%      ~90%
    Strong JPEG compression        ~70%    ~90%      ~80%
    Half size                      ~90%    ~90%     ~100%
    Contrast changed               ~30%    ~30%      ~50%
    Edges cropped by 2%            ~10%      0%      ~30%
    Rotated, mirrored                 0%      0%         0%
    RAW and JPEG of one shot       ~40%    ~50%      ~70%
    Neighbouring burst shots (error)  0%      0%       ~3%

Video:

  Video_Fingerprint  Exact copies and the same movie moved into another
                     container without re-encoding (e.g. MKV ↔ MP4).
                     Uses the duration and three frames.
  Video_dHash_Mid    Less strict: compares only the middle frame.

  Re-encoded video (another quality, another codec) is usually
  not found.

Music:

  Audio_Fingerprint  The same recording in different formats and
                     quality: MP3 ↔ FLAC, another bitrate, mono, another
                     volume. A different master, a live version or
                     a trimmed recording will not match.
  Audio_PCM_Hash     The same sound with other tags or in another
                     container (a re-tagged MP3, WAV ↔ FLAC).
  Audio_Artist_Title "artist - title" from the tags, ignoring case
                     and punctuation.

Sizes, aspect ratio and duration (see below) are useful for a rough
first pass.


5.2. All fields

General:
  Media_Type            File type: Image, Video or Audio
  Plugin_Version        Plugin version

Photos:
  Image_Width, Image_Height      Width and height in pixels
  Image_Dimensions               Size as text: 6000x4000
  Image_AspectRatio              Aspect ratio: 3:2, 16:9, 4:3...
  Photo_Make, Photo_Model        Camera make and model
  Photo_Lens                     Lens
  Photo_Date_Taken               Date and time taken
  Photo_Exposure                 Exposure time: 1/250
  Photo_FNumber                  Aperture: 2.8
  Photo_ISO                      ISO sensitivity
  Photo_Focal_Length_mm          Focal length, mm
  Photo_Focal_Length_35mm        35 mm equivalent focal length
  Photo_Flash                    Whether the flash fired
  Photo_Orientation              Orientation (code 1–8)
  Photo_Software                 Program that saved the file
  Photo_GPS_Latitude             Latitude in degrees
  Photo_GPS_Longitude            Longitude in degrees
  Photo_Has_GPS                  Whether there are coordinates

Video:
  Video_Duration_Sec             Duration in seconds
  Video_Length                   Duration h:mm:ss
  Video_Width, Video_Height      Frame width and height
  Video_Dimensions               Size as text: 1920x1080
  Video_Frame_Rate               Frames per second
  Video_Codec                    Video codec: H.264, HEVC, AV1...
  Video_Bitrate_kbps             Bitrate, kbps
  Video_Audio_Codec              Audio codec: AAC, AC-3...
  Video_Audio_Channels           Number of audio channels
  Video_Audio_Sample_Rate_Hz     Audio sample rate, Hz
  Video_Title                    Title
  Video_Artist, Video_Director   Artist, director
  Video_Date, Video_Year         Date and year
  Video_Genre                    Genre
  Video_Comment                  Comment or description

Music:
  Audio_Duration_Sec             Duration in seconds
  Audio_Length                   Duration h:mm:ss
  Audio_Artist, Audio_Title      Artist, title
  Audio_Album                    Album
  Audio_Album_Artist             Album artist
  Audio_Composer                 Composer
  Audio_Genre                    Genre
  Audio_Year                     Year
  Audio_Track, Audio_Track_Total Track number and total tracks
  Audio_Disc, Audio_Disc_Total   Disc number and total discs
  Audio_Comment                  Comment
  Audio_Has_Cover                Whether there is album art
  Audio_Codec                    Format: MP3, FLAC, AAC...
  Audio_Lossless                 Lossless (FLAC, WAV, ALAC...)
  Audio_Bitrate_kbps             Bitrate, kbps
  Audio_Sample_Rate_Hz           Sample rate, Hz
  Audio_Channels                 Number of channels
  Audio_Bit_Depth                Bit depth


6. SETTINGS
-----------

Open any file in the viewer (F3) and press S or choose "Settings..." in
the right-click menu. You can set:

  * the language (same as Total Commander, English or Russian);
  * starting in full screen;
  * auto-rotating photos by the camera data;
  * the loupe zoom, the background color around photos;
  * "Don't enlarge small images";
  * "Smooth enlarged images": enlarged photos and album art look smooth
    instead of made of square pixels (zooming in beyond 400% shows the 
    pixels as they are);
  * confirmation before moving to the Recycle Bin;
  * the OSD: where to show it, font, color and contents;
  * full screen: buttons and panels over the picture, hiding them;
  * the slideshow interval;
  * auto-advance to the next file, repeat, shuffle;
  * resuming long videos where they stopped;
  * the format of saved frames (PNG or JPEG);
  * programs for "Open in editor" — separately for photos, video
    and audio;
  * connecting the fields ("Register WDX" button).

The settings are stored in mediares.ini next to Total Commander's
settings. If you put mediares.ini next to the plugin file itself, it is
used from there (handy for a portable installation).
