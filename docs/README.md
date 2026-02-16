# Deragabu Agent

Rust 端光標捕獲代理，負責捕獲 Windows 光標、轉換為 PNG、並通過 WebSocket 服務器發送給客戶端。

## 功能

- 🖱️ **光標捕獲**：實時監聽 Windows 光標變化
- 🖼️ **PNG 編碼**：將光標圖像轉換為 PNG 格式
- 📦 **Protobuf 序列化**：使用 Protocol Buffers 高效序列化數據
- 🌐 **WebSocket 服務器**：通過 WebSocket 廣播光標數據給所有連接的客戶端

## 架構

```
光標捕獲 (cursor_capture.rs)
    ↓ (mpsc channel)
WebSocket 服務器 (websocket_server.rs)
    ↓ (broadcast channel)
連接的客戶端們
```

## 依賴

- **image**: 圖像處理和 PNG 編碼
- **prost**: Protocol Buffers 序列化
- **tokio-tungstenite**: 異步 WebSocket 服務器
- **windows**: Windows API 調用

## 安裝 Protocol Buffers 編譯器

### 方法 1: 使用 Chocolatey (Windows)

```powershell
choco install protoc
```

### 方法 2: 手動下載

1. 訪問 https://github.com/protocolbuffers/protobuf/releases
2. 下載最新的 `protoc-<version>-win64.zip`
3. 解壓並將 `bin\protoc.exe` 添加到 PATH

### 方法 3: 使用 Scoop

```powershell
scoop install protobuf
```

## 編譯

```bash
cargo build --release
```

## 快速開始

1. **安裝 Protocol Buffers 編譯器** (選擇一種方法)：
   ```powershell
   # 使用 winget (推薦)
   winget install --id Google.Protobuf -e
   
   # 或使用 Chocolatey
   choco install protoc -y
   
   # 或使用 Scoop
   scoop install protobuf
   ```

2. **編譯項目**：
   ```bash
   cargo build --release
   ```

3. **運行服務器**：
   ```bash
   cargo run --release
   ```

4. **測試客戶端**：
   
   提供三個測試客戶端選擇：
   
   - **test-client-v2.html** - ⭐ 推薦！全新設計，完整功能
     - 現代化 UI 設計
     - 實時緩存命中率顯示
     - 詳細的光標信息面板
     - 智能日誌管理
   
   - **test-client-simple.html** - 簡化版，支持緩存
   
   - **test-client.html** - 原始版本
   
   在瀏覽器中打開文件，點擊"連接"按鈕，移動滑鼠查看光標實時更新。

## 運行

```bash
# 使用默認地址和 WebP 格式 (推薦)
cargo run --release

# 使用 PNG 格式
$env:IMAGE_FORMAT="png"
cargo run --release

# 自定義 WebP 質量 (0-100, 默認: 80)
$env:WEBP_QUALITY="90"
cargo run --release

# 自定義綁定地址
$env:BIND_ADDR="0.0.0.0:8080"
cargo run --release
```

## 環境變數

- `BIND_ADDR`: WebSocket 服務器綁定地址（默認: `127.0.0.1:9000`）
- `IMAGE_FORMAT`: 圖像編碼格式 - `webp` (默認) 或 `png`
- `WEBP_QUALITY`: WebP 質量 (0-100, 默認: 80)
  - 0 = 無損壓縮 (文件更大但質量完美)
  - 1-100 = 有損壓縮 (數值越高質量越好但文件越大)
- `RUST_LOG`: 日誌級別（例如: `debug`, `info`, `warn`, `error`）

## 圖像格式對比

| 格式 | 壓縮率 | 質量 | 動畫支持 | 推薦場景 |
|------|--------|------|----------|----------|
| **WebP** (默認) | 優秀 (30-70% 更小) | 可配置 | ✅ 支持 | 一般使用，網絡傳輸 |
| PNG | 一般 | 無損 | ❌ 不支持 | 需要無損質量 |

### 性能測試範例

典型 32x32 光標：
- PNG: ~1.5-3 KB
- WebP (quality=80): ~0.5-1 KB (節省 60-70%)
- WebP (lossless): ~1-2 KB

## Protobuf 消息格式

```protobuf
message CursorMessage {
    MessageType type = 1;        // 消息類型
    bytes image_data = 2;        // 圖像數據 (PNG 或 WebP)
    int32 hotspot_x = 3;         // 光標熱點 X 座標
    int32 hotspot_y = 4;         // 光標熱點 Y 座標
    int32 width = 5;             // 圖像寬度
    int32 height = 6;            // 圖像高度
    uint64 timestamp = 7;        // 時間戳（毫秒）
    ImageFormat image_format = 8; // 圖像格式
}

enum MessageType {
    MESSAGE_TYPE_UNSPECIFIED = 0;
    MESSAGE_TYPE_CURSOR_UPDATE = 1;   // 光標更新
    MESSAGE_TYPE_CURSOR_HIDE = 2;     // 光標隱藏
    MESSAGE_TYPE_HEARTBEAT = 3;       // 心跳
}

enum ImageFormat {
    IMAGE_FORMAT_PNG = 0;          // PNG 格式
    IMAGE_FORMAT_WEBP = 1;         // WebP 格式
    IMAGE_FORMAT_WEBP_ANIMATED = 2; // WebP 動畫格式 (未來支持)
}
```


## WebSocket 協議

- **連接**: `ws://<BIND_ADDR>`
- **消息格式**: 二進制 (Protobuf 編碼的 CursorMessage)
- **心跳**: 服務器每 30 秒發送一次 Ping

## 客戶端範例

### JavaScript/Browser

```javascript
const ws = new WebSocket('ws://127.0.0.1:9000');
ws.binaryType = 'arraybuffer';

ws.onmessage = async (event) => {
    // 使用 protobuf.js 解析
    const message = CursorMessage.decode(new Uint8Array(event.data));
    
    if (message.type === MessageType.CURSOR_UPDATE) {
        // 將 PNG 數據轉換為圖像
        const blob = new Blob([message.imageData], { type: 'image/png' });
        const url = URL.createObjectURL(blob);
        
        // 顯示光標
        console.log(`光標: ${message.width}x${message.height}, 熱點: (${message.hotspotX}, ${message.hotspotY})`);
    }
};
```

### Python

```python
import websocket
import cursor_pb2  # 從 .proto 生成

def on_message(ws, message):
    cursor_msg = cursor_pb2.CursorMessage()
    cursor_msg.ParseFromString(message)
    
    if cursor_msg.type == cursor_pb2.MESSAGE_TYPE_CURSOR_UPDATE:
        # 保存 PNG
        with open('cursor.png', 'wb') as f:
            f.write(cursor_msg.image_data)

ws = websocket.WebSocketApp('ws://127.0.0.1:9000',
                           on_message=on_message)
ws.run_forever()
```

## 性能

- 捕獲頻率: ~60 FPS (16ms 間隔)
- 只在光標變化時發送數據
- PNG 壓縮減少帶寬使用
- 支援多個客戶端同時連接

## 許可證

MIT

