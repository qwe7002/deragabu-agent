# 測試指南

## 問題修復

### ✅ 已修復: `protobuf.parse(...).then is not a function`

**原因**: protobuf.js 的 `parse()` 方法是同步的，不返回 Promise。

**修復**: 
```javascript
// ❌ 錯誤的用法
protobuf.parse(protoDefinition).then(root => { ... });

// ✅ 正確的用法
const root = protobuf.parse(protoDefinition).root;
```

## 測試客戶端

### 1. test-client.html (原版 - 已修復)
- 功能完整
- 詳細的統計信息
- 完整的日誌記錄
- **狀態**: ✅ 已修復 protobuf 錯誤

### 2. test-client-simple.html (新版 - 推薦)
- 更簡潔的界面
- 更好的錯誤處理
- 更美觀的設計
- 包含初始化檢查
- **狀態**: ✅ 全新實現

## 快速測試步驟

### 1. 啟動服務器

```bash
# 方法 1: 使用批次檔
.\run.bat

# 方法 2: 使用 cargo
cargo run --release

# 方法 3: 使用開發模式
.\run-dev.bat
```

### 2. 打開測試客戶端

在瀏覽器中打開以下任一文件：
- `test-client.html` - 原版（已修復）
- `test-client-simple.html` - 簡化版（推薦）

### 3. 連接測試

1. 確認 URL 為 `ws://127.0.0.1:9000`
2. 點擊 "連接" 按鈕
3. 等待連接成功（綠色狀態）
4. 移動滑鼠，觀察光標變化

### 4. 預期結果

✅ **成功標誌**:
- 狀態顯示為 "🟢 已連接"
- 看到 "Protobuf 定義加載完成" 日誌
- 移動滑鼠時，光標圖像實時更新
- FPS 計數器顯示 ~60
- 統計數據正常增長

❌ **常見問題**:
- "Protobuf 尚未加載" - 刷新頁面重試
- 連接失敗 - 確認服務器正在運行
- 無光標更新 - 檢查是否有光標變化

## 控制台日誌

打開瀏覽器開發者工具 (F12)，應該看到：

```
Protobuf 定義加載完成
[時間] 🔌 正在連接到 ws://127.0.0.1:9000...
[時間] ✅ WebSocket 連接成功
[時間] 🖱️ 光標更新: 32x32
[時間] 💓 收到心跳
```

## 測試場景

### 場景 1: 光標變化測試
1. 保持鼠標指針（箭頭）
2. 移動到文本框（I 型光標）
3. 移動到鏈接（手型光標）
4. 觀察光標圖像的變化

### 場景 2: 性能測試
1. 快速移動滑鼠
2. 觀察 FPS 保持在 ~60
3. 檢查 CPU 使用率（應該很低）

### 場景 3: 多客戶端測試
1. 在多個瀏覽器標籤頁打開測試客戶端
2. 全部連接到服務器
3. 所有客戶端應該同步更新

### 場景 4: 斷線重連測試
1. 點擊 "斷開" 按鈕
2. 等待幾秒
3. 點擊 "連接" 按鈕
4. 應該成功重新連接

## 性能指標

正常運行時應該看到：
- **FPS**: 55-60
- **延遲**: < 20ms
- **每幀大小**: 1-5 KB
- **CPU 使用**: < 5%
- **記憶體**: ~10 MB

## 故障排除

### 問題: 無法連接

```bash
# 檢查服務器是否運行
netstat -ano | findstr :9000

# 檢查防火牆
# Windows Defender -> 允許應用通過防火牆
```

### 問題: Protobuf 錯誤

- **test-client.html**: 已修復，直接使用即可
- **test-client-simple.html**: 包含更好的錯誤處理

### 問題: 光標不更新

1. 確認服務器正在運行
2. 確認光標實際有變化（移動到不同元素）
3. 檢查瀏覽器控制台是否有錯誤
4. 嘗試刷新頁面

## 開發者信息

### WebSocket 消息格式

```javascript
// 連接
ws = new WebSocket('ws://127.0.0.1:9000');
ws.binaryType = 'arraybuffer';

// 接收消息
ws.onmessage = (event) => {
    const data = new Uint8Array(event.data);
    const message = CursorMessage.decode(data);
    // message.type: 1=更新, 2=隱藏, 3=心跳
    // message.image_data: PNG 圖像數據
    // message.hotspot_x, hotspot_y: 熱點座標
    // message.width, height: 尺寸
    // message.timestamp: 時間戳
};
```

### Protobuf 結構

```protobuf
message CursorMessage {
    MessageType type = 1;
    bytes image_data = 2;
    int32 hotspot_x = 3;
    int32 hotspot_y = 4;
    int32 width = 5;
    int32 height = 6;
    uint64 timestamp = 7;
}
```

## 總結

✅ **兩個測試客戶端都可以正常使用了！**

- `test-client.html` - 已修復 protobuf.parse 錯誤
- `test-client-simple.html` - 新版本，更好的體驗

推薦使用 `test-client-simple.html` 進行測試，因為它有更好的錯誤處理和更美觀的界面。

