@echo off
setlocal
cd /d "%~dp0"
python "%~dp0scripts\test_content_cache.py" %*
exit /b %errorlevel%
