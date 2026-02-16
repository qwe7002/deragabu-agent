use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message as WsMessage};
use tracing::{debug, error, info};

use crate::cursor::{
    cursor_message::Payload, CursorMessage, CursorSignal, MessageType,
};
use crate::cursor_capture::{
    CursorEvent, create_hide_message, create_scaled_cursor_message,
    get_last_cursor_id, get_raw_cursor,
};

/// Run WebSocket server (sends cursor data to connected clients, scaled per client DPR)
pub async fn run_websocket_server(
    bind_addr: String,
    mut rx: mpsc::Receiver<CursorEvent>,
) -> Result<()> {
    // Create broadcast channel for cursor events (lightweight, not pre-encoded)
    let (tx_broadcast, _) = broadcast::channel::<CursorEvent>(100);
    let tx_broadcast = Arc::new(tx_broadcast);

    // Parse bind address
    let addr: SocketAddr = bind_addr.parse()?;
    let listener = TcpListener::bind(&addr).await?;

    info!("WebSocket server listening on: {}", addr);

    // Broadcast task: forward cursor events to all clients
    let tx_broadcast_clone = tx_broadcast.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            debug!("Broadcasting cursor event: {:?}", event);
            let _ = tx_broadcast_clone.send(event);
        }
    });

    // Accept client connections
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("New client connected: {}", peer_addr);

                let rx_broadcast = tx_broadcast.subscribe();
                tokio::spawn(handle_client(stream, peer_addr, rx_broadcast));
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}

/// Handle individual client connection with per-client DPR scaling
async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    mut rx_broadcast: broadcast::Receiver<CursorEvent>,
) {
    info!("Handling client: {}", peer_addr);

    // Perform WebSocket handshake
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed ({}): {}", peer_addr, e);
            return;
        }
    };

    info!("WebSocket handshake successful: {}", peer_addr);

    let (mut write, mut read) = ws_stream.split();

    // Per-client state
    let mut client_dpr: f32 = 1.0; // Default DPR until client sends config
    let mut sent_cursor_ids: HashSet<String> = HashSet::new();

    // Heartbeat interval
    let mut heartbeat_interval = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // Receive cursor events from broadcast
            Ok(event) = rx_broadcast.recv() => {
                match event {
                    CursorEvent::CursorChanged(ref cursor_id) => {
                        if let Some(_raw) = get_raw_cursor(cursor_id) {
                            if sent_cursor_ids.contains(cursor_id) {
                                // Already sent to this client - send lightweight signal
                                let signal_msg = create_signal_message(cursor_id);
                                let mut buf = Vec::new();
                                if let Err(e) = signal_msg.encode(&mut buf) {
                                    error!("Protobuf encoding failed: {}", e);
                                    continue;
                                }
                                debug!("Sending cursor signal to {} (dpr={:.2}): {}", peer_addr, client_dpr, cursor_id);
                                if let Err(e) = write.send(WsMessage::Binary(buf.into())).await {
                                    error!("Send failed ({}): {}", peer_addr, e);
                                    break;
                                }
                            } else {
                                // First time - send full cursor data scaled for this client's DPR
                                if let Some(data_msg) = create_scaled_cursor_message(cursor_id, client_dpr) {
                                    let mut buf = Vec::new();
                                    if let Err(e) = data_msg.encode(&mut buf) {
                                        error!("Protobuf encoding failed: {}", e);
                                        continue;
                                    }
                                    debug!("Sending scaled cursor data to {} (dpr={:.2}): {} ({} bytes)",
                                        peer_addr, client_dpr, cursor_id, buf.len());
                                    if let Err(e) = write.send(WsMessage::Binary(buf.into())).await {
                                        error!("Send failed ({}): {}", peer_addr, e);
                                        break;
                                    }
                                    sent_cursor_ids.insert(cursor_id.clone());
                                }
                            }
                        }
                    }
                    CursorEvent::CursorHidden => {
                        let hide_msg = create_hide_message();
                        let mut buf = Vec::new();
                        if let Err(e) = hide_msg.encode(&mut buf) {
                            error!("Protobuf encoding failed: {}", e);
                            continue;
                        }
                        if let Err(e) = write.send(WsMessage::Binary(buf.into())).await {
                            error!("Send failed ({}): {}", peer_addr, e);
                            break;
                        }
                    }
                }
            }

            // Heartbeat
            _ = heartbeat_interval.tick() => {
                if let Err(e) = write.send(WsMessage::Ping(vec![])).await {
                    error!("Heartbeat send failed ({}): {}", peer_addr, e);
                    break;
                }
                debug!("Heartbeat sent: {}", peer_addr);
            }

            // Receive client messages (including DPR config)
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Close(_))) => {
                        info!("Client closed connection: {}", peer_addr);
                        break;
                    }
                    Some(Ok(WsMessage::Pong(_))) => {
                        debug!("Received Pong: {}", peer_addr);
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        let text_str = text.to_string();
                        debug!("Received text message ({}): {}", peer_addr, text_str);

                        // Parse client config: {"device_pixel_ratio": 2.0}
                        if let Some(new_dpr) = parse_dpr_from_json(&text_str) {
                            if new_dpr > 0.0 && new_dpr <= 10.0 && (new_dpr - client_dpr).abs() > 0.01 {
                                info!("Client {} set DPR: {:.2} -> {:.2}", peer_addr, client_dpr, new_dpr);
                                client_dpr = new_dpr;

                                // Clear sent cursors - need to resend at new scale
                                sent_cursor_ids.clear();

                                // Send current cursor at new scale immediately
                                if let Some(current_id) = get_last_cursor_id() {
                                    if let Some(data_msg) = create_scaled_cursor_message(&current_id, client_dpr) {
                                        let mut buf = Vec::new();
                                        if let Err(e) = data_msg.encode(&mut buf) {
                                            error!("Protobuf encoding failed: {}", e);
                                        } else {
                                            info!("Sending current cursor to {} at DPR {:.2} ({} bytes)",
                                                peer_addr, client_dpr, buf.len());
                                            let _ = write.send(WsMessage::Binary(buf.into())).await;
                                            sent_cursor_ids.insert(current_id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        debug!("Received binary message ({}): {} bytes", peer_addr, data.len());
                    }
                    Some(Err(e)) => {
                        error!("WebSocket receive error ({}): {}", peer_addr, e);
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended: {}", peer_addr);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    info!("Client disconnected: {}", peer_addr);
}

/// Parse device_pixel_ratio from a simple JSON string.
/// Avoids adding serde_json dependency - handles {"device_pixel_ratio": 1.5}
fn parse_dpr_from_json(json: &str) -> Option<f32> {
    let key = "device_pixel_ratio";
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let colon_pos = rest.find(':')?;
    let after_colon = rest[colon_pos + 1..].trim();

    // Extract the number (stop at comma, brace, or end)
    let num_end = after_colon.find(|c: char| c == ',' || c == '}' || c == '\n')
        .unwrap_or(after_colon.len());
    let num_str = after_colon[..num_end].trim();
    num_str.parse::<f32>().ok()
}

/// Create a cursor signal message (tells client to switch to a cached cursor)
fn create_signal_message(cursor_id: &str) -> CursorMessage {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    CursorMessage {
        r#type: MessageType::CursorSignal.into(),
        payload: Some(Payload::CursorSignal(CursorSignal {
            cursor_id: cursor_id.to_string(),
        })),
        timestamp,
    }
}

