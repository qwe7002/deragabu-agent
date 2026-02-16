# 項目架構說明

## 概述

Deragabu Agent 是一個 Windows 光標捕獲服務，能夠實時捕獲系統光標並通過 WebSocket 服務器廣播給連接的客戶端。

## 技術棧

- **語言**: Rust (Edition 2021)
- **異步運行時**: Tokio
- **WebSocket**: tokio-tungstenite
- **序列化**: Protocol Buffers (prost)
- **圖像處理**: image crate
- **Windows API**: windows-rs

## 模組說明

### 1. main.rs
主程式入口，負責：
- 初始化日誌系統
- 創建通信通道 (mpsc)
- 啟動光標捕獲任務
- 啟動 WebSocket 服務器任務

### 2. cursor_capture.rs
光標捕獲模組，負責：
- 使用 Windows API 捕獲光標句柄 (HCURSOR)
- 將光標圖標轉換為 RGBA 圖像
- 處理彩色光標和單色光標
- 編碼為 PNG 格式
- 通過 mpsc 通道發送給 WebSocket 服務器

**關鍵函數**：
- `run_cursor_capture()`: 主循環，每 16ms 檢查一次光標
- `capture_cursor()`: 捕獲當前光標並轉換為消息
- `get_cursor_image()`: 從 HCURSOR 獲取圖像數據
- `get_bitmap_image()`: 處理彩色位圖
- `get_monochrome_cursor_image()`: 處理單色光標
- `encode_png()`: 將圖像編碼為 PNG

### 3. websocket_server.rs
WebSocket 服務器模組，負責：
- 監聽 TCP 連接
- 執行 WebSocket 握手
- 將光標消息廣播給所有連接的客戶端
- 處理客戶端心跳

**關鍵函數**：
- `run_websocket_server()`: 啟動服務器並接受連接
- `handle_client()`: 處理單個客戶端連接

## 數據流

```
┌─────────────────┐
│  Windows 系統   │
│   (光標狀態)    │
└────────┬────────┘
         │
         ▼
┌─────────────────────────┐
│  cursor_capture.rs      │
│  - GetCursorInfo()      │
│  - GetIconInfo()        │
│  - GetDIBits()          │
│  - PNG 編碼             │
└────────┬────────────────┘
         │ mpsc::Sender
         ▼
┌─────────────────────────┐
│  websocket_server.rs    │
│  - broadcast::channel   │
│  - TcpListener          │
└────────┬────────────────┘
         │ WebSocket (Binary)
         ▼
┌─────────────────────────┐
│  客戶端 (瀏覽器/應用)   │
│  - Protobuf 解碼        │
│  - 顯示光標圖像         │
└─────────────────────────┘
```

## Protobuf 消息定義

位於 `proto/cursor.proto`：

- **CursorMessage**: 光標消息
  - `type`: 消息類型 (更新/隱藏/心跳)
  - `image_data`: PNG 圖像數據
  - `hotspot_x/y`: 光標熱點座標
  - `width/height`: 圖像尺寸
  - `timestamp`: 時間戳

## 性能優化

1. **增量更新**: 只在光標變化時發送消息
2. **PNG 壓縮**: 減少網絡帶寬
3. **廣播模式**: 支援多客戶端，數據只編碼一次
4. **異步 I/O**: 使用 Tokio 實現高效並發

## 構建過程

1. **build.rs**: 執行 `prost-build` 編譯 `.proto` 文件
2. 生成的 Rust 代碼位於 `$OUT_DIR/cursor.rs`
3. 通過 `include!()` 宏引入生成的代碼

## 環境變數

- `BIND_ADDR`: WebSocket 綁定地址 (默認: 127.0.0.1:9000)
- `RUST_LOG`: 日誌級別 (trace/debug/info/warn/error)
- `PROTOC`: Protobuf 編譯器路徑 (可選)

## 依賴說明

### 運行時依賴
- `tokio`: 異步運行時
- `tokio-tungstenite`: WebSocket 實現
- `prost`: Protobuf 運行時
- `image`: 圖像處理和 PNG 編碼
- `windows`: Windows API 綁定
- `anyhow`: 錯誤處理
- `tracing`: 結構化日誌

### 構建依賴
- `prost-build`: Protobuf 編譯器

## 測試

使用 `test-client.html` 測試：
1. 運行 `cargo run --release`
2. 在瀏覽器中打開 `test-client.html`
3. 點擊"連接"按鈕
4. 移動滑鼠觀察光標變化

## 已知限制

1. 僅支援 Windows 平台
2. 需要安裝 Protocol Buffers 編譯器
3. 光標捕獲頻率固定為 ~60 FPS
4. 部分特殊光標可能無法正確捕獲

## 未來改進

- [ ] 支援自定義捕獲頻率
- [ ] 添加 HTTPS/WSS 支援
- [ ] 優化記憶體使用
- [ ] 添加性能監控指標
- [ ] 支援光標位置追蹤
- [ ] 添加配置文件支援

