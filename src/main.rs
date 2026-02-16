mod cursor_capture;
mod websocket_server;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info};

// Include generated Protobuf code
pub mod cursor {
    include!(concat!(env!("OUT_DIR"), "/cursor.rs"));
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    info!("Deragabu Agent starting...");

    // Create channel for passing cursor events (lightweight, per-client scaling done in WS handler)
    let (tx, rx) = mpsc::channel::<cursor_capture::CursorEvent>(32);

    // WebSocket server bind address
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9000".to_string());

    // Start WebSocket server task (sends cursor data to clients)
    let ws_handle = tokio::spawn(websocket_server::run_websocket_server(bind_addr, rx));

    // Start cursor capture task
    let capture_handle = tokio::spawn(async move {
        if let Err(e) = cursor_capture::run_cursor_capture(tx).await {
            error!("Cursor capture error: {}", e);
        }
    });

    // Wait for tasks to complete
    tokio::select! {
        result = ws_handle => {
            if let Err(e) = result {
                error!("WebSocket task error: {}", e);
            }
        }
        result = capture_handle => {
            if let Err(e) = result {
                error!("Cursor capture task error: {}", e);
            }
        }
    }

    Ok(())
}
