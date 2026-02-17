use anyhow::Result;
use axum::{
    extract::State,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::{interval, Duration};
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info};

use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::cursor::{
    cursor_message::Payload, CursorMessage, CursorSignal, MessageType,
};
use crate::cursor_capture::{
    CursorEvent, create_hide_message, create_scaled_cursor_message,
    get_cached_cursor, get_last_cursor_id,
};

#[derive(Deserialize)]
struct OfferRequest {
    sdp: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    sdp_type: String,
}

#[derive(Serialize)]
struct AnswerResponse {
    sdp: String,
    #[serde(rename = "type")]
    sdp_type: String,
}

struct AppState {
    tx_broadcast: Arc<broadcast::Sender<CursorEvent>>,
    api: webrtc::api::API,
    /// Keep peer connections alive
    _peer_connections: Mutex<Vec<Arc<RTCPeerConnection>>>,
}

/// Per-client state shared between sender task and message handler
struct ClientState {
    dpr: f32,
    sent_cursor_ids: HashSet<String>,
}

/// Run WebRTC signaling + data channel server
pub async fn run_webrtc_server(
    bind_addr: String,
    mut rx: mpsc::Receiver<CursorEvent>,
) -> Result<()> {
    let (tx_broadcast, _) = broadcast::channel::<CursorEvent>(100);
    let tx_broadcast = Arc::new(tx_broadcast);

    // Create WebRTC API (data-channel-only, no media codecs needed)
    let api = APIBuilder::new().build();

    let addr: SocketAddr = bind_addr.parse()?;

    let state = Arc::new(AppState {
        tx_broadcast: tx_broadcast.clone(),
        api,
        _peer_connections: Mutex::new(Vec::new()),
    });

    // Broadcast task: forward cursor events from capture to all clients
    let tx_broadcast_clone = tx_broadcast.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            debug!("Broadcasting cursor event: {:?}", event);
            let _ = tx_broadcast_clone.send(event);
        }
    });

    // HTTP signaling server with CORS
    let app = Router::new()
        .route("/", get(serve_test_page))
        .route("/offer", post(handle_offer))
        .layer(CorsLayer::permissive())
        .with_state(state);

    info!("WebRTC signaling server listening on: {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Serve built-in test client page
async fn serve_test_page() -> Html<&'static str> {
    Html(include_str!("../test-client.html"))
}

/// Handle SDP offer from client, create peer connection, return SDP answer
async fn handle_offer(
    State(state): State<Arc<AppState>>,
    Json(offer_req): Json<OfferRequest>,
) -> Result<Json<AnswerResponse>, (axum::http::StatusCode, String)> {
    info!("Received WebRTC offer from client");

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = Arc::new(
        state
            .api
            .new_peer_connection(config)
            .await
            .map_err(|e| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create peer connection: {}", e),
                )
            })?,
    );

    // Store to keep alive
    state
        ._peer_connections
        .lock()
        .await
        .push(peer_connection.clone());

    // Log connection state changes
    peer_connection.on_peer_connection_state_change(Box::new(
        move |s: RTCPeerConnectionState| {
            info!("Peer connection state: {:?}", s);
            Box::pin(async {})
        },
    ));

    // Handle incoming data channels from client
    let tx_broadcast = state.tx_broadcast.clone();
    peer_connection.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        info!("Client opened data channel: {}", dc.label());
        let rx_broadcast = tx_broadcast.subscribe();

        Box::pin(async move {
            let client_state = Arc::new(Mutex::new(ClientState {
                dpr: 1.0,
                sent_cursor_ids: HashSet::new(),
            }));

            // On open: start sending cursor events
            let dc_sender = dc.clone();
            let cs_sender = client_state.clone();
            dc.on_open(Box::new(move || {
                info!("DataChannel open, starting cursor sender");
                let dc = dc_sender;
                let client_state = cs_sender;
                let mut rx = rx_broadcast;

                Box::pin(async move {
                    tokio::spawn(async move {
                        let mut heartbeat = interval(Duration::from_secs(30));
                        heartbeat.tick().await; // skip first tick

                        loop {
                            tokio::select! {
                                result = rx.recv() => {
                                    match result {
                                        Ok(event) => {
                                            let mut cs = client_state.lock().await;
                                            if send_cursor_event(&dc, &mut cs, &event).await.is_err() {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Lagged(n)) => {
                                            debug!("Broadcast lagged, missed {} events", n);
                                        }
                                        Err(broadcast::error::RecvError::Closed) => {
                                            info!("Broadcast closed");
                                            break;
                                        }
                                    }
                                }
                                _ = heartbeat.tick() => {
                                    let msg = create_heartbeat_message();
                                    let mut buf = Vec::new();
                                    if msg.encode(&mut buf).is_err() {
                                        continue;
                                    }
                                    if dc.send(&Bytes::from(buf)).await.is_err() {
                                        break;
                                    }
                                    debug!("Heartbeat sent");
                                }
                            }
                        }
                        info!("Cursor sender stopped");
                    });
                })
            }));

            // On message: handle DPR config from client
            let cs_msg = client_state.clone();
            let dc_msg = dc.clone();
            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let cs = cs_msg.clone();
                let dc = dc_msg.clone();

                Box::pin(async move {
                    if msg.is_string {
                        let text = String::from_utf8_lossy(&msg.data);
                        debug!("Client text message: {}", text);

                        if let Some(new_dpr) = parse_dpr_from_json(&text) {
                            let mut state = cs.lock().await;
                            if new_dpr > 0.0
                                && new_dpr <= 10.0
                                && (new_dpr - state.dpr).abs() > 0.01
                            {
                                info!("Client DPR: {:.2} -> {:.2}", state.dpr, new_dpr);
                                state.dpr = new_dpr;
                                state.sent_cursor_ids.clear();

                                if let Some(id) = get_last_cursor_id() {
                                    if let Some(data_msg) =
                                        create_scaled_cursor_message(&id, state.dpr)
                                    {
                                        let mut buf = Vec::new();
                                        if data_msg.encode(&mut buf).is_ok() {
                                            let _ = dc.send(&Bytes::from(buf)).await;
                                            state.sent_cursor_ids.insert(id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
            }));

            dc.on_close(Box::new(move || {
                info!("DataChannel closed");
                Box::pin(async {})
            }));
        })
    }));

    // Set remote description (client's offer)
    let offer = RTCSessionDescription::offer(offer_req.sdp).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Invalid SDP offer: {}", e),
        )
    })?;

    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("set_remote_description failed: {}", e),
            )
        })?;

    // Create and set local answer
    let answer = peer_connection.create_answer(None).await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("create_answer failed: {}", e),
        )
    })?;

    peer_connection
        .set_local_description(answer)
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("set_local_description failed: {}", e),
            )
        })?;

    // Wait for ICE gathering with timeout
    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    tokio::select! {
        _ = gather_complete.recv() => { debug!("ICE gathering complete"); }
        _ = tokio::time::sleep(Duration::from_secs(5)) => { debug!("ICE gathering timeout"); }
    }

    let local_desc = peer_connection.local_description().await.ok_or((
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "No local description available".to_string(),
    ))?;

    info!("Sending SDP answer to client");

    Ok(Json(AnswerResponse {
        sdp: local_desc.sdp,
        sdp_type: "answer".to_string(),
    }))
}

/// Send cursor event as protobuf binary over data channel
async fn send_cursor_event(
    dc: &Arc<RTCDataChannel>,
    state: &mut ClientState,
    event: &CursorEvent,
) -> Result<(), ()> {
    match event {
        CursorEvent::CursorChanged(cursor_id) => {
            if get_cached_cursor(cursor_id).is_some() {
                if state.sent_cursor_ids.contains(cursor_id) {
                    let msg = create_signal_message(cursor_id);
                    let mut buf = Vec::new();
                    if let Err(e) = msg.encode(&mut buf) {
                        error!("Encode error: {}", e);
                        return Ok(());
                    }
                    if let Err(e) = dc.send(&Bytes::from(buf)).await {
                        error!("DC send error: {}", e);
                        return Err(());
                    }
                } else if let Some(data_msg) =
                    create_scaled_cursor_message(cursor_id, state.dpr)
                {
                    let mut buf = Vec::new();
                    if let Err(e) = data_msg.encode(&mut buf) {
                        error!("Encode error: {}", e);
                        return Ok(());
                    }
                    debug!(
                        "Sending cursor data (dpr={:.2}): {} ({} bytes)",
                        state.dpr,
                        cursor_id,
                        buf.len()
                    );
                    if let Err(e) = dc.send(&Bytes::from(buf)).await {
                        error!("DC send error: {}", e);
                        return Err(());
                    }
                    state.sent_cursor_ids.insert(cursor_id.clone());
                }
            }
        }
        CursorEvent::CursorHidden => {
            let msg = create_hide_message();
            let mut buf = Vec::new();
            if let Err(e) = msg.encode(&mut buf) {
                error!("Encode error: {}", e);
                return Ok(());
            }
            if let Err(e) = dc.send(&Bytes::from(buf)).await {
                error!("DC send error: {}", e);
                return Err(());
            }
        }
    }
    Ok(())
}

/// Parse device_pixel_ratio from a simple JSON string
fn parse_dpr_from_json(json: &str) -> Option<f32> {
    let key = "device_pixel_ratio";
    let pos = json.find(key)?;
    let rest = &json[pos + key.len()..];
    let colon_pos = rest.find(':')?;
    let after_colon = rest[colon_pos + 1..].trim();
    let num_end = after_colon
        .find(|c: char| c == ',' || c == '}' || c == '\n')
        .unwrap_or(after_colon.len());
    after_colon[..num_end].trim().parse::<f32>().ok()
}

fn create_heartbeat_message() -> CursorMessage {
    CursorMessage {
        r#type: MessageType::Heartbeat.into(),
        payload: None,
        timestamp: now_ms(),
    }
}

fn create_signal_message(cursor_id: &str) -> CursorMessage {
    CursorMessage {
        r#type: MessageType::CursorSignal.into(),
        payload: Some(Payload::CursorSignal(CursorSignal {
            cursor_id: cursor_id.to_string(),
        })),
        timestamp: now_ms(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
