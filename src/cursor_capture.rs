use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetObjectW, PatBlt, ReleaseDC, SelectObject,
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACKNESS, DIB_RGB_COLORS, WHITENESS,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING,
    DI_NORMAL, HCURSOR, ICONINFO,
};

use crate::cursor::{
    cursor_message::Payload, CursorData, CursorMessage, MessageType,
};

/// Cursor event for broadcasting to clients
#[derive(Clone, Debug)]
pub enum CursorEvent {
    /// Cursor changed - carries cursor_id (pixel hash based)
    CursorChanged(String),
    /// Cursor hidden
    CursorHidden,
}

/// Raw cached cursor data (stores RGBA pixels for on-demand scaling per client)
#[derive(Clone)]
pub struct RawCachedCursor {
    pub id: String,
    pub rgba_pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub is_animated: bool,
    pub frame_delay_ms: u32,
}

/// Global raw cursor cache: cursor_id -> raw cursor data
static RAW_CURSOR_CACHE: Mutex<Option<HashMap<String, RawCachedCursor>>> = Mutex::new(None);

/// Last Windows cursor handle (HCURSOR value)
static LAST_CURSOR_HANDLE: Mutex<isize> = Mutex::new(0);

/// Last cursor_id (pixel hash) for detecting animation frame changes
static LAST_CURSOR_ID: Mutex<Option<String>> = Mutex::new(None);

/// Known animated cursor handles (frames change with same handle)
static ANIMATED_HANDLES: Mutex<Option<HashSet<isize>>> = Mutex::new(None);

/// Known static cursor handles (confirmed non-animated)
static STATIC_HANDLES: Mutex<Option<HashSet<isize>>> = Mutex::new(None);

/// Counter for animation probe attempts on current cursor handle
static ANIM_PROBE_COUNT: Mutex<u32> = Mutex::new(0);

/// Timestamp of last animation frame change (for measuring frame delay)
static LAST_FRAME_CHANGE_MS: Mutex<u64> = Mutex::new(0);

/// Estimated average frame delay for current animated cursor (ms)
static AVG_FRAME_DELAY: Mutex<u32> = Mutex::new(60);

/// Timestamp when current cursor handle was first seen (for animation step calculation)
static HANDLE_FIRST_SEEN_MS: Mutex<u64> = Mutex::new(0);

/// Run cursor capture loop (sends CursorEvent for per-client scaling)
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorEvent>) -> Result<()> {
    // Enable DPI awareness
    unsafe {
        let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
    }

    // Initialize cache and animation tracking
    {
        let mut cache = RAW_CURSOR_CACHE.lock().unwrap();
        if cache.is_none() {
            *cache = Some(HashMap::new());
            info!("Cursor cache initialized");
        }
    }
    {
        let mut animated = ANIMATED_HANDLES.lock().unwrap();
        if animated.is_none() {
            *animated = Some(HashSet::new());
        }
    }
    {
        let mut statics = STATIC_HANDLES.lock().unwrap();
        if statics.is_none() {
            *statics = Some(HashSet::new());
        }
    }

    let dpi_scale = get_dpi_scale();
    info!("Starting cursor capture (DPI scale: {:.2})", dpi_scale);

    let mut poll_interval = interval(Duration::from_millis(16)); // ~60fps

    loop {
        poll_interval.tick().await;

        match capture_cursor() {
            Ok(Some(event)) => {
                if tx.send(event).await.is_err() {
                    warn!("Receiver closed, stopping cursor capture");
                    break;
                }
            }
            Ok(None) => {
                // No change
            }
            Err(e) => {
                warn!("Failed to capture cursor: {}", e);
            }
        }
    }

    Ok(())
}

/// Get system DPI scale factor
pub fn get_dpi_scale() -> f32 {
    unsafe {
        let dpi = GetDpiForSystem();
        dpi as f32 / 96.0
    }
}

/// Get the last cursor_id (so new clients can get the current cursor)
pub fn get_last_cursor_id() -> Option<String> {
    LAST_CURSOR_ID.lock().unwrap().clone()
}

/// Get raw cursor data from the cache by cursor_id
pub fn get_raw_cursor(cursor_id: &str) -> Option<RawCachedCursor> {
    let cache_guard = RAW_CURSOR_CACHE.lock().unwrap();
    cache_guard.as_ref()?.get(cursor_id).cloned()
}

/// Create a CursorMessage with the cursor image scaled for a specific client DPR.
///
/// The server captures cursors at the system's native DPI (e.g., 1.5x).
/// If the client has devicePixelRatio = 2.0, the cursor is scaled by
/// `client_dpr / server_dpi_scale` to produce a pixel-perfect image.
pub fn create_scaled_cursor_message(cursor_id: &str, client_dpr: f32) -> Option<CursorMessage> {
    let raw = get_raw_cursor(cursor_id)?;
    let server_scale = get_dpi_scale();

    let scale_factor = client_dpr / server_scale;

    let (scaled_rgba, target_w, target_h, target_hx, target_hy) = if (scale_factor - 1.0).abs() < 0.01 {
        // No scaling needed
        (raw.rgba_pixels.clone(), raw.width, raw.height, raw.hotspot_x, raw.hotspot_y)
    } else {
        let target_w = (raw.width as f32 * scale_factor).round().max(1.0) as u32;
        let target_h = (raw.height as f32 * scale_factor).round().max(1.0) as u32;
        let target_hx = (raw.hotspot_x as f32 * scale_factor).round() as i32;
        let target_hy = (raw.hotspot_y as f32 * scale_factor).round() as i32;
        let scaled = scale_rgba(&raw.rgba_pixels, raw.width, raw.height, target_w, target_h);
        (scaled, target_w, target_h, target_hx, target_hy)
    };

    let webp_data = encode_webp(&scaled_rgba, target_w, target_h).ok()?;

    debug!(
        "Scaled cursor: id={}, {}x{} -> {}x{} (client_dpr={:.2}, server_scale={:.2}, factor={:.2}), webp={} bytes",
        raw.id, raw.width, raw.height, target_w, target_h, client_dpr, server_scale, scale_factor, webp_data.len()
    );

    Some(CursorMessage {
        r#type: MessageType::CursorData.into(),
        payload: Some(Payload::CursorData(CursorData {
            cursor_id: raw.id.clone(),
            image_data: webp_data,
            width: target_w as i32,
            height: target_h as i32,
            hotspot_x: target_hx,
            hotspot_y: target_hy,
            dpi_scale: client_dpr,
            is_animated: raw.is_animated,
            frame_delay_ms: raw.frame_delay_ms,
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

/// Scale RGBA image using bilinear interpolation
fn scale_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }

    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    let x_ratio = src_w as f32 / dst_w as f32;
    let y_ratio = src_h as f32 / dst_h as f32;

    for y in 0..dst_h {
        for x in 0..dst_w {
            let src_x = x as f32 * x_ratio;
            let src_y = y as f32 * y_ratio;

            let x0 = src_x.floor() as u32;
            let y0 = src_y.floor() as u32;
            let x1 = (x0 + 1).min(src_w - 1);
            let y1 = (y0 + 1).min(src_h - 1);

            let fx = src_x - x0 as f32;
            let fy = src_y - y0 as f32;

            let dst_idx = (y * dst_w + x) as usize * 4;

            for c in 0..4 {
                let p00 = src[(y0 * src_w + x0) as usize * 4 + c] as f32;
                let p10 = src[(y0 * src_w + x1) as usize * 4 + c] as f32;
                let p01 = src[(y1 * src_w + x0) as usize * 4 + c] as f32;
                let p11 = src[(y1 * src_w + x1) as usize * 4 + c] as f32;

                let value = p00 * (1.0 - fx) * (1.0 - fy)
                    + p10 * fx * (1.0 - fy)
                    + p01 * (1.0 - fx) * fy
                    + p11 * fx * fy;

                dst[dst_idx + c] = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    dst
}

/// Capture current cursor and return event if changed.
/// Supports animated cursors by periodically re-capturing images
/// even when the Windows cursor handle hasn't changed.
fn capture_cursor() -> Result<Option<CursorEvent>> {
    unsafe {
        let mut cursor_info = CURSORINFO {
            cbSize: mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };

        if GetCursorInfo(&mut cursor_info).is_err() {
            return Err(anyhow!("GetCursorInfo failed"));
        }

        // Cursor hidden
        if cursor_info.flags.0 & CURSOR_SHOWING.0 == 0 {
            let mut last = LAST_CURSOR_HANDLE.lock().unwrap();
            if *last != 0 {
                *last = 0;
                *LAST_CURSOR_ID.lock().unwrap() = None;
                *ANIM_PROBE_COUNT.lock().unwrap() = 0;
                *LAST_FRAME_CHANGE_MS.lock().unwrap() = 0;
                debug!("Cursor hidden");
                return Ok(Some(CursorEvent::CursorHidden));
            }
            return Ok(None);
        }

        let hcursor = cursor_info.hCursor;
        let cursor_handle = hcursor.0 as isize;

        // Check if cursor handle changed
        let handle_changed = {
            let last = LAST_CURSOR_HANDLE.lock().unwrap();
            cursor_handle != *last
        };

        if handle_changed {
            // New cursor handle — always capture
            {
                let mut last = LAST_CURSOR_HANDLE.lock().unwrap();
                *last = cursor_handle;
            }
            // Reset animation probing for this new handle
            *ANIM_PROBE_COUNT.lock().unwrap() = 0;
            *LAST_FRAME_CHANGE_MS.lock().unwrap() = 0;
            *AVG_FRAME_DELAY.lock().unwrap() = 60;
            *HANDLE_FIRST_SEEN_MS.lock().unwrap() = get_timestamp();

            // Capture the cursor image
            let raw = capture_cursor_image(hcursor)?;
            let cursor_id = raw.id.clone();

            // Update last cursor ID
            *LAST_CURSOR_ID.lock().unwrap() = Some(cursor_id.clone());

            // Check if already cached (same visual cursor, possibly from a different handle)
            {
                let cache_guard = RAW_CURSOR_CACHE.lock().unwrap();
                let cache = cache_guard.as_ref().unwrap();
                if cache.contains_key(&cursor_id) {
                    debug!("Cursor cached (by id), sending change event: {}", cursor_id);
                    return Ok(Some(CursorEvent::CursorChanged(cursor_id)));
                }
            }

            // New cursor — cache it
            debug!("New cursor detected: handle={}, id={}", cursor_handle, cursor_id);
            {
                let mut cache_guard = RAW_CURSOR_CACHE.lock().unwrap();
                let cache = cache_guard.as_mut().unwrap();

                let w = raw.width;
                let h = raw.height;
                let rgba_len = raw.rgba_pixels.len();
                let raw_entry = RawCachedCursor {
                    id: raw.id,
                    rgba_pixels: raw.rgba_pixels,
                    width: w,
                    height: h,
                    hotspot_x: raw.hotspot_x,
                    hotspot_y: raw.hotspot_y,
                    is_animated: false,
                    frame_delay_ms: 0,
                };
                cache.insert(cursor_id.clone(), raw_entry);

                // Trim cache if too large
                if cache.len() > 100 {
                    let keys: Vec<_> = cache.keys().cloned().collect();
                    for key in keys.iter().take(50) {
                        cache.remove(key);
                    }
                    debug!("Cache trimmed to {} entries", cache.len());
                }

                debug!("Cached cursor: id={}, size={}x{}, rgba={} bytes",
                    cursor_id, w, h, rgba_len);
            }

            Ok(Some(CursorEvent::CursorChanged(cursor_id)))
        } else {
            // Same cursor handle — check for animated cursor frame changes
            let is_animated = ANIMATED_HANDLES.lock().unwrap()
                .as_ref().map_or(false, |s| s.contains(&cursor_handle));
            let is_static = STATIC_HANDLES.lock().unwrap()
                .as_ref().map_or(false, |s| s.contains(&cursor_handle));

            if is_static {
                return Ok(None); // Confirmed non-animated, skip
            }

            let probe_count = {
                let mut count = ANIM_PROBE_COUNT.lock().unwrap();
                *count += 1;
                *count
            };

            // Probing schedule:
            // - Known animated: every frame (16ms) for smooth frame capture
            // - Unknown: every 3 frames for the first 60 probes (~1 second)
            // - After 60 probes with no animation: mark as static
            let should_probe = if is_animated {
                true
            } else if probe_count <= 60 {
                probe_count % 3 == 0
            } else {
                // Done probing, no animation found — mark as static
                if probe_count == 61 {
                    STATIC_HANDLES.lock().unwrap().as_mut().unwrap().insert(cursor_handle);
                    debug!("Cursor handle={} confirmed static after {} probes", cursor_handle, probe_count);
                }
                false
            };

            if !should_probe {
                return Ok(None);
            }

            // Re-capture the cursor image to check for frame change
            let raw = capture_cursor_image(hcursor)?;
            let cursor_id = raw.id.clone();

            // Compare with last known cursor_id
            let last_id = LAST_CURSOR_ID.lock().unwrap().clone();
            if last_id.as_deref() == Some(&cursor_id) {
                return Ok(None); // Same frame, no change
            }

            // Frame changed! This is (or confirmed to be) an animated cursor
            let now_ms = get_timestamp();
            let frame_delay = {
                let mut last_change = LAST_FRAME_CHANGE_MS.lock().unwrap();
                let delay = if *last_change > 0 {
                    (now_ms.saturating_sub(*last_change)) as u32
                } else {
                    60 // Default initial estimate
                };
                *last_change = now_ms;
                delay.max(16).min(1000) // Clamp to reasonable range
            };

            // Update average frame delay (exponential moving average)
            let avg_delay = {
                let mut avg = AVG_FRAME_DELAY.lock().unwrap();
                if is_animated {
                    *avg = (*avg * 3 + frame_delay) / 4;
                } else {
                    *avg = frame_delay;
                }
                *avg
            };

            if !is_animated {
                ANIMATED_HANDLES.lock().unwrap().as_mut().unwrap().insert(cursor_handle);
                info!("Detected animated cursor: handle={}, first frame delay={}ms", cursor_handle, frame_delay);
            }

            // Update last cursor ID
            *LAST_CURSOR_ID.lock().unwrap() = Some(cursor_id.clone());

            // Cache this animation frame if not already cached
            {
                let mut cache_guard = RAW_CURSOR_CACHE.lock().unwrap();
                let cache = cache_guard.as_mut().unwrap();

                if !cache.contains_key(&cursor_id) {
                    let raw_entry = RawCachedCursor {
                        id: raw.id.clone(),
                        rgba_pixels: raw.rgba_pixels,
                        width: raw.width,
                        height: raw.height,
                        hotspot_x: raw.hotspot_x,
                        hotspot_y: raw.hotspot_y,
                        is_animated: true,
                        frame_delay_ms: avg_delay,
                    };
                    cache.insert(cursor_id.clone(), raw_entry);
                    debug!("Cached animated frame: id={}, size={}x{}, delay={}ms",
                        cursor_id, raw.width, raw.height, avg_delay);
                }

                // Also update existing frames with latest delay estimate
                // (delay stabilizes over time)
                for entry in cache.values_mut() {
                    if entry.is_animated {
                        entry.frame_delay_ms = avg_delay;
                    }
                }

                // Trim cache if too large
                if cache.len() > 100 {
                    let keys: Vec<_> = cache.keys().cloned().collect();
                    for key in keys.iter().take(50) {
                        cache.remove(key);
                    }
                }
            }

            debug!("Animated cursor frame: id={}, delay={}ms (avg={}ms)",
                cursor_id, frame_delay, avg_delay);
            Ok(Some(CursorEvent::CursorChanged(cursor_id)))
        }
    }
}

/// Capture cursor image from HCURSOR, returns raw RGBA pixels + metadata.
/// Uses DrawIconEx for color cursors to support animated cursor frame capture.
unsafe fn capture_cursor_image(hcursor: HCURSOR) -> Result<RawCachedCursor> {
    let hicon = CopyIcon(hcursor)?;
    let mut icon_info = ICONINFO::default();

    if GetIconInfo(hicon, &mut icon_info).is_err() {
        DestroyIcon(hicon)?;
        return Err(anyhow!("GetIconInfo failed"));
    }

    let hotspot_x = icon_info.xHotspot as i32;
    let hotspot_y = icon_info.yHotspot as i32;
    let is_monochrome = icon_info.hbmColor.is_invalid();

    // Get dimensions from bitmap info
    let (width, height) = if !icon_info.hbmColor.is_invalid() {
        let mut bmp = BITMAP::default();
        GetObjectW(
            icon_info.hbmColor,
            mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        (bmp.bmWidth as u32, bmp.bmHeight as u32)
    } else if !icon_info.hbmMask.is_invalid() {
        let mut bmp = BITMAP::default();
        GetObjectW(
            icon_info.hbmMask,
            mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        // Monochrome mask is double height (AND + XOR)
        (bmp.bmWidth as u32, (bmp.bmHeight / 2) as u32)
    } else {
        DestroyIcon(hicon)?;
        return Err(anyhow!("No bitmap data in cursor"));
    };

    if width == 0 || height == 0 {
        if !icon_info.hbmColor.is_invalid() { let _ = DeleteObject(icon_info.hbmColor); }
        if !icon_info.hbmMask.is_invalid() { let _ = DeleteObject(icon_info.hbmMask); }
        DestroyIcon(hicon)?;
        return Err(anyhow!("Cursor has zero dimensions: {}x{}", width, height));
    }

    if is_monochrome {
        // Monochrome cursor — use mask-based method (never animated)
        let result = get_monochrome_cursor_rgba(icon_info.hbmMask)?;
        let _ = DeleteObject(icon_info.hbmMask);
        DestroyIcon(hicon)?;
        let cursor_id = format!("cur_{}", blake3::hash(&result.0).to_hex()[..12].to_string());
        return Ok(RawCachedCursor {
            id: cursor_id,
            rgba_pixels: result.0,
            width: result.1,
            height: result.2,
            hotspot_x,
            hotspot_y,
            is_animated: false,
            frame_delay_ms: 0,
        });
    }

    // Color cursor — clean up GetIconInfo bitmaps and use DrawIconEx
    if !icon_info.hbmColor.is_invalid() { let _ = DeleteObject(icon_info.hbmColor); }
    if !icon_info.hbmMask.is_invalid() { let _ = DeleteObject(icon_info.hbmMask); }
    DestroyIcon(hicon)?;

    // Calculate animation step based on time elapsed since cursor handle appeared.
    // Default frame rate: ~60ms per frame (1 jiffy), standard for Windows animated cursors.
    let step = {
        let first_seen = *HANDLE_FIRST_SEEN_MS.lock().unwrap();
        let now = get_timestamp();
        (now.saturating_sub(first_seen) / 60) as u32
    };

    // Render using DrawIconEx with dual-background technique for correct alpha
    let rgba_pixels = render_cursor_frame(hcursor, width, height, step)?;
    let cursor_id = format!("cur_{}", blake3::hash(&rgba_pixels).to_hex()[..12].to_string());

    Ok(RawCachedCursor {
        id: cursor_id,
        rgba_pixels,
        width,
        height,
        hotspot_x,
        hotspot_y,
        is_animated: false,
        frame_delay_ms: 0,
    })
}

/// Render a cursor frame using DrawIconEx with dual-background technique.
///
/// Draws the cursor twice — on black and white backgrounds — to correctly
/// recover per-pixel alpha for all cursor types (32-bit ARGB, masked, XOR).
/// The `step` parameter selects which frame to render for animated cursors.
unsafe fn render_cursor_frame(
    hcursor: HCURSOR,
    width: u32,
    height: u32,
    step: u32,
) -> Result<Vec<u8>> {
    let hdc_screen = GetDC(None);
    if hdc_screen.is_invalid() {
        return Err(anyhow!("GetDC failed"));
    }

    let hdc_mem = CreateCompatibleDC(hdc_screen);
    if hdc_mem.is_invalid() {
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("CreateCompatibleDC failed"));
    }

    let hbmp = CreateCompatibleBitmap(hdc_screen, width as i32, height as i32);
    if hbmp.is_invalid() {
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("CreateCompatibleBitmap failed"));
    }

    let old_obj = SelectObject(hdc_mem, hbmp);

    let mut bmp_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    // Pass 1: Draw cursor on BLACK background
    let _ = PatBlt(hdc_mem, 0, 0, width as i32, height as i32, BLACKNESS);
    if DrawIconEx(hdc_mem, 0, 0, hcursor, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (black pass)"));
    }

    let mut black_pixels = vec![0u8; (width * height * 4) as usize];
    GetDIBits(
        hdc_mem, hbmp, 0, height,
        Some(black_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info, DIB_RGB_COLORS,
    );

    // Pass 2: Draw cursor on WHITE background
    let _ = PatBlt(hdc_mem, 0, 0, width as i32, height as i32, WHITENESS);
    if DrawIconEx(hdc_mem, 0, 0, hcursor, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (white pass)"));
    }

    let mut white_pixels = vec![0u8; (width * height * 4) as usize];
    GetDIBits(
        hdc_mem, hbmp, 0, height,
        Some(white_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info, DIB_RGB_COLORS,
    );

    // Cleanup GDI resources
    SelectObject(hdc_mem, old_obj);
    let _ = DeleteObject(hbmp);
    let _ = DeleteDC(hdc_mem);
    ReleaseDC(None, hdc_screen);

    // Compute RGBA with correct alpha from dual-render results.
    //
    // On black background: rendered = src_color * alpha / 255
    // On white background: rendered = src_color * alpha / 255 + 255 * (255 - alpha) / 255
    // Difference: white - black = 255 - alpha
    // Therefore: alpha = 255 - (white - black)
    //
    // GetDIBits returns pixels in BGRA order.
    let pixel_count = (width * height) as usize;
    let mut rgba = vec![0u8; pixel_count * 4];

    for i in 0..pixel_count {
        let idx = i * 4;
        let b_black = black_pixels[idx] as i32;
        let g_black = black_pixels[idx + 1] as i32;
        let r_black = black_pixels[idx + 2] as i32;

        let b_white = white_pixels[idx] as i32;
        let g_white = white_pixels[idx + 1] as i32;
        let r_white = white_pixels[idx + 2] as i32;

        // Alpha from each channel; use minimum for robustness against rounding
        let alpha = (255 - (r_white - r_black))
            .min(255 - (g_white - g_black))
            .min(255 - (b_white - b_black))
            .clamp(0, 255) as u8;

        // Un-premultiply to recover straight-alpha RGB
        let (r, g, b) = if alpha > 0 {
            let a = alpha as f32;
            (
                (r_black as f32 * 255.0 / a).round().clamp(0.0, 255.0) as u8,
                (g_black as f32 * 255.0 / a).round().clamp(0.0, 255.0) as u8,
                (b_black as f32 * 255.0 / a).round().clamp(0.0, 255.0) as u8,
            )
        } else {
            (0, 0, 0)
        };

        rgba[i * 4] = r;
        rgba[i * 4 + 1] = g;
        rgba[i * 4 + 2] = b;
        rgba[i * 4 + 3] = alpha;
    }

    Ok(rgba)
}

/// Get RGBA pixels from a color HBITMAP
#[allow(dead_code)]
unsafe fn get_color_bitmap_rgba(
    hbitmap: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<(Vec<u8>, u32, u32)> {
    // Get bitmap dimensions via GetObject
    let mut bmp = BITMAP::default();
    let obj_size = GetObjectW(
        hbitmap,
        mem::size_of::<BITMAP>() as i32,
        Some(&mut bmp as *mut _ as *mut _),
    );
    if obj_size == 0 {
        return Err(anyhow!("GetObject failed for color bitmap"));
    }

    let width = bmp.bmWidth as u32;
    let height = bmp.bmHeight as u32;

    if width == 0 || height == 0 {
        return Err(anyhow!("Bitmap has zero dimensions: {}x{}", width, height));
    }

    info!("Color bitmap: {}x{}, planes={}, bpp={}", width, height, bmp.bmPlanes, bmp.bmBitsPixel);

    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return Err(anyhow!("CreateCompatibleDC failed"));
    }

    // Set up BITMAPINFO for top-down 32bpp DIB
    let mut bmp_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // negative = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut pixels = vec![0u8; (width * height * 4) as usize];

    let result = GetDIBits(
        hdc,
        hbitmap,
        0,
        height,
        Some(pixels.as_mut_ptr() as *mut _),
        &mut bmp_info,
        DIB_RGB_COLORS,
    );

    let _ = DeleteDC(hdc);

    if result == 0 {
        return Err(anyhow!("GetDIBits (pixels) failed"));
    }

    // Convert BGRA to RGBA
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2); // B <-> R
    }

    Ok((pixels, width, height))
}

/// Apply AND mask to RGBA pixels for transparency
#[allow(dead_code)]
unsafe fn apply_mask_to_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    hmask: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<Vec<u8>> {
    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return Err(anyhow!("CreateCompatibleDC failed for mask"));
    }

    let mut bmp_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut mask_pixels = vec![0u8; (width * height * 4) as usize];

    let result = GetDIBits(
        hdc,
        hmask,
        0,
        height,
        Some(mask_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info,
        DIB_RGB_COLORS,
    );

    let _ = DeleteDC(hdc);

    if result == 0 {
        return Err(anyhow!("GetDIBits (mask) failed"));
    }

    let mut output = rgba.to_vec();

    // If any pixel has alpha == 0 in original AND the mask pixel is white (0xFF),
    // it means transparent. Only apply mask if original alpha is all zero.
    let has_alpha = rgba.chunks_exact(4).any(|c| c[3] != 0);

    if !has_alpha {
        // No pre-existing alpha — combine AND mask with XOR color bitmap.
        //
        // Windows cursor rendering formula:
        //   result = (screen AND and_mask) XOR xor_image
        //
        // AND=0x00                → display XOR pixel directly (opaque)
        // AND=0xFF, XOR=black     → transparent (screen shows through)
        // AND=0xFF, XOR=non-zero  → screen inversion area (e.g. I-beam cursor)
        //     Since we can't do real inversion, render as a visible black pixel.
        for (i, chunk) in output.chunks_exact_mut(4).enumerate() {
            let mask_idx = i * 4;
            let and_val = mask_pixels[mask_idx]; // AND mask (0x00 = opaque, 0xFF = transparent/XOR)

            if and_val == 0x00 {
                // AND=0: show XOR pixel as-is (opaque)
                chunk[3] = 255;
            } else {
                // AND=1: check XOR value
                let r = chunk[0];
                let g = chunk[1];
                let b = chunk[2];
                if r == 0 && g == 0 && b == 0 {
                    // AND=1, XOR=black: transparent
                    chunk[3] = 0;
                } else {
                    // AND=1, XOR=non-zero: inversion area (e.g. text-select I-beam)
                    // Render as black so the cursor is visible on any background
                    chunk[0] = 0;
                    chunk[1] = 0;
                    chunk[2] = 0;
                    chunk[3] = 255;
                }
            }
        }
    }

    Ok(output)
}

/// Get RGBA pixels from a monochrome cursor mask bitmap
unsafe fn get_monochrome_cursor_rgba(
    hmask: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<(Vec<u8>, u32, u32)> {
    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return Err(anyhow!("CreateCompatibleDC failed"));
    }

    // Get mask dimensions via GetObject
    let mut bmp = BITMAP::default();
    let obj_size = GetObjectW(
        hmask,
        mem::size_of::<BITMAP>() as i32,
        Some(&mut bmp as *mut _ as *mut _),
    );
    if obj_size == 0 {
        let _ = DeleteDC(hdc);
        return Err(anyhow!("GetObject failed for monochrome mask"));
    }

    let width = bmp.bmWidth as u32;
    let full_height = bmp.bmHeight as u32;

    info!("Monochrome mask: {}x{} (full height)", width, full_height);
    // Monochrome cursor mask is double height: top half = AND mask, bottom half = XOR mask
    let height = full_height / 2;

    let mut bmp_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(full_height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut mask_pixels = vec![0u8; (width * full_height * 4) as usize];

    let result = GetDIBits(
        hdc,
        hmask,
        0,
        full_height,
        Some(mask_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info,
        DIB_RGB_COLORS,
    );

    let _ = DeleteDC(hdc);

    if result == 0 {
        return Err(anyhow!("GetDIBits (mono pixels) failed"));
    }

    let row_bytes = (width * 4) as usize;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    for y in 0..height as usize {
        for x in 0..width as usize {
            let and_idx = y * row_bytes + x * 4;
            let xor_idx = (y + height as usize) * row_bytes + x * 4;
            let out_idx = y * row_bytes + x * 4;

            let and_val = mask_pixels[and_idx]; // AND mask
            let xor_val = mask_pixels[xor_idx]; // XOR mask

            if and_val == 0 && xor_val == 0 {
                // Black pixel
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 255;
            } else if and_val == 0xFF && xor_val == 0xFF {
                // White pixel (inverted - render as white, semi-transparent)
                rgba[out_idx] = 255;
                rgba[out_idx + 1] = 255;
                rgba[out_idx + 2] = 255;
                rgba[out_idx + 3] = 200;
            } else if and_val == 0xFF && xor_val == 0 {
                // Transparent
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 0;
            } else {
                // Black opaque
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 255;
            }
        }
    }

    Ok((rgba, width, height))
}

/// Encode RGBA pixels to WebP (lossless for cursor quality)
fn encode_webp(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let encoder = webp::Encoder::from_rgba(rgba, width, height);
    // Use lossless encoding for pixel-perfect cursor images
    let memory = encoder.encode_lossless();
    Ok(memory.to_vec())
}

/// Get current timestamp (milliseconds)
fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

