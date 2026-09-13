@echo off
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0dev-watch.ps1" -Once %*
if errorlevel 1 (
    echo.
    echo DevManager Dev Smoke failed to build or launch.
    echo Review the error above or target-live-dev\launch-status.txt.
    pause
    exit /b 1
)
echo.
echo DevManager Dev Smoke launched successfully.
echo The installed DevManager remains untouched and can stay open.
timeout /t 4 /nobreak >nul
