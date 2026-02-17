@echo off
echo ========================================
echo   Deragabu Agent - 光標捕獲服務器
echo ========================================
echo.
echo 正在啟動服務器...
echo WebRTC 信令地址: http://127.0.0.1:9000
echo.
echo 提示:
echo - 在瀏覽器中打開 http://127.0.0.1:9000 測試
echo - 按 Ctrl+C 停止服務器
echo.
echo ========================================
echo.

set RUST_LOG=info
.\target\release\deragabu-agent.exe

pause

