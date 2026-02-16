use anyhow::{anyhow, Result};
use std::collections::HashMap;
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
    CopyIcon, DestroyIcon, DrawIconEx, GetCursorInfo, GetIconInfo, LoadCursorW, CURSORINFO, CURSOR_SHOWING,
    DI_NORMAL, HCURSOR, HICON, ICONINFO,
    IDC_ARROW, IDC_IBEAM, IDC_WAIT, IDC_CROSS, IDC_UPARROW,
    IDC_SIZENWSE, IDC_SIZENESW, IDC_SIZEWE, IDC_SIZENS, IDC_SIZEALL,
    IDC_NO, IDC_HAND, IDC_APPSTARTING, IDC_HELP,
};
use windows::core::PCWSTR;

use crate::cursor::{
    cursor_message::Payload, CursorData, CursorMessage, NativeCursor, MessageType,
};

/// Cursor event for broadcasting to clients
#[derive(Clone, Debug)]
pub enum CursorEvent {
    /// Cursor changed - carries cursor_id
    CursorChanged(String),
    /// Cursor hidden
    CursorHidden,
    /// XOR/inversion cursor detected - client should use native CSS cursor
    CursorNative(String),
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
static CURSOR_CACHE: Mutex<Option<HashMap<String, CachedCursor>>> = Mutex::new(None);

/// Last Windows cursor handle (HCURSOR value)
static LAST_CURSOR_HANDLE: Mutex<isize> = Mutex::new(0);

/// Last cursor_id for detecting changes
static LAST_CURSOR_ID: Mutex<Option<String>> = Mutex::new(None);

/// Default frame delay for animated cursors (ms) - Windows standard is 1 jiffy = ~60ms
const ANIM_FRAME_DELAY_MS: i32 = 60;

/// Maximum animation frames to probe (safety limit)
const MAX_ANIM_FRAMES: u32 = 120;

/// Result of capturing a cursor - either an image or a native cursor signal
enum CaptureResult {
    /// Successfully captured cursor image
    Cursor(CachedCursor),
    /// XOR cursor detected - use native CSS cursor with given name
    Native(String),
}

/// Run cursor capture loop
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorEvent>) -> Result<()> {
    // Enable DPI awareness
    unsafe {
        let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
    }

    // Initialize cache
    {
        let mut cache = CURSOR_CACHE.lock().unwrap();
        if cache.is_none() {
            *cache = Some(HashMap::new());
            info!("Cursor cache initialized");
        }
    }

    let dpi_scale = get_dpi_scale();
    info!("Starting cursor capture (DPI scale: {:.2})", dpi_scale);

    let mut poll_interval = interval(Duration::from_millis(16)); // ~60fps check

    loop {
        poll_interval.tick().await;

        match capture_cursor() {
            Ok(Some(event)) => {
                if tx.send(event).await.is_err() {
                    warn!("Receiver closed, stopping cursor capture");
                    break;
                }
            }
            Ok(None) => {}
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

/// Create native cursor message (tells client to use local/CSS cursor rendering)
pub fn create_native_cursor_message(cursor_name: &str) -> CursorMessage {
    CursorMessage {
        r#type: MessageType::CursorNative.into(),
        payload: Some(Payload::NativeCursor(NativeCursor {
            cursor_name: cursor_name.to_string(),
        })),
        timestamp: get_timestamp(),
    }
}

/// Capture current cursor and return event if changed.
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

        if !handle_changed {
            return Ok(None); // Same cursor, nothing to do
        }

        // New cursor handle
        {
            let mut last = LAST_CURSOR_HANDLE.lock().unwrap();
            *last = cursor_handle;
        }

        // Capture the cursor (with all animation frames if animated)
        let result = capture_full_cursor(hcursor)?;

        match result {
            CaptureResult::Native(css_name) => {
                // XOR cursor - tell client to use native CSS cursor
                let native_id = format!("native:{}", css_name);
                *LAST_CURSOR_ID.lock().unwrap() = Some(native_id.clone());
                info!("XOR cursor detected, using native cursor: {}", css_name);
                Ok(Some(CursorEvent::CursorNative(css_name)))
            }
            CaptureResult::Cursor(cached) => {
                let cursor_id = cached.id.clone();

                // Update last cursor ID
                *LAST_CURSOR_ID.lock().unwrap() = Some(cursor_id.clone());

                // Check if already in cache
                {
                    let cache_guard = CURSOR_CACHE.lock().unwrap();
                    let cache = cache_guard.as_ref().unwrap();
                    if cache.contains_key(&cursor_id) {
                        debug!("Cursor already cached: {}", cursor_id);
                        return Ok(Some(CursorEvent::CursorChanged(cursor_id)));
                    }
                }

                // Cache the new cursor
                info!(
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

                Ok(Some(CursorEvent::CursorChanged(cursor_id)))
            }
        }
    }
}

/// Capture a cursor with all its animation frames and encode as WebP.
/// For static cursors: returns a single-frame lossless WebP.
/// For animated cursors: probes all frames via DrawIconEx step parameter,
/// then encodes them as an animated WebP.
/// For XOR/inversion cursors: returns Native with the CSS cursor name.
unsafe fn capture_full_cursor(hcursor: HCURSOR) -> Result<CaptureResult> {
    let hicon = CopyIcon(hcursor)?;
    let mut icon_info = ICONINFO::default();

    if GetIconInfo(hicon, &mut icon_info).is_err() {
        DestroyIcon(hicon)?;
        return Err(anyhow!("GetIconInfo failed"));
    }

    let hotspot_x = icon_info.xHotspot as i32;
    let hotspot_y = icon_info.yHotspot as i32;
    let is_monochrome = icon_info.hbmColor.is_invalid();

    // Get dimensions
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
        (bmp.bmWidth as u32, (bmp.bmHeight / 2) as u32)
    } else {
        DestroyIcon(hicon)?;
        return Err(anyhow!("No bitmap data in cursor"));
    };

    if width == 0 || height == 0 {
        if !icon_info.hbmColor.is_invalid() { let _ = DeleteObject(icon_info.hbmColor); }
        if !icon_info.hbmMask.is_invalid() { let _ = DeleteObject(icon_info.hbmMask); }
        DestroyIcon(hicon)?;
        return Err(anyhow!("Cursor has zero dimensions"));
    }

    // Clean up bitmaps from GetIconInfo
    if !icon_info.hbmColor.is_invalid() { let _ = DeleteObject(icon_info.hbmColor); }
    if !icon_info.hbmMask.is_invalid() { let _ = DeleteObject(icon_info.hbmMask); }
    DestroyIcon(hicon)?;

    if is_monochrome {
        // Monochrome cursors use AND/XOR masks - check if it has XOR inversion pixels.
        // If so, use native cursor rendering instead.
        let hicon_copy = CopyIcon(hcursor)?;
        let mut icon_info2 = ICONINFO::default();
        if GetIconInfo(hicon_copy, &mut icon_info2).is_err() {
            DestroyIcon(hicon_copy)?;
            return Err(anyhow!("GetIconInfo failed (monochrome)"));
        }
        if !icon_info2.hbmColor.is_invalid() { let _ = DeleteObject(icon_info2.hbmColor); }
        let result = get_monochrome_cursor_rgba(icon_info2.hbmMask);
        let _ = DeleteObject(icon_info2.hbmMask);
        DestroyIcon(hicon_copy)?;
        let (rgba, w, h, has_xor) = result?;

        if has_xor {
            // Monochrome cursor with XOR pixels - use native cursor
            let css_name = identify_system_cursor(hcursor);
            info!("Monochrome XOR cursor detected, native cursor: {}", css_name);
            return Ok(CaptureResult::Native(css_name));
        }

        let webp_data = encode_static_webp(&rgba, w, h)?;
        let cursor_id = format!("cur_{}", &blake3::hash(&rgba).to_hex()[..12]);

        return Ok(CaptureResult::Cursor(CachedCursor {
            id: cursor_id,
            webp_data,
            width: w,
            height: h,
            hotspot_x,
            hotspot_y,
            is_animated: false,
            frame_count: 1,
            frame_delay_ms: 0,
        }));
    }

    // Color cursor - render first frame to check for XOR pixels
    let hicon_raw: HICON = mem::transmute(hcursor);
    let (first_frame, has_xor) = render_cursor_frame_with_xor_detection(hicon_raw, width, height, 0)?;

    if has_xor {
        // Color cursor with XOR pixels - use native cursor
        let css_name = identify_system_cursor(hcursor);
        info!("Color XOR cursor detected, native cursor: {}", css_name);
        return Ok(CaptureResult::Native(css_name));
    }

    // No XOR - proceed with normal capture
    // Probe animation frames using the original HCURSOR handle.
    let frames = probe_animation_frames_with_first(hicon_raw, width, height, first_frame)?;

    if frames.len() <= 1 {
        // Static cursor
        let rgba = &frames[0];

        let webp_data = encode_static_webp(rgba, width, height)?;
        let cursor_id = format!("cur_{}", &blake3::hash(rgba).to_hex()[..12]);

        Ok(CaptureResult::Cursor(CachedCursor {
            id: cursor_id,
            webp_data,
            width,
            height,
            hotspot_x,
            hotspot_y,
            is_animated: false,
            frame_count: 1,
            frame_delay_ms: 0,
        }))
    } else {
        // Animated cursor - encode as animated WebP
        let frame_count = frames.len() as u32;
        let frame_delay = ANIM_FRAME_DELAY_MS as u32;

        // Build a combined hash from all frames for a stable cursor_id
        let mut hasher_input = Vec::new();
        for frame in &frames {
            hasher_input.extend_from_slice(blake3::hash(frame).as_bytes());
        }
        let cursor_id = format!("ani_{}", &blake3::hash(&hasher_input).to_hex()[..12]);

        let webp_data = encode_animated_webp(&frames, width, height, ANIM_FRAME_DELAY_MS)?;

        info!(
            "Animated cursor encoded: {} frames, {}x{}, delay={}ms, webp={} bytes",
            frame_count, width, height, frame_delay, webp_data.len()
        );

        Ok(CaptureResult::Cursor(CachedCursor {
            id: cursor_id,
            webp_data,
            width,
            height,
            hotspot_x,
            hotspot_y,
            is_animated: true,
            frame_count,
            frame_delay_ms: frame_delay,
        }))
    }
}

/// Probe all unique animation frames for a cursor, reusing an already-rendered first frame.
/// Returns a Vec of RGBA frame data. If cursor is not animated, returns 1 frame.
unsafe fn probe_animation_frames_with_first(
    hicon: HICON,
    width: u32,
    height: u32,
    first_frame: Vec<u8>,
) -> Result<Vec<Vec<u8>>> {
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut frame_hashes: Vec<String> = Vec::new();

    let first_hash = blake3::hash(&first_frame).to_hex()[..16].to_string();
    frames.push(first_frame);
    frame_hashes.push(first_hash);

    for step in 1..MAX_ANIM_FRAMES {
        let rgba = match render_cursor_frame(hicon, width, height, step) {
            Ok(data) => data,
            Err(e) => {
                debug!("Frame probe ended at step {} ({}): {} frames captured", step, e, frames.len());
                break;
            }
        };
        let hash = blake3::hash(&rgba).to_hex()[..16].to_string();

        if hash == frame_hashes[0] {
            // We've looped back to the first frame - animation cycle complete
            debug!("Animation cycle detected at step {}: {} unique frames", step, frames.len());
            break;
        } else if frame_hashes.contains(&hash) {
            // Duplicate frame within cycle, stop
            debug!("Duplicate frame at step {} (not first frame): stopping", step);
            break;
        } else {
            frames.push(rgba);
            frame_hashes.push(hash);
        }
    }

    Ok(frames)
}

/// Render a single cursor frame using DrawIconEx with dual-background technique
/// for correct per-pixel alpha recovery.
unsafe fn render_cursor_frame(
    hicon: HICON,
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
    if DrawIconEx(hdc_mem, 0, 0, hicon, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (black pass, step={})", step));
    }

    let mut black_pixels = vec![0u8; (width * height * 4) as usize];
    GetDIBits(
        hdc_mem, hbmp, 0, height,
        Some(black_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info, DIB_RGB_COLORS,
    );

    // Pass 2: Draw cursor on WHITE background
    let _ = PatBlt(hdc_mem, 0, 0, width as i32, height as i32, WHITENESS);
    if DrawIconEx(hdc_mem, 0, 0, hicon, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (white pass, step={})", step));
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

        // Detect XOR/inversion pixels (black > white -> impossible with normal alpha)
        let is_xor = r_black > r_white || g_black > g_white || b_black > b_white;

        if is_xor {
            // XOR cursor area - the black-background pass reveals the raw XOR mask
            // (black XOR mask = mask). Since XOR cursors invert screen content and
            // most backgrounds are light, we invert the mask so the cursor appears
            // dark and visible (e.g. white mask -> black cursor).
            rgba[i * 4] = (255 - r_black) as u8;
            rgba[i * 4 + 1] = (255 - g_black) as u8;
            rgba[i * 4 + 2] = (255 - b_black) as u8;
            rgba[i * 4 + 3] = 255;
        } else {
            // Standard alpha: alpha = 255 - (white - black)
            let alpha = (255 - (r_white - r_black))
                .min(255 - (g_white - g_black))
                .min(255 - (b_white - b_black))
                .clamp(0, 255) as u8;

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
    }

    Ok(rgba)
}

/// Render a single cursor frame and detect XOR pixels.
/// Returns (RGBA data, has_xor) where has_xor indicates the frame contains XOR/inversion pixels.
unsafe fn render_cursor_frame_with_xor_detection(
    hicon: HICON,
    width: u32,
    height: u32,
    step: u32,
) -> Result<(Vec<u8>, bool)> {
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
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    // Pass 1: Draw cursor on BLACK background
    let _ = PatBlt(hdc_mem, 0, 0, width as i32, height as i32, BLACKNESS);
    if DrawIconEx(hdc_mem, 0, 0, hicon, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (black pass, step={})", step));
    }

    let mut black_pixels = vec![0u8; (width * height * 4) as usize];
    GetDIBits(
        hdc_mem, hbmp, 0, height,
        Some(black_pixels.as_mut_ptr() as *mut _),
        &mut bmp_info, DIB_RGB_COLORS,
    );

    // Pass 2: Draw cursor on WHITE background
    let _ = PatBlt(hdc_mem, 0, 0, width as i32, height as i32, WHITENESS);
    if DrawIconEx(hdc_mem, 0, 0, hicon, width as i32, height as i32, step, None, DI_NORMAL).is_err() {
        SelectObject(hdc_mem, old_obj);
        let _ = DeleteObject(hbmp);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
        return Err(anyhow!("DrawIconEx failed (white pass, step={})", step));
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

    // Compute RGBA with XOR detection
    let pixel_count = (width * height) as usize;
    let mut rgba = vec![0u8; pixel_count * 4];
    let mut has_xor = false;

    for i in 0..pixel_count {
        let idx = i * 4;
        let b_black = black_pixels[idx] as i32;
        let g_black = black_pixels[idx + 1] as i32;
        let r_black = black_pixels[idx + 2] as i32;

        let b_white = white_pixels[idx] as i32;
        let g_white = white_pixels[idx + 1] as i32;
        let r_white = white_pixels[idx + 2] as i32;

        let is_xor = r_black > r_white || g_black > g_white || b_black > b_white;

        if is_xor {
            has_xor = true;
            // Still produce valid RGBA for fallback (inverted XOR mask)
            rgba[i * 4] = (255 - r_black) as u8;
            rgba[i * 4 + 1] = (255 - g_black) as u8;
            rgba[i * 4 + 2] = (255 - b_black) as u8;
            rgba[i * 4 + 3] = 255;
        } else {
            let alpha = (255 - (r_white - r_black))
                .min(255 - (g_white - g_black))
                .min(255 - (b_white - b_black))
                .clamp(0, 255) as u8;

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
    }

    Ok((rgba, has_xor))
}

/// Identify a system cursor by comparing its HCURSOR handle with known system cursors.
/// Returns a CSS cursor value string.
unsafe fn identify_system_cursor(hcursor: HCURSOR) -> String {
    let cursor_mappings: &[(PCWSTR, &str)] = &[
        (IDC_ARROW, "default"),
        (IDC_IBEAM, "text"),
        (IDC_WAIT, "wait"),
        (IDC_CROSS, "crosshair"),
        (IDC_UPARROW, "default"),
        (IDC_SIZENWSE, "nwse-resize"),
        (IDC_SIZENESW, "nesw-resize"),
        (IDC_SIZEWE, "ew-resize"),
        (IDC_SIZENS, "ns-resize"),
        (IDC_SIZEALL, "move"),
        (IDC_NO, "not-allowed"),
        (IDC_HAND, "pointer"),
        (IDC_APPSTARTING, "progress"),
        (IDC_HELP, "help"),
    ];

    for (idc, css_name) in cursor_mappings {
        if let Ok(sys_cursor) = LoadCursorW(None, *idc) {
            if sys_cursor.0 == hcursor.0 {
                debug!("Identified system cursor: {} -> {}", idc.0 as usize, css_name);
                return css_name.to_string();
            }
        }
    }

    // Could not identify - use "default" as fallback
    debug!("Unknown XOR cursor handle {:?}, using 'default'", hcursor.0);
    "default".to_string()
}

/// Encode RGBA pixels as a static (single-frame) lossless WebP
fn encode_static_webp(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let encoder = webp::Encoder::from_rgba(rgba, width, height);
    let memory = encoder.encode_lossless();
    Ok(memory.to_vec())
}

/// Encode multiple RGBA frames as an animated WebP.
fn encode_animated_webp(
    frames: &[Vec<u8>],
    width: u32,
    height: u32,
    frame_delay_ms: i32,
) -> Result<Vec<u8>> {
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

/// Get RGBA pixels from a monochrome cursor mask bitmap.
/// Returns (rgba, width, height, has_xor) where has_xor indicates XOR inversion pixels exist.
unsafe fn get_monochrome_cursor_rgba(
    hmask: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<(Vec<u8>, u32, u32, bool)> {
    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return Err(anyhow!("CreateCompatibleDC failed"));
    }

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
    let mut has_xor = false;

    for y in 0..height as usize {
        for x in 0..width as usize {
            let and_idx = y * row_bytes + x * 4;
            let xor_idx = (y + height as usize) * row_bytes + x * 4;
            let out_idx = y * row_bytes + x * 4;

            let and_val = mask_pixels[and_idx];
            let xor_val = mask_pixels[xor_idx];

            if and_val == 0 && xor_val == 0 {
                // AND=0, XOR=0: black opaque pixel
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 255;
            } else if and_val == 0 && xor_val == 0xFF {
                // AND=0, XOR=1: white opaque pixel
                rgba[out_idx] = 255;
                rgba[out_idx + 1] = 255;
                rgba[out_idx + 2] = 255;
                rgba[out_idx + 3] = 255;
            } else if and_val == 0xFF && xor_val == 0xFF {
                // AND=1, XOR=1: screen inversion area
                has_xor = true;
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 200;
            } else if and_val == 0xFF && xor_val == 0 {
                // AND=1, XOR=0: transparent
                rgba[out_idx] = 0;
                rgba[out_idx + 1] = 0;
                rgba[out_idx + 2] = 0;
                rgba[out_idx + 3] = 0;
            } else {
                // Mixed - use XOR value as color
                rgba[out_idx] = xor_val;
                rgba[out_idx + 1] = xor_val;
                rgba[out_idx + 2] = xor_val;
                rgba[out_idx + 3] = 255;
            }
        }
    }

    Ok((rgba, width, height, has_xor))
}

/// Get current timestamp (milliseconds)
fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
