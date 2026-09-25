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
│   ├── cache.rs                # Потокобезопасный LRU-кэш анализа (ключ: путь + размер + mtime)
│   └── wdx_api.rs              # Логика полей WDX + макрос export_content_plugin!
├── wdx/                        # Облегчённый WDX-плагин (cdylib): одна строка — export_content_plugin!()
├── combo/                      # Полный 2-в-1 плагин WDX + WLX (cdylib)
│   ├── window.rs               # Окно Lister, обработка сообщений, команды и горячие клавиши
│   ├── state.rs                # Per-HWND состояние, список файлов папки (натуральная сортировка)
│   ├── image_view.rs           # Масштаб/панорама/лупа, GDI double buffering, OSD
│   ├── image_cache.rs          # BGRA-кэш (бюджет по байтам, LRU) + фоновая предзагрузка соседей
│   ├── fullscreen.rs           # Полноэкранный режим: отдельное topmost-окно на мониторе Lister
│   ├── config.rs               # Настройки mediares.ini
│   ├── dialog.rs               # Общие хелперы модальных диалогов
│   ├── exif_dialog.rs          # Окно EXIF-метаданных
│   └── settings_dialog.rs      # Окно настроек
└── pluginst/                   # Инсталляционные скрипты для TC
    ├── pluginst-wdx.inf
    └── pluginst-combo.inf
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
