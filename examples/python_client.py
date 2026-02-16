"""
Deragabu Agent Python 客戶端範例

需要安裝:
    pip install websocket-client protobuf pillow
"""

import websocket
import cursor_pb2  # 需要從 cursor.proto 生成
from PIL import Image
import io
import time

def on_message(ws, message):
    """處理收到的消息"""
    # 解析 Protobuf
    cursor_msg = cursor_pb2.CursorMessage()
    cursor_msg.ParseFromString(message)

    if cursor_msg.type == cursor_pb2.MESSAGE_TYPE_CURSOR_UPDATE:
        # 保存 PNG
        timestamp = int(time.time() * 1000)
        filename = f"cursor_{timestamp}.png"

        with open(filename, 'wb') as f:
            f.write(cursor_msg.image_data)

        # 顯示信息
        img = Image.open(io.BytesIO(cursor_msg.image_data))
        print(f"收到光標: {cursor_msg.width}x{cursor_msg.height}, "
              f"熱點: ({cursor_msg.hotspot_x}, {cursor_msg.hotspot_y}), "
              f"大小: {len(cursor_msg.image_data)} bytes, "
              f"保存為: {filename}")

    elif cursor_msg.type == cursor_pb2.MESSAGE_TYPE_CURSOR_HIDE:
        print("光標已隱藏")

    elif cursor_msg.type == cursor_pb2.MESSAGE_TYPE_HEARTBEAT:
        print("收到心跳")

def on_error(ws, error):
    """處理錯誤"""
    print(f"錯誤: {error}")

def on_close(ws, close_status_code, close_msg):
    """連接關閉"""
    print("WebSocket 連接已關閉")

def on_open(ws):
    """連接成功"""
    print("WebSocket 連接成功")

if __name__ == "__main__":
    # 連接到 WebSocket 服務器
    ws_url = "ws://127.0.0.1:9000"
    print(f"正在連接到 {ws_url}...")

    ws = websocket.WebSocketApp(
        ws_url,
        on_open=on_open,
        on_message=on_message,
        on_error=on_error,
        on_close=on_close
    )

    # 運行
    ws.run_forever()

