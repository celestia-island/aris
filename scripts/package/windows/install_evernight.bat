@echo off
REM ============================================================
REM  Entelecheia Gateway — Evernight auto-installer for Windows
REM ============================================================
REM  This script runs when the user inserts the USB-C cable and
REM  opens the auto-mounted drive (or accepts the AutoRun prompt).
REM
REM  It performs the following:
REM    1. Detects Windows version (Win 10/11 64-bit required)
REM    2. Copies the evernight.exe client to %ProgramFiles%
REM    3. Installs evernight as a Windows service (auto-start)
REM    4. Configures the USB-C NCM network adapter
REM    5. Registers this machine as a node with the gateway
REM    6. Opens the default browser to the gateway dashboard
REM ============================================================

setlocal enabledelayedexpansion

echo.
echo  ============================================
echo    Entelecheia Gateway — Evernight Installer
echo  ============================================
echo.

REM --- Check Windows version (require 10 or 11, 64-bit) ---
ver | findstr /R "10\." >nul
if %errorlevel% neq 0 (
    echo  [!] Windows 10 or later is required.
    echo      For older Windows, please use manual installation.
    pause
    exit /b 1
)

if not "%PROCESSOR_ARCHITECTURE%"=="AMD64" (
    echo  [!] 64-bit Windows is required.
    pause
    exit /b 1
)

echo  [ok] Windows %PROCESSOR_ARCHITECTURE% detected.

REM --- Determine USB drive root (parent of this script's directory) ---
REM %~dp0 is "E:\windows\" (with trailing backslash).
REM Strip the trailing backslash, then use for-loop to get parent dir.
set "SCRIPT_DIR=%~dp0"
set "SCRIPT_DIR=%SCRIPT_DIR:~0,-1%"
for %%i in ("%SCRIPT_DIR%") do set "USB_ROOT=%%~dpi"
set "USB_ROOT=%USB_ROOT:~0,-1%"

REM --- Install location ---
set "INSTALL_DIR=%ProgramFiles%\Entelecheia"
set "EVERNIGHT_EXE=%INSTALL_DIR%\evernight.exe"

echo  [..] Installing to %INSTALL_DIR% ...

if not exist "%INSTALL_DIR%" mkdir "%INSTALL_DIR%"

REM --- Copy the evernight binary ---
if exist "%USB_ROOT%\common\evernight-windows-amd64.exe" (
    copy /Y "%USB_ROOT%\common\evernight-windows-amd64.exe" "%EVERNIGHT_EXE%" >nul
    echo  [ok] evernight.exe installed.
) else (
    echo  [!!] evernight-windows-amd64.exe not found on the USB drive.
    echo       Please re-flash the gateway firmware.
    pause
    exit /b 1
)

REM --- Register as a Windows service (auto-start on boot) ---
echo  [..] Registering Windows service...

sc query EvernightGateway >nul 2>&1
if %errorlevel% equ 0 (
    echo  [..] Service already exists, updating...
    sc stop EvernightGateway >nul 2>&1
    sc delete EvernightGateway >nul 2>&1
)

sc create EvernightGateway binPath= "%EVERNIGHT_EXE% serve --mode client --gateway 10.0.99.1:50000" start= auto DisplayName= "Entelecheia Evernight Gateway Client" >nul
sc description EvernightGateway "Automatically connects this machine to the Entelecheia gateway via USB-C NCM." >nul
sc start EvernightGateway >nul

if %errorlevel% equ 0 (
    echo  [ok] Service installed and started.
) else (
    echo  [!!] Service registration failed. Try running as Administrator.
    pause
    exit /b 1
)

REM --- Open the gateway dashboard in the browser ---
echo  [..] Opening dashboard...
timeout /t 2 /nobreak >nul
start "" "http://10.0.99.1:8080"

echo.
echo  ============================================
echo    Installation complete!
echo  ============================================
echo.
echo  The Evernight client is now running as a service.
echo  Dashboard: http://10.0.99.1:8080
echo.
echo  To manage:  services.msc ^> EvernightGateway
echo  To remove:   sc delete EvernightGateway
echo.
pause
