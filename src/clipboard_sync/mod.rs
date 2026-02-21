use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

// ── Public types ─────────────────────────────────────────────────────────────

/// Clipboard content variants.  Images are transmitted as raw PNG bytes
/// (no additional compression) so the receiver always sees the original format.
#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text(String),
    Image {
        /// Raw PNG-encoded bytes (lossless, original format)
        png_data: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// File list stub — only metadata is sent; actual file bytes are not
    /// transferred until chunked-file-transfer is implemented.
    Files(Vec<String>),
}

/// An event carrying new clipboard content and its blake3 content hash.
#[derive(Debug, Clone)]
pub struct ClipboardEvent {
    pub content: ClipboardContent,
    /// blake3 hex digest of the payload bytes, used for deduplication.
    pub content_hash: String,
}

// ── Last-set-by-us hash (prevents echo back to all clients) ──────────────────

use std::sync::Mutex as StdMutex;

static LAST_SET_HASH: StdMutex<Option<String>> = StdMutex::new(None);

/// Record the hash of content we just pushed into the host clipboard so that
/// the next poll doesn't re-broadcast it to clients.
pub fn record_set_hash(hash: &str) {
    if let Ok(mut guard) = LAST_SET_HASH.lock() {
        *guard = Some(hash.to_string());
    }
}

fn get_last_set_hash() -> Option<String> {
    LAST_SET_HASH.lock().ok()?.clone()
}

// ── Capture task ─────────────────────────────────────────────────────────────

/// Poll the host clipboard every 500 ms and send a [`ClipboardEvent`] whenever
/// the content changes.  Runs until the receiver end of `tx` is dropped.
pub async fn run_clipboard_capture(tx: mpsc::Sender<ClipboardEvent>) -> Result<()> {
    info!("Clipboard capture started (polling every 500 ms)");

    let mut poll = interval(Duration::from_millis(500));
    let mut last_broadcast_hash: Option<String> = None;

    loop {
        poll.tick().await;

        // arboard must be called on a non-async thread (especially on macOS).
        let result = tokio::task::spawn_blocking(read_clipboard).await;

        let event = match result {
            Ok(Ok(Some(ev))) => ev,
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => {
                debug!("Clipboard read error: {}", e);
                continue;
            }
            Err(e) => {
                debug!("spawn_blocking error: {}", e);
                continue;
            }
        };

        let hash = event.content_hash.clone();

        // Skip if content has not changed since our last broadcast.
        if last_broadcast_hash.as_deref() == Some(&hash) {
            continue;
        }

        // Skip if this is the echo of content we just wrote from a client.
        if get_last_set_hash().as_deref() == Some(&hash) {
            debug!("Skipping clipboard echo (hash matches last-set)");
            last_broadcast_hash = Some(hash);
            continue;
        }

        debug!("Clipboard changed — broadcasting (hash prefix: {}…)", &hash[..8]);
        last_broadcast_hash = Some(hash);

        if tx.send(event).await.is_err() {
            info!("Clipboard capture: receiver dropped, stopping");
            break;
        }
    }

    Ok(())
}

// ── Low-level clipboard read (sync, meant for spawn_blocking) ────────────────

fn read_clipboard() -> Result<Option<ClipboardEvent>> {
    let mut clipboard = arboard::Clipboard::new()?;

    // Try text first (cheapest).
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            return Ok(Some(ClipboardEvent {
                content: ClipboardContent::Text(text),
                content_hash: hash,
            }));
        }
    }

    // Try image.
    if let Ok(img) = clipboard.get_image() {
        let png = encode_rgba_to_png(img.bytes.as_ref(), img.width as u32, img.height as u32)?;
        let hash = blake3::hash(&png).to_hex().to_string();
        return Ok(Some(ClipboardEvent {
            content: ClipboardContent::Image {
                png_data: png,
                width: img.width as u32,
                height: img.height as u32,
            },
            content_hash: hash,
        }));
    }

    // File-list detection: arboard doesn't expose a cross-platform file-list
    // API yet.  On Windows, CF_HDROP is not surfaced by arboard.  We log and
    // return None; when chunked file transfer is implemented this is the hook.
    debug!("Clipboard contains no readable text or image (may be files or empty)");
    Ok(None)
}

/// Encode a flat RGBA byte slice to PNG in memory (raw, no extra compression).
fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use image::{ImageBuffer, Rgba};

    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| anyhow::anyhow!("Invalid RGBA dimensions ({width}x{height})"))?;

    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(buf.into_inner())
}

// ── Write to host clipboard (called when a client pushes clipboard to us) ────

/// Apply clipboard content received from a client to the host clipboard.
/// Records the hash so the capture loop skips the resulting echo.
pub fn apply_to_clipboard(content: &ClipboardContent, hash: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;

    match content {
        ClipboardContent::Text(text) => {
            clipboard.set_text(text.clone())?;
            info!("Applied clipboard text from client ({} chars)", text.len());
        }
        ClipboardContent::Image { png_data, width: _, height: _ } => {
            // Decode PNG → RGBA for arboard.  Dimensions come from the PNG header.
            let img = image::load_from_memory(png_data)?;
            let (w, h) = (img.width(), img.height());
            let rgba = img.to_rgba8();
            let img_data = arboard::ImageData {
                bytes: rgba.into_raw().into(),
                width: w as usize,
                height: h as usize,
            };
            clipboard.set_image(img_data)?;
            info!("Applied clipboard image from client ({}x{})", w, h);
        }
        ClipboardContent::Files(names) => {
            // Stub: file transfer not yet implemented.
            warn!(
                "File transfer not yet implemented — received file list from client: {:?}",
                names
            );
            let fallback = format!("[Files — transfer not implemented]\n{}", names.join("\n"));
            clipboard.set_text(fallback)?;
        }
    }

    record_set_hash(hash);
    Ok(())
}
