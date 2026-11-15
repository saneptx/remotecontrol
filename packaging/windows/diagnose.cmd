@echo off
rem Diagnose connection problems (asks for administrator rights to read the log).
cd /d "%~dp0"
net session >nul 2>&1
if errorlevel 1 (
  powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
  exit /b
)
rc-host.exe status
echo.
pause
