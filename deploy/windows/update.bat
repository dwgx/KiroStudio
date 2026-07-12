@echo off
REM ============================================================================
REM KiroStudio - Windows update script (follow upstream, rebuild)
REM Pulls latest code from the upstream git repo, rebuilds frontend + exe.
REM NOTE: The panel's "OTA update" button DOES work on Windows now (v0.6.6+):
REM   it downloads the Windows .exe, swaps it via rename (renaming the running
REM   exe out of the way), and auto-restarts (v0.7.5+ self-relaunches even when
REM   run as a bare double-clicked exe). Use OTA to jump to the latest RELEASE.
REM Advantage of this script over OTA: git pull fetches the FULL latest master
REM   (every change), not just versions that happen to have a GitHub Release.
REM After it finishes: restart the gateway (close the start/run window, then
REM   double-click start.bat or run.bat again) to load the new exe.
REM   ASCII-only on purpose (see build.bat note on .bat encoding).
REM ============================================================================
setlocal

REM Go to project root (this script lives in deploy\windows\, two levels up)
cd /d "%~dp0..\.."
echo [KiroStudio] Project root: %CD%
echo.

REM ---- 1) Check git ----
where git >nul 2>nul
if errorlevel 1 (
  echo [ERROR] git not found. Install Git for Windows: https://git-scm.com/download/win
  goto :fail
)

REM ---- 2) Refuse to clobber local changes to TRACKED files (pull could conflict) --
REM    Only uncommitted changes to tracked files matter: those are what a pull can
REM    conflict with or overwrite. We use --untracked-files=no so that ordinary
REM    untracked files in the project root (notes, screenshots, local config.json /
REM    credentials.json, target/, etc.) do NOT falsely block the update.
REM    We do NOT auto-discard your work: if tracked files are dirty, we stop and
REM    ask you to commit or stash first.
set DIRTY=
for /f %%i in ('git status --porcelain --untracked-files^=no 2^>nul') do set DIRTY=1
if defined DIRTY (
  echo [WARN] You have uncommitted changes to tracked files:
  git status --short --untracked-files=no
  echo.
  echo Commit or stash them before updating, so your work is not lost.
  echo   git stash        ^(shelve changes^)   or   git commit -am "wip"
  goto :fail
)

REM ---- 3) Ensure the gateway is NOT running (Windows locks a running .exe) ----
REM    cargo build writes target\release\kirostudio.exe; if the gateway is still
REM    running from that file, the write fails with "Access is denied". Stop it
REM    first (close the start.bat / run.bat window, or Ctrl-C then Y).
tasklist /fi "imagename eq kirostudio.exe" 2>nul | find /i "kirostudio.exe" >nul
if not errorlevel 1 (
  echo [WARN] kirostudio.exe is still running.
  echo Windows locks a running .exe, so the rebuild would fail to overwrite it.
  echo Please STOP the gateway first ^(close the start.bat / run.bat window,
  echo   or press Ctrl-C then Y in it^), then run update.bat again.
  goto :fail
)

REM ---- 4) Pull latest upstream ----
echo [1/2] Pulling latest code ^(git pull^) ...
git pull --ff-only
if errorlevel 1 (
  echo [ERROR] git pull failed. If it reports diverged history, resolve manually.
  goto :fail
)
echo.

REM ---- 5) Rebuild frontend + exe via build.bat ----
echo [2/2] Rebuilding ^(frontend + exe^) ...
call "%~dp0build.bat"
if errorlevel 1 (
  echo [ERROR] rebuild failed. See errors above; the running exe is unchanged.
  goto :fail
)

echo.
echo ============================================================
echo [OK] Update complete. New exe: target\release\kirostudio.exe
echo Next: restart the gateway to load it -
echo   close the start.bat / run.bat window, then double-click it again.
echo   ^(The panel "one-click restart" cannot swap the exe file on Windows;
echo    a fresh launch is required after updating the binary.^)
echo ============================================================
endlocal
exit /b 0

:fail
echo.
echo Update did not complete. The running gateway is unaffected.
endlocal
exit /b 1
