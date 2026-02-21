mod clipboard_sync;
mod cursor_capture;
mod sunshine_monitor;
mod webrtc_server;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info};

// Include generated Protobuf code
pub mod cursor {
    include!(concat!(env!("OUT_DIR"), "/cursor.rs"));
}

/// Unified event type broadcast to all connected WebRTC clients.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Cursor(cursor_capture::CursorEvent),
    Clipboard(clipboard_sync::ClipboardEvent),
    Settings(sunshine_monitor::SunshineSettingsEvent),
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Deragabu Agent starting...");

    // Channels feeding into the unified broadcast
    let (cursor_tx, mut cursor_rx) = mpsc::channel::<cursor_capture::CursorEvent>(32);
    let (clipboard_tx, mut clipboard_rx) = mpsc::channel::<clipboard_sync::ClipboardEvent>(32);
    let (settings_tx, mut settings_rx) =
        mpsc::channel::<sunshine_monitor::SunshineSettingsEvent>(8);
    let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(64);

    // Forward cursor events → AgentEvent
    let agent_tx_cursor = agent_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = cursor_rx.recv().await {
            if agent_tx_cursor.send(AgentEvent::Cursor(ev)).await.is_err() {
                break;
            }
        }
    });

    // Forward clipboard events → AgentEvent
    let agent_tx_clipboard = agent_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = clipboard_rx.recv().await {
            if agent_tx_clipboard.send(AgentEvent::Clipboard(ev)).await.is_err() {
                break;
            }
        }
    });

    // Forward sunshine settings events → AgentEvent
    let agent_tx_settings = agent_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = settings_rx.recv().await {
            if agent_tx_settings
                .send(AgentEvent::Settings(ev))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Server bind address
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".to_string());

    // Start WebRTC signaling + data channel server
    let rtc_handle = tokio::spawn(webrtc_server::run_webrtc_server(bind_addr, agent_rx));

    // Start cursor capture task
    let capture_handle = tokio::spawn(async move {
        if let Err(e) = cursor_capture::run_cursor_capture(cursor_tx).await {
            error!("Cursor capture error: {}", e);
        }
    });

    // Start clipboard capture task
    let clipboard_handle = tokio::spawn(async move {
        if let Err(e) = clipboard_sync::run_clipboard_capture(clipboard_tx).await {
            error!("Clipboard capture error: {}", e);
        }
    });

    // Start Sunshine monitor (detects draw_cursor state from running Sunshine process)
    let sunshine_handle = tokio::spawn(async move {
        if let Err(e) = sunshine_monitor::run_sunshine_monitor(settings_tx).await {
            error!("Sunshine monitor error: {}", e);
        }
    });

    // Wait for any task to complete (any exit is treated as fatal)
    tokio::select! {
        result = rtc_handle => {
            if let Err(e) = result {
                error!("WebRTC server task error: {}", e);
            }
        }
        result = capture_handle => {
            if let Err(e) = result {
                error!("Cursor capture task error: {}", e);
            }
        }
        result = clipboard_handle => {
            if let Err(e) = result {
                error!("Clipboard capture task error: {}", e);
            }
        }
        result = sunshine_handle => {
            if let Err(e) = result {
                error!("Sunshine monitor task error: {}", e);
            }
        }
    }

    Ok(())
}

