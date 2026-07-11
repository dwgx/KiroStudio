@echo off
REM ============================================================================
REM KiroStudio - Windows build script
REM Builds frontend admin-ui/dist (embedded at compile time by rust-embed)
REM   + compiles the release exe (crt-static, no VCRUNTIME140.dll dependency).
REM Output: target\release\kirostudio.exe (frontend bundled, single-file run).
REM Usage: double-click, or run deploy\windows\build.bat from project root.
REM NOTE: ASCII-only on purpose. .bat Chinese text is unreliable across
REM       codepages (chcp 65001 + multibyte can desync cmd's parser). All
REM       Chinese user-facing text lives in start.ps1 (UTF-8 with BOM).
REM ============================================================================
setlocal

REM Go to project root (this script lives in deploy\windows\, two levels up)
cd /d "%~dp0..\.."
echo [KiroStudio] Project root: %CD%
echo.

REM ---- 1) Check toolchain ----
where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] cargo not found. Install Rust first: https://rustup.rs
  goto :fail
)

REM ---- 2) Build frontend dist (rust-embed needs admin-ui\dist before cargo build) ----
echo [1/2] Building frontend admin-ui\dist ...
pushd admin-ui
where pnpm >nul 2>nul
if errorlevel 1 (
  echo   pnpm not found, using npm
  if not exist node_modules ( call npm install )
  call npm run build
) else (
  call pnpm install --frozen-lockfile
  call pnpm build
)
set FE_ERR=%errorlevel%
popd
if not "%FE_ERR%"=="0" (
  echo [ERROR] frontend build failed
  goto :fail
)
if not exist admin-ui\dist\index.html (
  echo [ERROR] missing frontend output admin-ui\dist\index.html
  goto :fail
)
echo   frontend build done
echo.

REM ---- 3) Compile release exe (crt-static via .cargo\config.toml) ----
REM --no-default-features drops native-tls (vendored OpenSSL needs Perl+NASM on
REM   Windows, a needless build barrier). Pure rustls: reqwest bundles its own
REM   roots and the generated config defaults to tlsBackend=rustls, so this is
REM   self-consistent AND identical to the downloadable GitHub Release exe.
echo [2/2] Compiling release binary (rustls, first build includes LTO, ~1-2 min)...
cargo build --release --no-default-features
if errorlevel 1 (
  echo [ERROR] cargo build failed
  goto :fail
)

echo.
echo ============================================================
echo [OK] Output: %CD%\target\release\kirostudio.exe
echo Next: double-click deploy\windows\start.bat (auto-config + run)
echo ============================================================
endlocal
exit /b 0

:fail
echo.
echo Build did not complete. See errors above.
endlocal
exit /b 1
