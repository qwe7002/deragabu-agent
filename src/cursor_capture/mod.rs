use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

use crate::cursor::{
    cursor_message::Payload, CursorData, CursorMessage, MessageType,
};

// Platform-specific modules
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use self::windows::{run_cursor_capture, get_dpi_scale};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::{run_cursor_capture, get_dpi_scale};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::{run_cursor_capture, get_dpi_scale};

/// Cursor event for broadcasting to clients
#[derive(Clone, Debug)]
pub enum CursorEvent {
    /// Cursor changed - carries cursor_id
    CursorChanged(String),
    /// Cursor hidden
    CursorHidden,
}

/// Cached cursor data with pre-encoded WebP (static or animated)
#[derive(Clone)]
pub struct CachedCursor {
    pub id: String,
    /// Pre-encoded WebP data (static or animated WebP)
    pub webp_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub is_animated: bool,
    pub frame_count: u32,
    pub frame_delay_ms: u32,
}

/// Global cursor cache: cursor_id -> cached cursor (with pre-encoded WebP)
pub(crate) static CURSOR_CACHE: Mutex<Option<HashMap<String, CachedCursor>>> = Mutex::new(None);

/// Last cursor_id for detecting changes
pub(crate) static LAST_CURSOR_ID: Mutex<Option<String>> = Mutex::new(None);

/// Get the last cursor_id
pub fn get_last_cursor_id() -> Option<String> {
    LAST_CURSOR_ID.lock().unwrap().clone()
}

/// Get cached cursor by id
pub fn get_cached_cursor(cursor_id: &str) -> Option<CachedCursor> {
    let cache_guard = CURSOR_CACHE.lock().unwrap();
    cache_guard.as_ref()?.get(cursor_id).cloned()
}

/// Create a CursorMessage with the cursor image for a specific client DPR.
/// The WebP data is pre-encoded (static or animated); client uses CSS scaling.
pub fn create_scaled_cursor_message(cursor_id: &str, client_dpr: f32) -> Option<CursorMessage> {
    let cached = get_cached_cursor(cursor_id)?;
    let server_scale = get_dpi_scale();
    let scale_factor = client_dpr / server_scale;

    // Compute target dimensions for client-side CSS scaling
    let (target_w, target_h, target_hx, target_hy) = if (scale_factor - 1.0).abs() < 0.01 {
        (cached.width, cached.height, cached.hotspot_x, cached.hotspot_y)
    } else {
        let target_w = (cached.width as f32 * scale_factor).round().max(1.0) as u32;
        let target_h = (cached.height as f32 * scale_factor).round().max(1.0) as u32;
        let target_hx = (cached.hotspot_x as f32 * scale_factor).round() as i32;
        let target_hy = (cached.hotspot_y as f32 * scale_factor).round() as i32;
        (target_w, target_h, target_hx, target_hy)
    };

    debug!(
        "Cursor message: id={}, {}x{} (client_dpr={:.2}, server_scale={:.2}), webp={} bytes, animated={}, frames={}",
        cached.id, target_w, target_h, client_dpr, server_scale,
        cached.webp_data.len(), cached.is_animated, cached.frame_count
    );

    Some(CursorMessage {
        r#type: MessageType::CursorData.into(),
        payload: Some(Payload::CursorData(CursorData {
            cursor_id: cached.id.clone(),
            image_data: cached.webp_data.clone(),
            width: target_w as i32,
            height: target_h as i32,
            hotspot_x: target_hx,
            hotspot_y: target_hy,
            dpi_scale: client_dpr,
            is_animated: cached.is_animated,
            frame_delay_ms: cached.frame_delay_ms,
        })),
        timestamp: get_timestamp(),
    })
}

/// Create cursor hide message
pub fn create_hide_message() -> CursorMessage {
    CursorMessage {
        r#type: MessageType::CursorHide.into(),
        payload: None,
        timestamp: get_timestamp(),
    }
}

/// Initialize the cursor cache
pub(crate) fn init_cache() {
    let mut cache = CURSOR_CACHE.lock().unwrap();
    if cache.is_none() {
        *cache = Some(HashMap::new());
        tracing::info!("Cursor cache initialized");
    }
}

/// Store a cursor in cache and return cursor_id. Returns (cursor_id, is_new).
pub(crate) fn cache_cursor(cached: CachedCursor) -> (String, bool) {
    let cursor_id = cached.id.clone();

    // Update last cursor ID
    *LAST_CURSOR_ID.lock().unwrap() = Some(cursor_id.clone());

    // Check if already in cache
    {
        let cache_guard = CURSOR_CACHE.lock().unwrap();
        let cache = cache_guard.as_ref().unwrap();
        if cache.contains_key(&cursor_id) {
            debug!("Cursor already cached: {}", cursor_id);
            return (cursor_id, false);
        }
    }

    // Cache the new cursor
    tracing::info!(
        "New cursor: id={}, {}x{}, animated={}, frames={}, webp={} bytes",
        cursor_id, cached.width, cached.height,
        cached.is_animated, cached.frame_count, cached.webp_data.len()
    );

    {
        let mut cache_guard = CURSOR_CACHE.lock().unwrap();
        let cache = cache_guard.as_mut().unwrap();
        cache.insert(cursor_id.clone(), cached);

        // Trim cache if too large
        if cache.len() > 50 {
            let keys: Vec<_> = cache.keys().cloned().collect();
            for key in keys.iter().take(25) {
                cache.remove(key);
            }
            debug!("Cache trimmed to {} entries", cache.len());
        }
    }

    (cursor_id, true)
}

/// Encode RGBA pixels as a static (single-frame) lossless WebP
pub(crate) fn encode_static_webp(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let encoder = webp::Encoder::from_rgba(rgba, width, height);
    let memory = encoder.encode_lossless();
    Ok(memory.to_vec())
}

/// Encode multiple RGBA frames as an animated WebP
pub(crate) fn encode_animated_webp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    frame_delay_ms: i32,
) -> Result<Vec<u8>> {
    use anyhow::anyhow;

    let mut encoder = webp_animation::Encoder::new((width, height))
        .map_err(|e| anyhow!("Failed to create animated WebP encoder: {:?}", e))?;

    for (i, frame_rgba) in frames.iter().enumerate() {
        let timestamp_ms = (i as i32) * frame_delay_ms;
        encoder.add_frame(frame_rgba, timestamp_ms)
            .map_err(|e| anyhow!("Failed to add frame {}: {:?}", i, e))?;
    }

    let final_timestamp = frames.len() as i32 * frame_delay_ms;
    let webp_data = encoder.finalize(final_timestamp)
        .map_err(|e| anyhow!("Failed to finalize animated WebP: {:?}", e))?;

    Ok(webp_data.to_vec())
}

/// Expand the canvas by `pad` pixels on each side, copying original pixels to the center.
/// Returns the new RGBA buffer with updated dimensions.
pub(crate) fn expand_canvas(rgba: &[u8], width: u32, height: u32, pad: u32) -> (Vec<u8>, u32, u32) {
    let old_w = width as usize;
    let old_h = height as usize;
    let new_w = old_w + (pad as usize) * 2;
    let new_h = old_h + (pad as usize) * 2;
    let mut new_rgba = vec![0u8; new_w * new_h * 4];

    for y in 0..old_h {
        for x in 0..old_w {
            let src = (y * old_w + x) * 4;
            let dst = ((y + pad as usize) * new_w + (x + pad as usize)) * 4;
            new_rgba[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }

    (new_rgba, new_w as u32, new_h as u32)
}

/// Add a white outline of `radius` pixels around opaque pixels for better visibility.
/// This helps XOR cursors (rendered as dark pixels) be visible on dark backgrounds.
pub(crate) fn add_white_outline(rgba: &mut [u8], width: u32, height: u32, radius: i32) {
    let w = width as usize;
    let h = height as usize;
    let r2 = (radius * radius) as f32;

    // First pass: identify transparent pixels within `radius` of any opaque pixel
    let mut outline_pixels = Vec::new();

    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            // Skip pixels that already have content
            if rgba[idx + 3] > 0 {
                continue;
            }

            // Check if any opaque pixel is within the radius
            let mut min_dist2 = f32::MAX;
            let y_start = (y as i32 - radius).max(0) as usize;
            let y_end = (y as i32 + radius).min(h as i32 - 1) as usize;
            let x_start = (x as i32 - radius).max(0) as usize;
            let x_end = (x as i32 + radius).min(w as i32 - 1) as usize;

            'outer: for ny in y_start..=y_end {
                for nx in x_start..=x_end {
                    let n_idx = (ny * w + nx) * 4;
                    if rgba[n_idx + 3] > 200 {
                        let dx = x as f32 - nx as f32;
                        let dy = y as f32 - ny as f32;
                        let d2 = dx * dx + dy * dy;
                        if d2 < min_dist2 {
                            min_dist2 = d2;
                        }
                        if d2 <= 1.0 {
                            break 'outer;
                        }
                    }
                }
            }

            if min_dist2 <= r2 {
                // Alpha fades from 255 at distance 0 to 0 at the edge
                let dist = min_dist2.sqrt();
                let alpha = ((1.0 - dist / radius as f32) * 255.0).clamp(0.0, 255.0) as u8;
                if alpha > 0 {
                    outline_pixels.push((idx, alpha));
                }
            }
        }
    }

    // Second pass: set outline pixels to white with distance-based alpha
    for (idx, alpha) in outline_pixels {
        rgba[idx] = 255;     // R
        rgba[idx + 1] = 255; // G
        rgba[idx + 2] = 255; // B
        rgba[idx + 3] = alpha;
    }
}

/// Get current timestamp (milliseconds)
pub(crate) fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
