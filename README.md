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
├── core/                       # Общая библиотека (lib):
│   ├── tc_api.rs               # WDX/WLX C ABI структуры и константы
│   ├── hashing.rs              # dHash (64-bit), pHash (DCT 64-bit), CoarseHash (32-bit), пропорции
│   ├── probe.rs                # Детектирование типа медиа (Image, Raw, PSD, Video, Audio)
│   ├── raw_preview.rs          # Извлечение встроенного JPEG из RAW (CR2, NEF, ARW, DNG, etc.)
│   ├── psd_preview.rs          # Извлечение превью и композита из PSD (ресурс 0x0410)
│   ├── video.rs                # Windows Media Foundation: кадры на 25/50/75% и фингерпринт
│   ├── cache.rs                # Потокобезопасный LRU-кэш анализа файлов
│   └── wdx_api.rs              # Централизованная логика всех полей Total Commander
├── wdx/                        # Облегчённый WDX-плагин (cdylib)
├── combo/                      # Полный 2-в-1 плагин WDX + WLX (cdylib)
│   ├── wlx_state.rs            # Per-HWND состояние инстансов Lister
│   ├── image_view.rs           # Декодирование и конвертация в BGRA DIB
│   └── wlx_window.rs           # Окно Lister, двойная буферизация GDI, letterbox
└── pluginst/                   # Инсталляционные скрипты для TC
    ├── pluginst-wdx.inf
    └── pluginst-combo.inf
```

---

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
