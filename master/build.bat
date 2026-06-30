@echo off
REM Build single-file zyoflow-master.exe (also copies to ..\dist\). Run on your own PC -- nothing to install.
cargo build --release
if not exist ..\dist mkdir ..\dist
copy /Y target\release\zyoflow-master.exe ..\dist\zyoflow-master.exe >nul
echo.
echo Done: master\target\release\zyoflow-master.exe  (also copied to dist\zyoflow-master.exe)
