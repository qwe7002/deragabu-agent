use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_os = "windows")]
mod windows;

/// Event emitted when Sunshine settings change
#[derive(Clone, Debug)]
pub struct SunshineSettingsEvent {
    /// Whether Sunshine's draw_cursor is enabled (cursor rendered in video stream)
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

/// Non-Windows stub: assume draw_cursor is enabled (Sunshine default).
#[cfg(not(target_os = "windows"))]
pub async fn run_sunshine_monitor(tx: mpsc::Sender<SunshineSettingsEvent>) -> Result<()> {
    let _ = tx
        .send(SunshineSettingsEvent { draw_cursor: true })
        .await;
    // Idle forever – nothing to monitor on non-Windows
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
    }
}
