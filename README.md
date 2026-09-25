# Mediares — Total Commander WDX + Combo (WDX+WLX) Plugin

Высокопроизводительный 64-битный плагин для **Total Commander** на языке **Rust**:
1. **`mediares_wdx`** — облегчённый Content-плагин (WDX) для мгновенного поиска визуальных дубликатов (фото, RAW, PSD, видео) с помощью перцептивных хешей (dHash, pHash, CoarseHash, Video Fingerprint).
2. **`mediares_combo`** — плагин 2-в-1 (WDX + WLX) в одном файле: полный функционал поиска дубликатов **плюс** быстрый встроенный просмотр по клавише **F3** (Lister) с поддержкой двойной буферизации GDI и тёмного интерфейса.

---

## Архитектура воркспейса

```
mediares_wlx/
├── Cargo.toml                  # Workspace root
├── PLAN.md                     # Дорожная карта и архитектурный план
├── core/                       # Общая библиотека (lib), без Win32-окон:
│   ├── tc_api.rs               # WDX/WLX C ABI структуры и константы
│   ├── ffi.rs                  # catch_unwind-обёртка и конвертация строк на границе с TC
│   ├── probe.rs                # Тип медиа по расширению + единый источник строк детекта
│   ├── image_decode.rs         # Декодирование фото (`image` + RAW/PSD превью) с лимитами памяти
│   ├── exif.rs                 # Разбор EXIF (JPEG, TIFF/RAW)
│   ├── jpeg.rs                 # Поиск/проверка встроенных JPEG-потоков
│   ├── raw_preview.rs          # Встроенный JPEG из RAW (IFD/SubIFD/strip + сигнатурный fallback)
│   ├── psd_preview.rs          # Композит PSD/PSB (8/16 бит) или миниатюра 0x0409/0x0410
│   ├── hashing.rs              # dHash (64-bit), pHash (DCT 64-bit), CoarseHash (32-bit), пропорции
│   ├── mf_init.rs              # Жизненный цикл COM / Media Foundation
│   ├── video_frame.rs          # Кадры на 25/50/75% через IMFSourceReader и фингерпринт
│   ├── audio_decode.rs         # Декодирование аудио в PCM (symphonia), точная перемотка — фича audio-decode
│   ├── audio_tags.rs           # Теги и обложка (lofty) — фича tags
│   ├── cache.rs                # Потокобезопасный LRU-кэш анализа (ключ: путь + размер + mtime)
│   └── wdx_api.rs              # Логика полей WDX + макрос export_content_plugin!
├── wdx/                        # Облегчённый WDX-плагин (cdylib): одна строка — export_content_plugin!()
├── combo/                      # Полный 2-в-1 плагин WDX + WLX (cdylib)
│   ├── window.rs               # Окно Lister, обработка сообщений, команды и горячие клавиши
│   ├── state.rs                # Per-HWND состояние, список файлов папки (натуральная сортировка)
│   ├── image_view.rs           # Масштаб/панорама/лупа, GDI double buffering, OSD
│   ├── image_cache.rs          # BGRA-кэш (бюджет по байтам, LRU) + фоновая предзагрузка соседей
│   ├── fullscreen.rs           # Полноэкранный режим: отдельное topmost-окно на мониторе Lister
│   ├── media_view.rs           # Общий слой аудио/видео: панель управления, мышь, события, таймеры
│   ├── playback_video.rs       # IMFMediaEngine (frame-server) + D3D11 swap chain
│   ├── video_view.rs           # Поверхность видео (letterbox) + события движка
│   ├── playback_audio.rs       # Аудио: поток-декодер symphonia → источник rodio (WASAPI)
│   ├── audio_view.rs           # Обложка + теги; fallback на Media Foundation (WMA, Opus)
│   ├── playlist.rs             # Очередь: автопереход, повтор, случайный порядок, чтение M3U
│   ├── transport_bar.rs        # Панель управления: ⏮ play/pause ⏭, время, таймлайн, звук
│   ├── config.rs               # Настройки mediares.ini
│   ├── dialog.rs               # Общие хелперы модальных диалогов
│   ├── exif_dialog.rs          # Окно EXIF-метаданных
│   ├── settings_dialog.rs      # Окно настроек
│   └── examples/lister_harness.rs  # Dev-стенд: хост-окно как у TC, сценарий и скриншоты
└── pluginst/                   # Инсталляционные скрипты для TC
    ├── pluginst-wdx.inf
    └── pluginst-combo.inf
```

### Просмотр видео и аудио (combo)

**Видео** — форматы как у WDX (MP4, MKV, AVI, MOV, WMV, WEBM, M4V, FLV, TS, MTS), декодирование через
системные кодеки Media Foundation.

**Аудио** — MP3, MP2, FLAC, WAV, OGG/OGA (Vorbis), M4A/M4B (AAC, ALAC), AAC, AIFF, CAF, MKA декодируются
на чистом Rust (`symphonia`, вывод через `rodio`/WASAPI) и не зависят от установленных кодеков. WMA и Opus,
которых нет в symphonia, играются через Media Foundation. Показываются обложка (встроенная или
`cover`/`folder`/`front.jpg` из папки), название, исполнитель, альбом, год и параметры потока (`lofty`).

Если файл не удаётся открыть, `ListLoad` возвращает NULL и TC пробует другой плагин.

**Плей-лист** — это файлы текущей папки (фото пропускаются) или записи открытого `.m3u`/`.m3u8`.
По окончании файла включается следующий (автопереход можно отключить); есть повтор списка или
одного файла и случайный порядок — в контекстном меню и в настройках.

| Действие | Клавиши / мышь |
|---|---|
| Пауза / воспроизведение | Пробел, K, клик по видео / обложке, мультимедийная клавиша |
| Перемотка ±5 с | ← / →, клик или перетаскивание по таймлайну |
| Следующий / предыдущий ключевой кадр (аудио: ±1 с) | ↑ / ↓ |
| Громкость | + / −, колесо мыши, ползунок; M — без звука |
| Следующий / предыдущий трек (только аудио/видео) | кнопки ⏭ / ⏮ на панели, мультимедийные клавиши |
| Следующий / предыдущий файл | N / P, PgDn / PgUp, Backspace |
| Полноэкранный режим | Enter, F, F11, двойной клик; Esc — выход |

### Dev-стенд

```bash
cargo run -p mediares_combo --example lister_harness -- <файл> <папка_скриншотов> key:4D wait:1500 shot:a key:0D wait:1000 shot:fs
```

### Настройки

`mediares.ini` ищется рядом с DLL (портативная установка); если его там нет — используется папка
плагинных настроек TC (путь из `ListSetDefaultParams`), т.к. папка плагина часто защищена от записи.

## Сборка и тестирование

### Запуск тестов
```bash
cargo test
```

### Сборка релизных бинарников
```bash
cargo build --release
```

После сборки в папке `target/release/` появятся:
- `mediares_wdx.dll` → переименовать в `mediares.wdx64`
- `mediares_combo.dll` → переименовать в `mediares.wlx64` (и `mediares.wdx64`)
