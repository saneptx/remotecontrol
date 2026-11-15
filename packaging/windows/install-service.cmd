@echo off
rem Install rc-host as a Windows service (asks for administrator rights).
cd /d "%~dp0"
net session >nul 2>&1
if errorlevel 1 (
  powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
  exit /b
)
rc-host.exe install %*
echo.
pause
