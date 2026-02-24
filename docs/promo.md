## Plan: Sunshine macOS 光標控制 — Agent 整合進 Sunshine 統一編譯

**TL;DR**: 將 deragabu-agent 編譯為靜態庫 (`.a`)，連結進 Sunshine 的 CMake 構建系統。Sunshine 通過 C FFI 呼叫 agent 的光標捕獲、WebRTC overlay、剪貼簿同步等功能。macOS 使用 AVFoundation (`AVCaptureSession` + `AVCaptureScreenInput`)，在 session 建立時設定 `capturesCursor = NO`（best-effort，見已知限制）。Agent 的軟光標 overlay 是主要光標機制，不依賴 `capturesCursor` 的可靠性。

---

### Architecture

```
┌─────────────────────────────────────────────────┐
│               Sunshine (C++ / ObjC)             │
│                                                 │
│  main.cpp ──► deragabu_agent_init()             │
│  display.mm ► !*cursor → agent overlay control  │
│  input ─────► deragabu_agent_set_display_cursor │
│  shutdown ──► deragabu_agent_shutdown()          │
│                                                 │
│         ┌──────────────────────────┐            │
│         │  libderagabu_agent.a     │            │
│         │  (Rust static lib)       │            │
│         │                          │            │
│         │  ┌─ cursor_capture ────┐ │            │
│         │  ├─ webrtc_server ─────┤ │            │
│         │  ├─ clipboard_sync ────┤ │            │
│         │  └─ sunshine_monitor ──┘ │            │
│         └──────────────────────────┘            │
└─────────────────────────────────────────────────┘
```

**單一進程、統一編譯、零 IPC 開銷。**

---

### Steps

#### Part A — Agent 端修改 (deragabu-agent → 靜態庫)

**1. Cargo.toml — 新增 `[lib]` 和 `[[bin]]`，輸出 staticlib**

```toml
[lib]
name = "deragabu_agent"
crate-type = ["staticlib", "rlib"]

[[bin]]
name = "deragabu-agent"
path = "src/main.rs"
```

`staticlib` 在 macOS 產出 `libderagabu_agent.a`，可直接連結進 C++ 二進制。`rlib` 保留讓 `main.rs` 繼續作為獨立二進制使用。

**2. 建立 `src/lib.rs` — 庫入口**

將 `src/main.rs` 中的模組宣告和 `AgentEvent` enum 搬入 `src/lib.rs`：

```rust
pub mod clipboard_sync;
pub mod cursor_capture;
pub mod sunshine_monitor;
pub mod webrtc_server;
pub mod ffi;

pub mod cursor {
    include!(concat!(env!("OUT_DIR"), "/cursor.rs"));
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Cursor(cursor_capture::CursorEvent),
    Clipboard(clipboard_sync::ClipboardEvent),
    Settings(sunshine_monitor::SunshineSettingsEvent),
}
```

`main.rs` 改為 `use deragabu_agent::*;` 重用 lib 的所有邏輯。

**3. 建立 `src/ffi.rs` — C FFI 介面**

暴露 `extern "C"` 函數，Sunshine 直接呼叫：

| 函數 | 說明 |
|------|------|
| `deragabu_agent_init(bind_addr: *const c_char) -> i32` | 在新 tokio runtime 上啟動 WebRTC server + 光標捕獲 + 剪貼簿同步 + sunshine monitor |
| `deragabu_agent_shutdown()` | 停止所有任務、銷毀 runtime |
| `deragabu_agent_set_display_cursor(display: bool)` | Sunshine 推送 `display_cursor` 狀態（FFI → AtomicBool → sunshine monitor task → 廣播給客戶端） |
| `deragabu_agent_is_running() -> bool` | 健康檢查 |

```rust
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn deragabu_agent_init(bind_addr: *const c_char) -> i32 {
    let addr = if bind_addr.is_null() {
        "0.0.0.0:9000".to_string()
    } else {
        unsafe { CStr::from_ptr(bind_addr) }
            .to_str()
            .unwrap_or("0.0.0.0:9000")
            .to_string()
    };

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return -1,
    };

    rt.spawn(async move {
        // 啟動所有子系統（與 main.rs 相同邏輯）
        crate::start_all_subsystems(addr).await;
    });

    let _ = RUNTIME.set(rt);
    RUNNING.store(true, Ordering::SeqCst);
    0
}

#[no_mangle]
pub extern "C" fn deragabu_agent_set_display_cursor(display: bool) {
    crate::sunshine_monitor::set_display_cursor_from_ffi(display);
}

#[no_mangle]
pub extern "C" fn deragabu_agent_shutdown() {
    RUNNING.store(false, Ordering::SeqCst);
    // Drop runtime 會等待所有 task 結束
}

#[no_mangle]
pub extern "C" fn deragabu_agent_is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}
```

**4. 修改 `src/sunshine_monitor/mod.rs` — FFI push 模式**

新增全域 `AtomicBool` 和公開 setter，macOS 的 monitor task 輪詢偵測變化：

```rust
use std::sync::atomic::{AtomicBool, Ordering};

static DISPLAY_CURSOR: AtomicBool = AtomicBool::new(true);
static FFI_MODE: AtomicBool = AtomicBool::new(false);

/// 由 FFI 呼叫，設定 display_cursor 狀態
pub fn set_display_cursor_from_ffi(val: bool) {
    FFI_MODE.store(true, Ordering::SeqCst);
    DISPLAY_CURSOR.store(val, Ordering::SeqCst);
}

/// macOS / FFI 模式的 monitor：輪詢 AtomicBool 偵測狀態變化
#[cfg(not(target_os = "windows"))]
pub async fn run_sunshine_monitor(tx: mpsc::Sender<SunshineSettingsEvent>) -> Result<()> {
    let mut last_value = true;
    let _ = tx.send(SunshineSettingsEvent { draw_cursor: true }).await;

    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current = DISPLAY_CURSOR.load(Ordering::SeqCst);
        if current != last_value {
            info!("display_cursor changed to {} (via FFI)", current);
            let _ = tx.send(SunshineSettingsEvent { draw_cursor: current }).await;
            last_value = current;
        }
    }
}
```

Windows 保留原有的記憶體讀取邏輯不變。

**5. 生成 C 標頭檔 `include/deragabu_agent.h`**

```c
#pragma once
#ifdef __cplusplus
extern "C" {
#endif

/// 啟動 agent（光標捕獲 + WebRTC server + 剪貼簿同步）
/// bind_addr: WebRTC 信令伺服器綁定地址，如 "0.0.0.0:9000"；NULL 使用預設值
/// 返回 0 成功，-1 失敗
int deragabu_agent_init(const char* bind_addr);

/// 停止 agent，釋放所有資源
void deragabu_agent_shutdown(void);

/// 設定 display_cursor 狀態（Sunshine 光標繪製開關）
/// display=true: Sunshine 在視頻流中繪製光標，客戶端隱藏 overlay
/// display=false: Sunshine 不繪製光標，客戶端顯示 agent overlay
void deragabu_agent_set_display_cursor(bool display);

/// 檢查 agent 是否正在運行
bool deragabu_agent_is_running(void);

#ifdef __cplusplus
}
#endif
```

**6. 提取共用啟動邏輯 `start_all_subsystems()`**

在 `src/lib.rs` 中新增 `pub async fn start_all_subsystems(bind_addr: String)`，將 `main.rs` 裡的 channel 建立、任務 spawn、`tokio::select!` 等邏輯搬過來。`main.rs` 和 `ffi.rs` 都調用這個函數。

---

#### Part B — Sunshine CMake 整合

**7. 將 deragabu-agent 作為 git submodule 加入 Sunshine**

```bash
cd sunshine/
git submodule add https://github.com/user/deragabu-agent.git third-party/deragabu-agent
```

**8. 加入 corrosion CMake 模組**

在 Sunshine 的 `CMakeLists.txt` 中：

```cmake
# ── Deragabu Agent (Rust → 靜態庫) ──────────────────────────
if(APPLE)
    include(FetchContent)
    FetchContent_Declare(
        Corrosion
        GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
        GIT_TAG v0.5
    )
    FetchContent_MakeAvailable(Corrosion)

    corrosion_import_crate(MANIFEST_PATH third-party/deragabu-agent/Cargo.toml)
    corrosion_set_hostbuild(deragabu_agent)

    target_link_libraries(sunshine PRIVATE deragabu_agent)
    target_include_directories(sunshine PRIVATE
        ${CMAKE_SOURCE_DIR}/third-party/deragabu-agent/include
    )
endif()
```

Corrosion 會自動執行 `cargo build --release`，產出 `libderagabu_agent.a`，CMake 連結進 Sunshine 二進制。**一次 `cmake --build build` 完成全部。**

---

#### Part C — Sunshine 源碼修改

**9. 修改 macOS 視頻捕獲 — `capturesCursor = NO` (once at init)**

Sunshine macOS 使用 AVFoundation (`AVCaptureSession` + `AVCaptureScreenInput`)。

> **⚠️ 已知限制**：`AVCaptureScreenInput.capturesCursor` 存在以下問題：
> - 動態切換需要 `beginConfiguration/commitConfiguration`（本質上暫停/恢復 session），在低延遲串流中代價過大
> - **macOS bug**：若用戶在「輔助功能」中調整了游標大小，`capturesCursor = NO` 會被**靜默忽略**，硬體游標仍會出現在捕獲的視頻中
> - 沒有已知的 workaround
>
> 因此 agent 的軟光標 overlay 是**唯一可靠**的光標機制。`capturesCursor = NO` 只是 best-effort 的硬體游標隱藏。

**`av_video.m`** — 在 session 初始化時一次性設定（不動態切換）：
```objc
AVCaptureScreenInput *screenInput = [[AVCaptureScreenInput alloc] initWithDisplayID:self.displayID];

// Deragabu Agent: hide hardware cursor from capture (best-effort).
// NOTE: Known macOS bug — if Accessibility cursor size != default,
//       this setting may be silently ignored.
[screenInput setCapturesCursor:NO];
```

**`display.mm`** — 控制 overlay，不觸碰 `capturesCursor`：
```objc
#include "deragabu_agent.h"

// 在 capture() 方法的循環中
static bool last_cursor_state = true;
bool want_cursor = cursor ? *cursor : true;
if (want_cursor != last_cursor_state) {
  if (deragabu_agent_is_running()) {
    // Direct mapping: true = show overlay, false = hide overlay
    deragabu_agent_set_display_cursor(want_cursor);
  }
  last_cursor_state = want_cursor;
}
```

> **語義說明**：FFI 的 `display` 參數語義是「是否向用戶顯示 overlay 光標」。直接傳遞 `*cursor`，無需任何反轉。
> Windows 端的反轉在 agent 內部處理（Sunshine `display_cursor=true` → HW 光標在視頻流 → overlay 不需要 → `draw_cursor=false`）。
>
> **已知副作用**：若 macOS bug 觸發（`capturesCursor=NO` 被忽略，硬體游標仍在視頻流中），`want_cursor=true` 時會出現「雙光標」（硬體 + overlay）。這是 Apple API 的限制。

**10. `display_cursor` 切換 — overlay 即時、硬體游標 best-effort**

切換 `display_cursor` 時：
1. `display.mm` 的捕獲循環直接將 `*cursor` 推送給 agent FFI → 廣播 `SettingsData` → 客戶端 overlay **即時響應**
2. 硬體游標狀態**不變**（`capturesCursor = NO` 在 init 時設定，不動態切換）

> **注意**：如果 macOS bug 導致硬體游標仍出現在視頻流中，用戶在 overlay OFF 時可能看到硬體游標殘影。
> 這是 Apple API 的已知問題，無法在 Sunshine 層面解決。

**11. Sunshine 啟動 / 關閉時初始化 / 銷毀 agent**

在 `src/main.cpp` 或 `src/platform/macos/misc.mm` 中：

```cpp
#include "deragabu_agent.h"

// 啟動時
int main(int argc, char* argv[]) {
    // ... Sunshine 初始化 ...

    #ifdef __APPLE__
    if (deragabu_agent_init("0.0.0.0:9000") != 0) {
        BOOST_LOG(warning) << "Failed to initialize deragabu agent";
    }
    #endif

    // ... 正常啟動流程 ...
}

// 關閉時（atexit 或 signal handler）
void cleanup() {
    #ifdef __APPLE__
    deragabu_agent_shutdown();
    #endif
}
```

---

#### Part D — 客戶端行為（已支援，無需修改）

`proto/cursor.proto` 中的 `SettingsData.draw_cursor` 已經定義，`src/webrtc_server.rs` 已會將 `SunshineSettingsEvent` 廣播給客戶端：

- `draw_cursor = true`：Agent 告訴客戶端顯示 overlay 軟光標
- `draw_cursor = false`：Agent 告訴客戶端隱藏 overlay 軟光標

> Windows 端的語義反轉在 agent 內部完成：Sunshine `display_cursor=true`（HW 光標在視頻流中）→ overlay 不需要 → `draw_cursor=false`。
> 客戶端不需要知道平台差異，只需根據 `draw_cursor` 顯示/隱藏 overlay。

---

### User Workflow

1. 在 Sunshine 配置中啟用 agent（自動設定 `capturesCursor = NO`）
2. 啟動 Sunshine → agent 自動初始化 → WebRTC server 啟動
3. 啟動串流 → `AVCaptureSession` 啟動，硬體游標 best-effort 隱藏
4. 用 Moonlight + test-client.html 連線 → 客戶端收到 `draw_cursor = true` → 顯示軟光標 overlay
5. 若用戶在串流中切換了 `display_cursor`：
   - Agent overlay **即時調整**（FFI push → 廣播 SettingsData）
   - 硬體游標不變（`capturesCursor` 不動態切換）
   - **無需重啟串流**

> **已知限制**：若 macOS 輔助功能中調整了游標大小，`capturesCursor = NO` 可能被忽略，
> 導致視頻流中出現硬體游標 + 軟光標的「雙光標」。建議用戶保持預設游標大小。

---

### Verification

1. **統一編譯測試**：使用 `build-sunshine-mac.sh` 一次完成 agent 編譯 + Sunshine patch + 構建
2. **初始狀態測試**：啟動 Sunshine + 串流 → 確認 agent overlay 軟光標正常顯示
3. **硬體游標測試**：確認 `capturesCursor = NO` 生效 → 視頻流中無硬體游標（保持預設游標大小）
4. **Accessibility bug 重現**：輔助功能中調整游標大小 → 確認硬體游標是否重新出現（記錄 bug 行為）
5. **Overlay 切換測試**：運行時切換 `display_cursor` → 確認 overlay 即時響應（不重啟串流）
6. **獨立運行測試**：不修改 Sunshine 時，`cargo build` 單獨編譯 agent 二進制仍可正常運行

### Decisions

- **staticlib 而非 cdylib**：靜態庫連結進 Sunshine 二進制，單一可執行檔，無需部署額外 `.dylib`
- **corrosion 整合**：CMake 自動調用 `cargo build`，開發者只需 `cmake --build`，零額外步驟
- **FFI + AtomicBool**：跨語言狀態同步最安全的方式，無 callback 生命週期問題
- **capturesCursor = NO (best-effort)**：在 session 建立時一次性設定，不動態切換。已知 macOS bug 可能導致此設定被忽略
- **Agent overlay 是主要光標**：不依賴 `capturesCursor` 的可靠性，overlay 始終是用戶看到的光標
- **不動態切換 capturesCursor**：`beginConfiguration/commitConfiguration` 代價對低延遲串流過大，且 bug 導致行為不可預測
- **`draw_cursor` 語義 = 「顯示 overlay 光標」**：`true` = 顯示，`false` = 隱藏。客戶端無需知道平台差異
- **Windows 反轉在 agent 內部**：Sunshine `display_cursor` 讀取後反轉再發送，macOS FFI 直接傳遞
- **保留獨立二進制**：`[[bin]]` 仍存在，不改 Sunshine 時 agent 可獨立運行（Windows / 非整合場景）
- **AVFoundation 而非 ScreenCaptureKit**：Sunshine 目前使用 `AVCaptureSession`，非 SCK
