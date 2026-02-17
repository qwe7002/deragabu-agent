use anyhow::{anyhow, Result};
use std::mem;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetObjectW, PatBlt, ReleaseDC, SelectObject,
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACKNESS, DIB_RGB_COLORS, WHITENESS,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, DrawIconEx, GetCursorInfo, GetCursorPos, GetIconInfo,
    GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HCURSOR, HICON, ICONINFO,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::{
    CachedCursor, CursorEvent, LAST_CURSOR_ID,
    cache_cursor, encode_animated_webp, encode_static_webp,
    expand_canvas, add_white_outline, init_cache,
};

/// Last Windows cursor handle (HCURSOR value)
static LAST_CURSOR_HANDLE: Mutex<isize> = Mutex::new(0);

/// Default frame delay for animated cursors (ms) - Windows standard is 1 jiffy = ~60ms
const ANIM_FRAME_DELAY_MS: i32 = 60;

/// Maximum animation frames to probe (safety limit)
const MAX_ANIM_FRAMES: u32 = 120;

/// Result of capturing a cursor
enum CaptureResult {
    /// Successfully captured cursor image
    Cursor(CachedCursor),
}

/// Summary of XOR pixel statistics for logging
struct XorShape {
    /// Number of XOR pixels
    count: u32,
}

impl XorShape {
    fn new() -> Self {
        XorShape { count: 0 }
    }

    fn add_pixel(&mut self, _x: u32, _y: u32) {
        self.count += 1;
    }
}

/// Get system DPI scale factor
pub fn get_dpi_scale() -> f32 {
    unsafe {
        let dpi = GetDpiForSystem();
        dpi as f32 / 96.0
    }
}

/// Check if the cursor position is at or beyond the virtual screen edge.
/// When the cursor moves past the screen boundary, Windows may report it as hidden,
/// but we want to keep the last cursor visible on the client in that case.
fn is_cursor_at_screen_edge() -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return false;
        }

        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        const EDGE_MARGIN: i32 = 2;

        pt.x <= vx + EDGE_MARGIN
            || pt.y <= vy + EDGE_MARGIN
            || pt.x >= vx + vw - EDGE_MARGIN
            || pt.y >= vy + vh - EDGE_MARGIN
    }
}

/// Run cursor capture loop
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorEvent>) -> Result<()> {
    // Enable DPI awareness
    unsafe {
        let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
    }

    init_cache();

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

        // Cursor not showing
        if cursor_info.flags.0 & CURSOR_SHOWING.0 == 0 {
            // Check if cursor is at screen edge — if so, ignore the hide
            // (Windows reports cursor as hidden when it goes beyond the screen boundary)
            if is_cursor_at_screen_edge() {
                return Ok(None);
            }

            // Genuine hide (e.g. application hid the cursor for text input)
            let mut last = LAST_CURSOR_HANDLE.lock().unwrap();
            if *last != 0 {
                *last = 0;
                *LAST_CURSOR_ID.lock().unwrap() = None;
                debug!("Cursor hidden (not at edge)");
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

        let CaptureResult::Cursor(cached) = result;

        let (cursor_id, _is_new) = cache_cursor(cached);
        Ok(Some(CursorEvent::CursorChanged(cursor_id)))
    }
}

/// Capture a cursor with all its animation frames and encode as WebP.
/// For static cursors: returns a single-frame lossless WebP.
/// For animated cursors: probes all frames via DrawIconEx step parameter,
/// then encodes them as an animated WebP.
/// XOR/inversion cursors are also rendered as images.
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
        // Monochrome cursors use AND/XOR masks
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
        let (rgba, w, h, has_xor, xor_shape) = result?;

        if has_xor {
            const XOR_PAD: u32 = 4;
            info!("Monochrome XOR cursor detected, rendering image directly ({}x{}, {} XOR pixels)", w, h, xor_shape.count);
            let (mut expanded, ew, eh) = expand_canvas(&rgba, w, h, XOR_PAD);
            add_white_outline(&mut expanded, ew, eh, XOR_PAD as i32);

            let webp_data = encode_static_webp(&expanded, ew, eh)?;
            let cursor_id = format!("cur_{}", &blake3::hash(&expanded).to_hex()[..12]);

            return Ok(CaptureResult::Cursor(CachedCursor {
                id: cursor_id,
                webp_data,
                width: ew,
                height: eh,
                hotspot_x: hotspot_x + XOR_PAD as i32,
                hotspot_y: hotspot_y + XOR_PAD as i32,
                is_animated: false,
                frame_count: 1,
                frame_delay_ms: 0,
            }));
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
    let (first_frame, has_xor, xor_shape) = render_cursor_frame_with_xor_detection(hicon_raw, width, height, 0)?;

    if has_xor {
        info!("Color XOR cursor detected, rendering image directly ({}x{}, {} XOR pixels)", width, height, xor_shape.count);
    }

    // Probe animation frames using the original HCURSOR handle.
    let frames = probe_animation_frames_with_first(hicon_raw, width, height, first_frame)?;

    if frames.len() <= 1 {
        // Static cursor
        let rgba = frames[0].clone();

        if has_xor {
            const XOR_PAD: u32 = 4;
            let (mut expanded, ew, eh) = expand_canvas(&rgba, width, height, XOR_PAD);
            add_white_outline(&mut expanded, ew, eh, XOR_PAD as i32);

            let webp_data = encode_static_webp(&expanded, ew, eh)?;
            let cursor_id = format!("cur_{}", &blake3::hash(&expanded).to_hex()[..12]);

            return Ok(CaptureResult::Cursor(CachedCursor {
                id: cursor_id,
                webp_data,
                width: ew,
                height: eh,
                hotspot_x: hotspot_x + XOR_PAD as i32,
                hotspot_y: hotspot_y + XOR_PAD as i32,
                is_animated: false,
                frame_count: 1,
                frame_delay_ms: 0,
            }));
        }

        let webp_data = encode_static_webp(&rgba, width, height)?;
        let cursor_id = format!("cur_{}", &blake3::hash(&rgba).to_hex()[..12]);

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
            debug!("Animation cycle detected at step {}: {} unique frames", step, frames.len());
            break;
        } else if frame_hashes.contains(&hash) {
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

    Ok(rgba)
}

/// Render a single cursor frame and detect XOR pixels.
unsafe fn render_cursor_frame_with_xor_detection(
    hicon: HICON,
    width: u32,
    height: u32,
    step: u32,
) -> Result<(Vec<u8>, bool, XorShape)> {
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

    // Compute RGBA with XOR detection and shape tracking
    let pixel_count = (width * height) as usize;
    let mut rgba = vec![0u8; pixel_count * 4];
    let mut has_xor = false;
    let mut xor_shape = XorShape::new();

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
            let px = (i % width as usize) as u32;
            let py = (i / width as usize) as u32;
            xor_shape.add_pixel(px, py);
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

    Ok((rgba, has_xor, xor_shape))
}

/// Get RGBA pixels from a monochrome cursor mask bitmap.
unsafe fn get_monochrome_cursor_rgba(
    hmask: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<(Vec<u8>, u32, u32, bool, XorShape)> {
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
    let mut xor_shape = XorShape::new();

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
                xor_shape.add_pixel(x as u32, y as u32);
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

    Ok((rgba, width, height, has_xor, xor_shape))
}
