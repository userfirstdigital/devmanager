@echo off
setlocal
powershell -ExecutionPolicy Bypass -File "%~dp0dev-watch.ps1" %*
