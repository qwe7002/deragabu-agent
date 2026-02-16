@echo off
echo ========================================
echo   Deragabu Agent - 光標捕獲服務器
echo ========================================
echo.
echo 正在啟動服務器...
echo WebSocket 地址: ws://127.0.0.1:9000
echo.
echo 提示:
echo - 在瀏覽器中打開 test-client.html 測試
echo - 按 Ctrl+C 停止服務器
echo.
echo ========================================
echo.

set RUST_LOG=info
.\target\release\deragabu-agent.exe

pause

