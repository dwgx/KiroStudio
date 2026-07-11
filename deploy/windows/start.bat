@echo off
REM ============================================================================
REM KiroStudio - Windows guided launcher entry (double-click to run)
REM Calls start.ps1: auto-detect/generate config + print keys/URL + run gateway.
REM First run auto-generates config.json with random keys, no manual setup.
REM (Comments kept ASCII-only so this .bat is encoding-immune on any locale.)
REM All user-facing Chinese text lives in start.ps1, which is UTF-8 with BOM.
REM ============================================================================
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start.ps1"
