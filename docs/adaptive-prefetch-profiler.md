# Профайлер адаптивного прогрева WDX

Из PowerShell соберите профайлер с поддержкой аудио и счётчиками кэша.

```powershell
Set-Location D:\PROJECT\mediares_wlx
cargo build --release --locked -p mediares_core --features audio-decode,tags,profile-stats --example wdx_profile
Remove-Item Env:MEDIARES_PREFETCH_THREADS -ErrorAction SilentlyContinue
```

Без `--threads` профайлер автоматически подбирает число потоков. Каждый пример можно запускать отдельно.

```powershell
.\target\release\examples\wdx_profile.exe "D:\test\11" --field Image_pHash --limit 800 --log target\auto-large-jpg.tsv
.\target\release\examples\wdx_profile.exe "D:\pictures" --field Image_pHash --limit 3000 --log target\auto-pictures.tsv
.\target\release\examples\wdx_profile.exe "D:\test\mp3\1" --field Audio_Fingerprint --log target\auto-audio.tsv
.\target\release\examples\wdx_profile.exe "D:\test\vid\1" --field Video_Fingerprint --limit 800 --log target\auto-small-video.tsv
.\target\release\examples\wdx_profile.exe "D:\video" --field Video_Fingerprint --limit 600 --log target\auto-large-video.tsv
```

Начальная строка показывает `threads=auto`. Строки `[prefetch auto]` показывают решения регулятора. Рядом с каждым TSV появится файл `.summary.txt`. Для полного прогона уберите `--limit`.
