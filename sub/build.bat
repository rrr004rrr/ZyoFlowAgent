@echo off
REM Build single-file zyoflow-sub.exe (also copies to ..\dist\). Run on your own PC -- nothing to install.
cargo build --release
if not exist ..\dist mkdir ..\dist
copy /Y target\release\zyoflow-sub.exe ..\dist\zyoflow-sub.exe >nul
echo.
echo Done: sub\target\release\zyoflow-sub.exe  (also copied to dist\zyoflow-sub.exe)
