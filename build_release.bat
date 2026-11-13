@echo off
setlocal enabledelayedexpansion

echo =======================================================
echo   Mediares - Build and Release Packaging Script
echo =======================================================

cd /d "%~dp0"

:: Check for Cargo
where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] cargo is not found in PATH! Please install Rust.
    exit /b 1
)

set "TARGET=x86_64-pc-windows-gnu"
where rustup >nul 2>nul
if errorlevel 1 (
    echo [ERROR] rustup not found. Install Rust using rustup.
    exit /b 1
)

:: The GNU-host Rust toolchain includes a matching MinGW linker and CRT libraries.
:: A target added to the MSVC toolchain does not include all of these libraries.
set "TOOLCHAIN=stable-x86_64-pc-windows-gnu"
rustup run %TOOLCHAIN% rustc --version >nul 2>nul
if errorlevel 1 (
    rustup toolchain install %TOOLCHAIN% --profile minimal
    if errorlevel 1 exit /b 1
)
for /f "delims=" %%S in ('rustup run %TOOLCHAIN% rustc --print sysroot') do set "GNU_SYSROOT=%%S"
set "GNU_BIN=%GNU_SYSROOT%\lib\rustlib\%TARGET%\bin\self-contained"
if not exist "%GNU_BIN%\x86_64-w64-mingw32-gcc.exe" (
    echo [ERROR] Bundled MinGW linker not found in %GNU_BIN%
    exit /b 1
)
where dlltool.exe >nul 2>nul
if errorlevel 1 (
    echo [ERROR] Add the w64devkit bin directory to PATH before running this script.
    exit /b 1
)
set "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=%GNU_BIN%\x86_64-w64-mingw32-gcc.exe"

:: Keep inherited MSVC/static-CRT flags out of this release build.
set "RUSTFLAGS="
set "CARGO_ENCODED_RUSTFLAGS="

:: Extract the shared workspace version.
set "VERSION=unknown"
for /f "tokens=3 delims= " %%A in ('findstr /b "version " Cargo.toml') do (
    set "VERSION=%%~A"
)
echo [INFO] Target release version: v!VERSION!

:: Check command-line argument to skip tests
set "RUN_TESTS=1"
if /i "%~1"=="--no-test" set "RUN_TESTS=0"
if /i "%~1"=="/notest"    set "RUN_TESTS=0"
if /i "%~1"=="notest"     set "RUN_TESTS=0"

if "!RUN_TESTS!"=="1" (
    echo:
    echo [1/4] Running workspace tests...
    cargo +%TOOLCHAIN% test --locked --workspace --target %TARGET%
    if errorlevel 1 (
        echo [ERROR] Tests failed! Aborting release packaging.
        exit /b 1
    )
    echo [OK] All tests passed successfully.
) else (
    echo:
    echo [1/4] Skipping tests: --no-test flag specified.
)

:: Build release binaries. The crates are "cdylib" + "rlib" (the rlib serves tests and examples),
:: and with both types LTO keeps more code: built as a cdylib alone each DLL is ~230 KB smaller.
echo:
echo [2/4] Building MinGW release binaries as cdylib-only...
:: Cargo still sees the manifest's rlib and can reduce profile.strip to debuginfo.
:: Force symbol stripping for these cdylib-only artifacts as well.
for %%P in (mediares_combo mediares_wdx) do (
    cargo +%TOOLCHAIN% rustc --locked --release --target %TARGET% -p %%P --lib --crate-type cdylib -- -C strip=symbols
    if errorlevel 1 (
        echo [ERROR] Cargo build of %%P failed!
        exit /b 1
    )
)

set "COMBO_DLL=target\%TARGET%\release\mediares_combo.dll"
set "WDX_DLL=target\%TARGET%\release\mediares_wdx.dll"

if not exist "%COMBO_DLL%" (
    echo [ERROR] Expected build artifact not found: %COMBO_DLL%
    exit /b 1
)
if not exist "%WDX_DLL%" (
    echo [ERROR] Expected build artifact not found: %WDX_DLL%
    exit /b 1
)

echo:
echo [3/4] Staging and verifying plugin archives...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\package_release.ps1" -Root "%~dp0." -Version "%VERSION%" -ComboDll "%COMBO_DLL%" -WdxDll "%WDX_DLL%"
if errorlevel 1 (
    echo [ERROR] Packaging failed. Previous artifacts were not reported as a new release.
    exit /b 1
)
echo:
echo [4/4] Build and packaging completed successfully.
exit /b 0
