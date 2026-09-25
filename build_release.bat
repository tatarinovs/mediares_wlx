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

:: Extract version from combo/Cargo.toml
set "VERSION=0.1.0"
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
    cargo test --workspace
    if errorlevel 1 (
        echo [ERROR] Tests failed! Aborting release packaging.
        exit /b 1
    )
    echo [OK] All tests passed successfully.
) else (
    echo:
    echo [1/4] Skipping tests: --no-test flag specified.
)

:: Build release binaries
echo:
echo [2/4] Building release binaries via cargo build --release...
cargo build --release --workspace
if errorlevel 1 (
    echo [ERROR] Cargo build failed!
    exit /b 1
)

set "COMBO_DLL=target\release\mediares_combo.dll"
set "WDX_DLL=target\release\mediares_wdx.dll"

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
copy /y "%COMBO_DLL%" "%DIST_DIR%\mediares.wdx64" >nul
copy /y "%WDX_DLL%" "%DIST_DIR%\mediares_wdx_only.wdx64" >nul

if exist "pluginst\pluginst-combo.inf" (
    copy /y "pluginst\pluginst-combo.inf" "%DIST_DIR%\pluginst.inf" >nul
)

:: Ensure default mediares.ini exists in dist
if not exist "%DIST_DIR%\mediares.ini" (
    (
        echo [Settings]
        echo StartFullscreen=0
        echo AutoRotateExif=1
        echo LoupeScale=1.0
        echo ShowOSD=1
        echo OSDFontSize=14
        echo OSDFontColor=16777215
        echo OSDFontName=Segoe UI
    ) > "%DIST_DIR%\mediares.ini"
)

:: Packaging Archives
echo:
echo [4/4] Packaging Total Commander plugin zip archives...

:: Package 1: Mediares Combo (WLX + WDX 2-in-1)
echo   - Packaging mediares-combo-v!VERSION!.zip ...
set "STAGE_COMBO=%STAGING_DIR%\combo"
mkdir "%STAGE_COMBO%"
copy /y "%COMBO_DLL%" "%STAGE_COMBO%\mediares.wlx64" >nul
copy /y "%COMBO_DLL%" "%STAGE_COMBO%\mediares.wdx64" >nul
copy /y "%DIST_DIR%\mediares.ini" "%STAGE_COMBO%\mediares.ini" >nul
copy /y "pluginst\pluginst-combo.inf" "%STAGE_COMBO%\pluginst.inf" >nul

pushd "%STAGE_COMBO%"
tar -a -c -f "%DIST_DIR%\mediares-combo-v!VERSION!.zip" *
popd

:: Package 2: Mediares WDX (Standalone duplicate finder)
echo   - Packaging mediares-wdx-v!VERSION!.zip ...
set "STAGE_WDX=%STAGING_DIR%\wdx"
mkdir "%STAGE_WDX%"
copy /y "%WDX_DLL%" "%STAGE_WDX%\mediares.wdx64" >nul
copy /y "pluginst\pluginst-wdx.inf" "%STAGE_WDX%\pluginst.inf" >nul

pushd "%STAGE_WDX%"
tar -a -c -f "%DIST_DIR%\mediares-wdx-v!VERSION!.zip" *
popd

:: Package 3: Mediares WLX (Standalone Lister viewer)
echo   - Packaging mediares-wlx-v!VERSION!.zip ...
set "STAGE_WLX=%STAGING_DIR%\wlx"
mkdir "%STAGE_WLX%"
copy /y "%COMBO_DLL%" "%STAGE_WLX%\mediares.wlx64" >nul
copy /y "%DIST_DIR%\mediares.ini" "%STAGE_WLX%\mediares.ini" >nul
copy /y "pluginst\pluginst-wlx.inf" "%STAGE_WLX%\pluginst.inf" >nul

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
