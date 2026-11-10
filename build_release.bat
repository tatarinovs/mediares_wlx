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

:: Extract version from combo/Cargo.toml
set "VERSION=unknown"
for /f "tokens=3 delims= " %%A in ('findstr /b "version" combo\Cargo.toml') do (
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
for %%P in (mediares_combo mediares_wdx) do (
    cargo +%TOOLCHAIN% rustc --locked --release --target %TARGET% -p %%P --lib --crate-type cdylib
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

:: Prepare dist and temporary staging directories
echo:
echo [3/4] Preparing output directories...
set "DIST_DIR=%~dp0dist"
set "STAGING_DIR=%~dp0target\staging"

if not exist "%DIST_DIR%" mkdir "%DIST_DIR%"
if exist "%STAGING_DIR%" rd /s /q "%STAGING_DIR%"
mkdir "%STAGING_DIR%"

:: Copy standalone binaries to dist/
echo [INFO] Copying standalone binaries to dist...
copy /y "%COMBO_DLL%" "%DIST_DIR%\mediares.wlx64" >nul
copy /y "%WDX_DLL%" "%DIST_DIR%\mediares_wdx_only.wdx64" >nul

copy /y "pluginst\pluginst-wlx.inf" "%DIST_DIR%\pluginst.inf" >nul

:: No mediares.ini in the archives: next to the DLL it would force portable mode (settings written into
:: the plugin folder, often read-only); without it the plugin uses TC's plugin settings folder and its defaults.
if exist "%DIST_DIR%\mediares.ini" del "%DIST_DIR%\mediares.ini"

:: Packaging Archives
echo:
echo [4/4] Packaging Total Commander plugin zip archives...

:: Package 1: Mediares WDX (fields only, no viewer)
echo   - Packaging mediares-wdx-v!VERSION!.zip ...
set "STAGE_WDX=%STAGING_DIR%\wdx"
mkdir "%STAGE_WDX%"
copy /y "%WDX_DLL%" "%STAGE_WDX%\mediares.wdx64" >nul
copy /y "pluginst\pluginst-wdx.inf" "%STAGE_WDX%\pluginst.inf" >nul
copy /y "pluginst\mediares.lng" "%STAGE_WDX%\mediares.lng" >nul
copy /y "docs\readme_rus.txt" "%STAGE_WDX%\readme_rus.txt" >nul
copy /y "docs\readme_eng.txt" "%STAGE_WDX%\readme_eng.txt" >nul

pushd "%STAGE_WDX%"
tar -a -c -f "%DIST_DIR%\mediares-wdx-v!VERSION!.zip" *
popd

:: Package 2: Mediares WLX: the viewer, installed as the Lister plugin; the same file also
:: carries the content-plugin fields, connected from its settings ("Register WDX")
echo   - Packaging mediares-wlx-v!VERSION!.zip ...
set "STAGE_WLX=%STAGING_DIR%\wlx"
mkdir "%STAGE_WLX%"
copy /y "%COMBO_DLL%" "%STAGE_WLX%\mediares.wlx64" >nul
copy /y "pluginst\pluginst-wlx.inf" "%STAGE_WLX%\pluginst.inf" >nul
:: Russian field names for TC's dialogs; must sit next to the plugin under the same name.
copy /y "pluginst\mediares.lng" "%STAGE_WLX%\mediares.lng" >nul
:: Sample of the built-in UI translations; a copy next to the plugin overrides them.
copy /y "pluginst\mediares_ui.lng" "%STAGE_WLX%\mediares_ui.lng" >nul
copy /y "docs\readme_rus.txt" "%STAGE_WLX%\readme_rus.txt" >nul
copy /y "docs\readme_eng.txt" "%STAGE_WLX%\readme_eng.txt" >nul

pushd "%STAGE_WLX%"
tar -a -c -f "%DIST_DIR%\mediares-wlx-v!VERSION!.zip" *
popd

:: Cleanup staging
rd /s /q "%STAGING_DIR%"

echo:
echo =======================================================
echo   Build and packaging completed successfully!
echo =======================================================
echo Output directory: %DIST_DIR%
echo:
dir "%DIST_DIR%\*.zip" "%DIST_DIR%\*.w?x64" | findstr /v "Directory Volume"
echo:
