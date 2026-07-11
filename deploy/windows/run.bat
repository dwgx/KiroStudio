@echo off
REM ============================================================================
REM KiroStudio - Windows plain start script (foreground, supervised loop)
REM Starts the gateway in the project root; logs print to this window.
REM Acts as a lightweight supervisor (like systemd Restart=always) so the
REM   admin panel's "one-click restart" / OTA update works on Windows:
REM   - exe exits cleanly (code 0) -> treated as a panel restart, relaunched.
REM   - exe exits non-zero (crash/bad config) -> NOT relaunched, error shown.
REM Stop the service: press Ctrl-C then answer Y to "Terminate batch job",
REM   or just close this window (whole process tree ends).
REM Config: reads config.json / credentials.json from project root (cwd).
REM NOTE: This script does NOT auto-generate config. If you have no config,
REM       use start.bat instead (guided: auto-creates config + prints keys).
REM       ASCII-only on purpose (see build.bat note on .bat encoding).
REM ============================================================================
setlocal

REM Go to project root (config/data resolve relative to cwd)
cd /d "%~dp0..\.."

set EXE=target\release\kirostudio.exe
if "%RUST_LOG%"=="" set RUST_LOG=info

if not exist "%EXE%" (
  echo [ERROR] not found: %EXE%
  echo Run deploy\windows\build.bat first to compile.
  goto :hold
)
if not exist config.json (
  echo [WARN] config.json missing. Tip: use start.bat for guided setup.
)
if not exist credentials.json (
  echo [WARN] credentials.json missing (add accounts via /admin panel).
)

echo ============================================================
echo [KiroStudio] Starting... cwd: %CD%
echo Log level RUST_LOG=%RUST_LOG%
echo Stop: close this window, or Ctrl-C then Y (Terminate batch job)
echo ============================================================
echo.

:runloop
"%EXE%"
set EXITCODE=%errorlevel%
if "%EXITCODE%"=="0" (
  echo.
  echo [KiroStudio] Gateway exited cleanly ^(code 0^) - panel restart/OTA, relaunching in 2s...
  echo   To stop the service instead: press Ctrl-C now, or close this window.
  timeout /t 2 /nobreak >nul
  goto :runloop
)
echo.
echo [KiroStudio] Gateway exited with code %EXITCODE% ^(crash or bad config^) - NOT relaunching.
echo   Check the log above ^(common: port in use / bad config.json / missing apiKey^).
:hold
echo Press any key to close...
pause >nul
endlocal
