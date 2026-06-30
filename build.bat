@echo off
REM Build single-file dist\zyoflow.exe (bundles web/ + lark-oapi). Copy to station, run -- no Python needed.
REM 一律用 venv 的 python 顯式呼叫，避免 PATH 解析到 user-site 的 pyinstaller（會分析錯環境、漏掉 Flask）。
venv\Scripts\python.exe -m pip install -q pyinstaller lark-oapi
REM --collect-all lark_oapi：把 lark-oapi 子模組/資料(protobuf 等)全收進來，否則長連接缺檔
venv\Scripts\python.exe -m PyInstaller --onefile --name zyoflow --add-data "web;web" --collect-all lark_oapi --console app.py
echo.
echo Done: dist\zyoflow.exe  (copy to your station host and run)
