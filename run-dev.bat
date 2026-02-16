@echo off
echo ========================================
echo   Deragabu Agent - 開發模式
echo ========================================
echo.
echo 正在編譯並運行 (Debug 模式)...
echo.

set RUST_LOG=debug
cargo run

pause

