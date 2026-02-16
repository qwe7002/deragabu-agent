use anyhow::{anyhow, Result};
use std::os::raw::c_int;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use super::{
    CachedCursor, CursorEvent, LAST_CURSOR_ID,
    cache_cursor, encode_static_webp, init_cache,
};

// ─── CoreGraphics type definitions ──────────────────────────────────────────

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// ─── CoreGraphics framework bindings ────────────────────────────────────────
//
// CGS* functions are private CoreGraphics Server APIs, widely used by
// screen‑capture / VNC tools on macOS. They reside in the CoreGraphics
// framework binary but are not declared in public headers.
//
// Screen Recording permission (System Settings → Privacy & Security) is
// required on macOS 10.15+.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSMainConnectionID() -> c_int;
    fn CGSCurrentCursorSeed() -> c_int;
    fn CGSGetGlobalCursorDataSize(connection: c_int, size: *mut c_int) -> c_int;
    fn CGSGetGlobalCursorData(
        connection: c_int,
        data: *mut u8,
        data_size: *mut c_int,
        row_bytes: *mut c_int,
        rect: *mut CGRect,
        hotspot: *mut CGPoint,
        depth: *mut c_int,
        components: *mut c_int,
        bits_per_component: *mut c_int,
    ) -> c_int;
    fn CGCursorIsVisible() -> bool;

    // Public display-mode APIs for DPI detection
    fn CGMainDisplayID() -> u32;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut std::ffi::c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const std::ffi::c_void) -> usize;
    fn CGDisplayModeGetWidth(mode: *const std::ffi::c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut std::ffi::c_void);
}

// ─── Platform state ─────────────────────────────────────────────────────────

/// Last cursor seed for detecting changes
static LAST_CURSOR_SEED: Mutex<c_int> = Mutex::new(-1);

// ─── Public API ─────────────────────────────────────────────────────────────

/// Get system DPI scale factor (Retina = 2.0, non-Retina = 1.0).
pub fn get_dpi_scale() -> f32 {
    unsafe {
        let display = CGMainDisplayID();
        let mode = CGDisplayCopyDisplayMode(display);
        if !mode.is_null() {
            let pixel_width = CGDisplayModeGetPixelWidth(mode);
            let point_width = CGDisplayModeGetWidth(mode);
            CGDisplayModeRelease(mode);
            if point_width > 0 {
                return pixel_width as f32 / point_width as f32;
            }
        }
    }
    1.0
}

/// Run cursor capture loop (macOS implementation).
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorEvent>) -> Result<()> {
    init_cache();

    let dpi_scale = get_dpi_scale();
    info!("Starting cursor capture on macOS (DPI scale: {:.2})", dpi_scale);

    // Verify CGS connection
    let conn = unsafe { CGSMainConnectionID() };
    if conn == 0 {
        return Err(anyhow!(
            "Failed to get CGS connection. Screen Recording permission may be required."
        ));
    }
    info!("CGS connection established (id: {})", conn);

    let mut poll_interval = interval(Duration::from_millis(16)); // ~60 fps

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

// ─── Internal helpers ───────────────────────────────────────────────────────

/// Capture current cursor and return event if changed.
fn capture_cursor() -> Result<Option<CursorEvent>> {
    unsafe {
        // Check cursor visibility (deprecated since 10.9 but still functional)
        if !CGCursorIsVisible() {
            let mut last_id = LAST_CURSOR_ID.lock().unwrap();
            if last_id.is_some() {
                *last_id = None;
                *LAST_CURSOR_SEED.lock().unwrap() = -1;
                debug!("Cursor hidden");
                return Ok(Some(CursorEvent::CursorHidden));
            }
            return Ok(None);
        }

        // Check cursor seed for changes
        let seed = CGSCurrentCursorSeed();
        {
            let mut last_seed = LAST_CURSOR_SEED.lock().unwrap();
            if seed == *last_seed {
                return Ok(None);
            }
            *last_seed = seed;
        }

        let connection = CGSMainConnectionID();

        // Step 1 – query required buffer size
        let mut data_size: c_int = 0;
        let err = CGSGetGlobalCursorDataSize(connection, &mut data_size);
        if err != 0 || data_size <= 0 {
            return Err(anyhow!(
                "CGSGetGlobalCursorDataSize failed (error={}, size={})",
                err,
                data_size
            ));
        }

        // Step 2 – allocate buffer and fetch cursor pixel data
        let mut data = vec![0u8; data_size as usize];
        let mut actual_size = data_size;
        let mut row_bytes: c_int = 0;
        let mut rect = CGRect::default();
        let mut hotspot = CGPoint::default();
        let mut depth: c_int = 0;
        let mut components: c_int = 0;
        let mut bits_per_component: c_int = 0;

        let err = CGSGetGlobalCursorData(
            connection,
            data.as_mut_ptr(),
            &mut actual_size,
            &mut row_bytes,
            &mut rect,
            &mut hotspot,
            &mut depth,
            &mut components,
            &mut bits_per_component,
        );

        if err != 0 {
            return Err(anyhow!("CGSGetGlobalCursorData failed (error={})", err));
        }

        let width = rect.size.width as u32;
        let height = rect.size.height as u32;

        if width == 0 || height == 0 {
            return Err(anyhow!("Cursor has zero dimensions ({}x{})", width, height));
        }

        let hotspot_x = hotspot.x as i32;
        let hotspot_y = hotspot.y as i32;
        let bytes_per_pixel = ((components * bits_per_component + 7) / 8) as usize;

        debug!(
            "macOS cursor: {}x{}, depth={}, comp={}, bpc={}, row_bytes={}, bpp={}, hotspot=({},{})",
            width, height, depth, components, bits_per_component,
            row_bytes, bytes_per_pixel, hotspot_x, hotspot_y
        );

        // Step 3 – convert from premultiplied ARGB (BGRA in LE memory) → straight RGBA
        let mut rgba = vec![0u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let src = (y as usize) * (row_bytes as usize) + (x as usize) * bytes_per_pixel;
                let dst = (y * width + x) as usize * 4;

                if src + bytes_per_pixel > data.len() {
                    continue;
                }

                if bytes_per_pixel >= 4 {
                    // macOS CoreGraphics stores premultiplied ARGB as 0xAARRGGBB.
                    // On little-endian (Intel / Apple Silicon) the memory layout is:
                    //   byte 0 = B, byte 1 = G, byte 2 = R, byte 3 = A
                    let b = data[src] as u16;
                    let g = data[src + 1] as u16;
                    let r = data[src + 2] as u16;
                    let a = data[src + 3];

                    // Un-premultiply
                    let (r, g, b) = if a > 0 && a < 255 {
                        let af = a as u16;
                        (
                            ((r * 255 + af / 2) / af).min(255) as u8,
                            ((g * 255 + af / 2) / af).min(255) as u8,
                            ((b * 255 + af / 2) / af).min(255) as u8,
                        )
                    } else {
                        (r as u8, g as u8, b as u8)
                    };

                    rgba[dst] = r;
                    rgba[dst + 1] = g;
                    rgba[dst + 2] = b;
                    rgba[dst + 3] = a;
                }
            }
        }

        // Step 4 – hash, encode, cache
        let cursor_id = format!("cur_{}", &blake3::hash(&rgba).to_hex()[..12]);
        let webp_data = encode_static_webp(&rgba, width, height)?;

        let cached = CachedCursor {
            id: cursor_id,
            webp_data,
            width,
            height,
            hotspot_x,
            hotspot_y,
            is_animated: false,
            frame_count: 1,
            frame_delay_ms: 0,
        };

        let (cursor_id, _) = cache_cursor(cached);
        Ok(Some(CursorEvent::CursorChanged(cursor_id)))
    }
}
