use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration};
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message as WsMessage};
use tracing::{debug, error, info};

use crate::cursor::CursorMessage;

/// Run WebSocket server (sends cursor data to connected clients)
pub async fn run_websocket_server(
    bind_addr: String,
    mut rx: mpsc::Receiver<CursorMessage>,
) -> Result<()> {
    // Create broadcast channel for sending messages to all clients
    let (tx_broadcast, _) = broadcast::channel::<Vec<u8>>(100);
    let tx_broadcast = Arc::new(tx_broadcast);

    // Parse bind address
    let addr: SocketAddr = bind_addr.parse()?;
    let listener = TcpListener::bind(&addr).await?;

    info!("WebSocket server listening on: {}", addr);

    // Start broadcast task: broadcast cursor messages to all clients
    let tx_broadcast_clone = tx_broadcast.clone();
    tokio::spawn(async move {
        while let Some(cursor_msg) = rx.recv().await {
            // Serialize Protobuf message
            let mut buf = Vec::new();
            if let Err(e) = cursor_msg.encode(&mut buf) {
                error!("Protobuf encoding failed: {}", e);
                continue;
            }

            debug!("Broadcasting cursor message: {} bytes", buf.len());

            // Broadcast to all subscribers
            let _ = tx_broadcast_clone.send(buf);
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

/// Handle individual client connection
async fn handle_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    mut rx_broadcast: broadcast::Receiver<Vec<u8>>,
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

    // Heartbeat interval
    let mut heartbeat_interval = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            // Receive broadcasted cursor messages
            Ok(data) = rx_broadcast.recv() => {
                // Send binary message
                if let Err(e) = write.send(WsMessage::Binary(data.into())).await {
                    error!("Send failed ({}): {}", peer_addr, e);
                    break;
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

            // Receive client messages
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
                        debug!("Received text message ({}): {}", peer_addr, text);
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

