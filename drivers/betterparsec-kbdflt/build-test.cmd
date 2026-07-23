@echo off
setlocal
set "ROOT=%~dp0"
set "KIT=C:\Program Files (x86)\Windows Kits\10"
set "VSDEVCMD=C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
if not exist "%VSDEVCMD%" (
  echo Visual Studio 2022 C++ tools not found.
  exit /b 2
)
call "%VSDEVCMD%" -arch=x64 -host_arch=x64
if errorlevel 1 exit /b %errorlevel%
if not exist "%ROOT%out\x64" mkdir "%ROOT%out\x64"
cl.exe /nologo /c /kernel /W4 /WX /wd4324 /GS /D_AMD64_ /DAMD64 /DNTDDI_VERSION=0x0A00000C /D_WIN32_WINNT=0x0A00 /I"%ROOT%include" /I"%KIT%\Include\10.0.26100.0\km" /I"%KIT%\Include\10.0.26100.0\shared" /I"%KIT%\Include\wdf\kmdf\1.33" /Fo"%ROOT%out\x64\driver.obj" "%ROOT%src\driver.c"
if errorlevel 1 exit /b %errorlevel%
link.exe /nologo /driver /machine:x64 /subsystem:native,10.00 /entry:FxDriverEntry /nodefaultlib /debug /pdb:"%ROOT%out\x64\betterparsec-kbdflt.pdb" /out:"%ROOT%out\x64\betterparsec-kbdflt.sys" /libpath:"%KIT%\Lib\10.0.26100.0\km\x64" /libpath:"%KIT%\Lib\wdf\kmdf\x64\1.33" "%ROOT%out\x64\driver.obj" BufferOverflowK.lib ntoskrnl.lib hal.lib wmilib.lib wdmsec.lib WdfLdr.lib WdfDriverEntry.lib
if errorlevel 1 exit /b %errorlevel%
echo Built %ROOT%out\x64\betterparsec-kbdflt.sys
