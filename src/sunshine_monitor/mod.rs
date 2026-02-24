use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

#[cfg(target_os = "windows")]
mod windows;

// ─── FFI push state ─────────────────────────────────────────────────────────
//
// When the agent is linked into Sunshine as a static library, Sunshine calls
// `set_display_cursor_from_ffi()` to push `display_cursor` state changes.
// The non-Windows monitor task polls this `AtomicBool` instead of reading
// process memory or config files.

/// Current display_cursor value (set via FFI or defaults to `true`).
static DISPLAY_CURSOR_FFI: AtomicBool = AtomicBool::new(true);

/// Set `display_cursor` from the C FFI.  Thread-safe, lock-free.
pub fn set_display_cursor_from_ffi(val: bool) {
    DISPLAY_CURSOR_FFI.store(val, Ordering::SeqCst);
}

/// Read the current FFI display_cursor value.
pub fn get_display_cursor_ffi() -> bool {
    DISPLAY_CURSOR_FFI.load(Ordering::SeqCst)
}

/// Event emitted when cursor overlay visibility should change.
#[derive(Clone, Debug)]
pub struct SunshineSettingsEvent {
    /// Whether the agent's overlay cursor should be shown to the user.
    /// `true` = show overlay cursor, `false` = hide overlay cursor.
    pub draw_cursor: bool,
}

/// Run the Sunshine monitor, watching for draw_cursor state changes.
///
/// On Windows this reads the Sunshine config file and also attempts to read
/// the running Sunshine process memory (via PDB debug symbols) for the live
/// runtime value of `draw_cursor`.
#[cfg(target_os = "windows")]
pub async fn run_sunshine_monitor(tx: mpsc::Sender<SunshineSettingsEvent>) -> Result<()> {
    windows::run_monitor(tx).await
}

/// Non-Windows monitor: polls the FFI `AtomicBool` for `display_cursor` changes.
///
/// When linked into Sunshine, the host calls `deragabu_agent_set_display_cursor()`
/// which updates the `AtomicBool`.  This task detects the change and broadcasts
/// a `SunshineSettingsEvent` to all connected WebRTC clients.
///
/// When running standalone (no FFI calls), the value stays at the default `true`
/// (= Sunshine draws cursor in video), which is the safe default.
#[cfg(not(target_os = "windows"))]
pub async fn run_sunshine_monitor(tx: mpsc::Sender<SunshineSettingsEvent>) -> Result<()> {
    // Send initial state
    let mut last_value = DISPLAY_CURSOR_FFI.load(Ordering::SeqCst);
    let _ = tx
        .send(SunshineSettingsEvent {
            draw_cursor: last_value,
        })
        .await;
    tracing::info!(
        "Sunshine monitor started (FFI poll mode, initial draw_cursor={})",
        last_value
    );

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let current = DISPLAY_CURSOR_FFI.load(Ordering::SeqCst);
        if current != last_value {
            tracing::info!("display_cursor changed to {} (via FFI)", current);
            let _ = tx
                .send(SunshineSettingsEvent {
                    draw_cursor: current,
                })
                .await;
            last_value = current;
        }
    }
}
