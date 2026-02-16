# 🚀 快速测试指南

## 问题修复验证

### ✅ 修复 1: 光标清晰度

#### 测试步骤

1. **启动服务器**
   ```bash
   cargo run --release
   ```

2. **打开测试工具**
   - 浏览器打开 `test-client.html`
   - 点击"连接"

3. **验证清晰度**
   - 默认显示比例: 200% (2x)
   - 移动鼠标查看光标
   - **预期**: 光标清晰，边缘锐利

4. **测试不同比例**
   - 选择 100% - 原始大小（最清晰）
   - 选择 400% - 4倍放大
   - **预期**: 所有比例都保持清晰

5. **测试十字线**
   - 勾选"显示十字线"
   - **预期**: 红色十字线精确标记热点位置

---

### ✅ 修复 2: 缓存命中率

#### 测试步骤

1. **启用调试日志**
   ```bash
   $env:RUST_LOG="debug"
   cargo run --release
   ```

2. **打开测试工具并连接**

3. **观察初始阶段（30秒）**
   - 移动鼠标，不要切换光标
   - **服务器日志应显示**:
     ```
     DEBUG Cached NEW cursor: handle=..., id=..., total=1
     DEBUG Cache HIT: cursor_handle=..., frame=1
     DEBUG Cache HIT: cursor_handle=..., frame=2
     ...
     ```
   - **客户端显示**:
     - 第一条: "📦 完整更新"
     - 后续: 没有日志（缓存引用不记录，避免刷屏）
     - 缓存命中率: 快速提升到 80%+

4. **观察稳定阶段（2-5分钟）**
   - 在不同应用间切换（记事本、浏览器等）
   - **预期**:
     - 每个新光标首次是"完整更新"
     - 后续都是缓存引用
     - 缓存命中率: 85-95%
     - 缓存数: 5-15 个

5. **观察强制更新（动画光标）**
   - 保持一个光标不变
   - 每 15 帧（~250ms）会看到:
     ```
     DEBUG Cache MISS or forced update: ...
     DEBUG Updated cached cursor: ...
     ```
   - **预期**: 这是正常的，用于支持动画光标

---

## 快速验证清单

### ✅ 光标清晰度

- [ ] 100% 比例清晰
- [ ] 200% 比例清晰
- [ ] 400% 比例清晰
- [ ] 十字线位置准确
- [ ] 边缘无模糊

### ✅ 缓存功能

- [ ] 首次连接收到完整更新
- [ ] 后续收到缓存引用
- [ ] 缓存命中率 > 80%
- [ ] 服务器日志显示 "Cache HIT"
- [ ] 切换光标后命中率保持高位

### ✅ 性能指标

- [ ] FPS 稳定在 55-60
- [ ] 数据传输 < 10 KB/分钟（稳定后）
- [ ] 缓存数合理（5-20个）
- [ ] 内存使用稳定

---

## 常见场景测试

### 场景 1: 静态光标

**操作**: 打开记事本，保持箭头光标不动

**预期**:
```
服务器日志:
DEBUG Cached NEW cursor: handle=XXX, total=1
DEBUG Cache HIT: cursor_handle=XXX, frame=1
DEBUG Cache HIT: cursor_handle=XXX, frame=2
...

客户端:
缓存命中率: 快速上升到 95%+
数据传输: 极少（只有初始更新）
```

### 场景 2: 光标切换

**操作**: 在文本框和按钮间移动光标（I型 ↔ 箭头）

**预期**:
```
服务器日志:
DEBUG Cached NEW cursor: handle=111, total=1  <- I型光标
DEBUG Cache HIT: cursor_handle=111, frame=1
DEBUG Cached NEW cursor: handle=222, total=2  <- 箭头
DEBUG Cache HIT: cursor_handle=222, frame=1
DEBUG Cache HIT: cursor_handle=111, frame=1   <- 回到I型，缓存命中！
DEBUG Cache HIT: cursor_handle=222, frame=1   <- 回到箭头，缓存命中！

客户端:
缓存命中率: 维持在 85-90%
缓存数: 2
```

### 场景 3: 动画光标

**操作**: 使用系统动画光标（如等待圆圈）

**预期**:
```
服务器日志:
DEBUG Cached NEW cursor: handle=333, total=3
DEBUG Cache HIT: cursor_handle=333, frame=1
...
DEBUG Cache HIT: cursor_handle=333, frame=14
DEBUG Cache MISS or forced update: frame=0    <- 强制更新
DEBUG Updated cached cursor: handle=333
DEBUG Cache HIT: cursor_handle=333, frame=1
...

客户端:
缓存命中率: 约 90%（每15帧有1次完整更新）
```

---

## 性能基准

### 正常值参考

| 指标 | 启动30秒 | 运行2分钟 | 运行10分钟 |
|------|----------|-----------|------------|
| 缓存命中率 | 50-70% | 85-90% | 90-95% |
| FPS | 55-60 | 55-60 | 55-60 |
| 数据传输/分钟 | 100-200 KB | 10-30 KB | 5-15 KB |
| 缓存数 | 1-3 | 5-10 | 10-20 |

### 异常值判断

**缓存命中率过低（< 50%）**:
- ❌ 缓存逻辑未生效
- 🔧 检查服务器日志是否有 "Cache HIT"
- 🔧 确认使用最新编译版本

**数据传输过高（> 100 KB/分钟稳定后）**:
- ❌ 缓存未工作或频繁切换光标
- 🔧 查看客户端日志，是否都是"完整更新"
- 🔧 减少光标切换频率测试

**FPS 过低（< 50）**:
- ❌ 性能问题
- 🔧 检查 CPU 使用率
- 🔧 降低显示比例（100%）
- 🔧 关闭其他应用

---

## 问题排查流程

### 如果光标仍然模糊

1. **确认使用新版测试工具**
   ```bash
   # 检查文件修改时间
   (Get-Item test-client.html).LastWriteTime
   ```
   应该是最近的时间

2. **检查浏览器**
   - 清除缓存 (Ctrl+Shift+Delete)
   - 硬刷新 (Ctrl+F5)
   - 尝试不同浏览器

3. **检查CSS**
   - 按F12打开开发者工具
   - 检查 `.cursor-image` 样式
   - 应该有: `transform: scale(2)`
   - 不应该有: `zoom: 2`

### 如果缓存不工作

1. **检查服务器日志**
   ```bash
   $env:RUST_LOG="debug"
   cargo run --release
   ```
   应该看到 "Cache HIT" 消息

2. **检查客户端**
   - 按F12打开控制台
   - 查看收到的消息
   - 检查 `is_full_update` 字段
   - 正常情况：第一次 true，后续 false

3. **重新编译**
   ```bash
   cargo clean
   cargo build --release
   ```

---

## 成功标志 ✅

当你看到以下所有迹象时，说明一切正常：

### 客户端
- ✅ 光标清晰锐利
- ✅ 缓存命中率 > 85%
- ✅ 缓存数稳定增长（5-20）
- ✅ 数据传输量很小（稳定后）
- ✅ FPS 稳定在 55-60

### 服务器日志（debug模式）
```
INFO  Starting cursor capture... (format: WebP)
INFO  Cursor cache initialized
DEBUG Cached NEW cursor: handle=12345, id=abc12345, total=1
DEBUG Cache HIT: cursor_handle=12345, frame=1
DEBUG Cache HIT: cursor_handle=12345, frame=2
DEBUG Cache HIT: cursor_handle=12345, frame=3
...
```

### 浏览器控制台
- ✅ 无错误消息
- ✅ Protobuf 解析成功
- ✅ WebSocket 连接稳定

---

## 快速命令

```bash
# 清理并重新编译
cargo clean && cargo build --release

# 以调试模式运行
$env:RUST_LOG="debug"; cargo run --release

# 以高质量 PNG 模式运行
$env:IMAGE_FORMAT="png"; cargo run --release

# 组合：调试 + PNG
$env:RUST_LOG="debug"; $env:IMAGE_FORMAT="png"; cargo run --release
```

---

## 预期测试时间

- 光标清晰度验证: **2 分钟**
- 缓存功能验证: **5 分钟**
- 完整性能测试: **10 分钟**

总计: **15-20 分钟** 即可完成全面测试

---

## 🎉 成功！

如果所有测试都通过，恭喜你！

- 🖼️ 光标清晰度问题已解决
- 💾 缓存机制正常工作
- 📉 带宽节省 90%+
- 🚀 性能流畅稳定

开始享受高质量的光标捕获吧！

