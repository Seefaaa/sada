@echo off
setlocal
pushd "%~dp0\..\..\" || exit /b 1

if not defined BYOND_PATH (
    for /f "tokens=2,*" %%a in ('reg query "HKLM\SOFTWARE\WOW6432Node\Dantom\BYOND" /v InstallPath 2^>nul') do set "BYOND_PATH=%%b"
)

if not defined BYOND_PATH (
    echo BYOND_PATH is not set and could not be determined automatically. Please set it to the path of your BYOND installation.
    exit /b 1
)

if not exist "%BYOND_PATH%\bin\dm.exe" (
    echo BYOND_PATH is set to "%BYOND_PATH%", but dm.exe was not found in that directory. Please check your BYOND installation.
    exit /b 1
)

if not exist "%BYOND_PATH%\bin\dd.exe" (
    echo BYOND_PATH is set to "%BYOND_PATH%", but dd.exe was not found in that directory. Please check your BYOND installation.
    exit /b 1
)

cargo build -p sada-client-async --target i686-pc-windows-msvc || exit /b 1
"%BYOND_PATH%\bin\dm" sada-client-async/dmsrc/async.dme && "%BYOND_PATH%\bin\dd" sada-client-async/dmsrc/async.dmb -trusted || exit /b 1

popd
exit /b 0
