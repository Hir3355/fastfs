@echo off
setlocal EnableExtensions

rem Keep the runtime module and remove generated build artifacts only.
set "ROOT=%~dp0"
set "RUNTIME=%ROOT%dist\FastFs"

if not exist "%RUNTIME%\FastFs.PowerShell.dll" goto :missing_runtime
if not exist "%RUNTIME%\fastfs.dll" goto :missing_runtime
if not exist "%RUNTIME%\FastFs.psd1" goto :missing_runtime

echo This will remove generated build artifacts from:
echo   %ROOT%target
echo   %ROOT%powershell\FastFs.PowerShell\bin
echo   %ROOT%powershell\FastFs.PowerShell\obj
echo The runtime module in %RUNTIME% will be kept.
choice /C YN /N /M "Continue? [Y/N] "
if errorlevel 2 exit /b 0

if exist "%ROOT%target" rd /s /q "%ROOT%target"
if exist "%ROOT%powershell\FastFs.PowerShell\bin" rd /s /q "%ROOT%powershell\FastFs.PowerShell\bin"
if exist "%ROOT%powershell\FastFs.PowerShell\obj" rd /s /q "%ROOT%powershell\FastFs.PowerShell\obj"

echo Cleanup completed. Runtime files were kept.
exit /b 0

:missing_runtime
echo Runtime module was not found under:
echo   %RUNTIME%
echo Build the project first, then run this file again.
exit /b 1
