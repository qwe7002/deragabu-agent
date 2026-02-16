# Python 客戶端使用說明

## 生成 Protobuf 代碼

首先需要從 `.proto` 文件生成 Python 代碼：

```bash
# 安裝 protobuf 編譯器和 Python 工具
pip install protobuf grpcio-tools

# 生成 Python 代碼
protoc --python_out=. --proto_path=../proto ../proto/cursor.proto
```

這會生成 `cursor_pb2.py` 文件。

## 安裝依賴

```bash
pip install websocket-client protobuf pillow
```

## 運行客戶端

```bash
python python_client.py
```

## 客戶端功能

- 連接到 WebSocket 服務器 (ws://127.0.0.1:9000)
- 接收光標消息
- 將光標圖像保存為 PNG 文件
- 顯示光標信息 (尺寸、熱點、大小)

## 輸出範例

```
正在連接到 ws://127.0.0.1:9000...
WebSocket 連接成功
收到光標: 32x32, 熱點: (0, 0), 大小: 1234 bytes, 保存為: cursor_1234567890.png
收到光標: 24x24, 熱點: (12, 12), 大小: 987 bytes, 保存為: cursor_1234567891.png
光標已隱藏
收到心跳
```

## 自定義

你可以修改 `python_client.py` 來：
- 顯示光標圖像 (使用 Pillow)
- 將光標應用到 GUI 應用
- 記錄光標變化歷史
- 實現光標同步功能

