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

// ─── Objective-C runtime & AppKit bindings (Retina cursor images) ───────────

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
    fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
}

// Typed function-pointer aliases for objc_msgSend.
// Casting to a concrete signature ensures correct register usage for
// pointer / integer / floating-point returns on both arm64 and x86_64.
#[allow(clashing_extern_declarations)]
extern "C" {
    #[link_name = "objc_msgSend"]
    fn msg_send_id(obj: *mut std::ffi::c_void, sel: *mut std::ffi::c_void) -> *mut std::ffi::c_void;

    #[link_name = "objc_msgSend"]
    fn msg_send_id_ptr3(
        obj: *mut std::ffi::c_void,
        sel: *mut std::ffi::c_void,
        a1: *mut std::ffi::c_void,
        a2: *mut std::ffi::c_void,
        a3: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;

    /// Returns NSPoint / NSSize ({f64, f64}) – fits in two FP registers.
    #[link_name = "objc_msgSend"]
    fn msg_send_point(
        obj: *mut std::ffi::c_void,
        sel: *mut std::ffi::c_void,
    ) -> CGPoint;
}

// Additional CoreGraphics APIs for rendering a CGImage into a bitmap.
extern "C" {
    fn CGImageGetWidth(image: *const std::ffi::c_void) -> usize;
    fn CGImageGetHeight(image: *const std::ffi::c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void;
    fn CGColorSpaceRelease(space: *mut std::ffi::c_void);
    fn CGBitmapContextCreate(
        data: *mut u8,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut std::ffi::c_void,
        bitmap_info: u32,
    ) -> *mut std::ffi::c_void;
    fn CGContextDrawImage(
        ctx: *mut std::ffi::c_void,
        rect: CGRect,
        image: *const std::ffi::c_void,
    );
    fn CGContextRelease(ctx: *mut std::ffi::c_void);
}

// kCGImageAlphaPremultipliedLast = RGBA with premultiplied alpha, 8 bpc
const BITMAP_INFO_RGBA_PREMUL: u32 = 1; // kCGImageAlphaPremultipliedLast

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

/// Try to obtain the current cursor image at native Retina resolution
/// via the Cocoa NSCursor API.  Returns (rgba, w, h, hotspot_x, hotspot_y)
/// with straight (un-premultiplied) alpha, or `None` on failure.
fn try_get_nscursor_rgba() -> Option<(Vec<u8>, u32, u32, i32, i32)> {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let result = try_get_nscursor_rgba_inner();
        objc_autoreleasePoolPop(pool);
        result
    }
}

unsafe fn try_get_nscursor_rgba_inner() -> Option<(Vec<u8>, u32, u32, i32, i32)> {
    // 1. [NSCursor currentSystemCursor]
    let cls = objc_getClass(b"NSCursor\0".as_ptr() as *const _);
    if cls.is_null() { return None; }
    let cursor = msg_send_id(cls, sel_registerName(b"currentSystemCursor\0".as_ptr() as *const _));
    if cursor.is_null() {
        debug!("NSCursor.currentSystemCursor returned nil");
        return None;
    }

    // 2. Hotspot (in points)
    let hotspot: CGPoint = msg_send_point(
        cursor,
        sel_registerName(b"hotSpot\0".as_ptr() as *const _),
    );

    // 3. [cursor image] -> NSImage
    let image = msg_send_id(
        cursor,
        sel_registerName(b"image\0".as_ptr() as *const _),
    );
    if image.is_null() { return None; }

    // 4. Image logical size in points
    //    NSSize has the same binary layout as CGPoint: {f64, f64}
    let ns_size: CGPoint = msg_send_point(
        image,
        sel_registerName(b"size\0".as_ptr() as *const _),
    );
    if ns_size.x <= 0.0 || ns_size.y <= 0.0 { return None; }

    // 5. Request CGImage at the best available resolution.
    //    CGImageForProposedRect takes a rect in *points*.  On a Retina
    //    display NSImage will automatically select the @2× representation
    //    and return a CGImage at native pixel resolution (e.g. 32×32 px
    //    for a 16×16 pt cursor).  We must NOT multiply by DPI here —
    //    that would request a rect twice the logical size and could
    //    cause NSImage to scale the representation up further.
    let mut proposed = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: ns_size.x,
            height: ns_size.y,
        },
    };
    let cgimage = msg_send_id_ptr3(
        image,
        sel_registerName(b"CGImageForProposedRect:context:hints:\0".as_ptr() as *const _),
        &mut proposed as *mut _ as *mut std::ffi::c_void,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if cgimage.is_null() {
        debug!("NSImage CGImageForProposedRect returned nil");
        return None;
    }

    // 6. Pixel dimensions of the CGImage (these are actual pixels, not points)
    let cg_w = CGImageGetWidth(cgimage);
    let cg_h = CGImageGetHeight(cgimage);
    if cg_w == 0 || cg_h == 0 { return None; }

    // 7. Render CGImage into a premultiplied-RGBA bitmap context
    let color_space = CGColorSpaceCreateDeviceRGB();
    if color_space.is_null() { return None; }
    let row_bytes = cg_w * 4;
    let mut rgba = vec![0u8; cg_h * row_bytes];
    let ctx = CGBitmapContextCreate(
        rgba.as_mut_ptr(),
        cg_w,
        cg_h,
        8,
        row_bytes,
        color_space,
        BITMAP_INFO_RGBA_PREMUL,
    );
    CGColorSpaceRelease(color_space);
    if ctx.is_null() { return None; }

    let draw_rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize { width: cg_w as f64, height: cg_h as f64 },
    };
    CGContextDrawImage(ctx, draw_rect, cgimage);
    CGContextRelease(ctx);

    // 8. Un-premultiply alpha
    for i in (0..rgba.len()).step_by(4) {
        let a = rgba[i + 3] as u16;
        if a > 0 && a < 255 {
            rgba[i]     = ((rgba[i]     as u16 * 255 + a / 2) / a).min(255) as u8;
            rgba[i + 1] = ((rgba[i + 1] as u16 * 255 + a / 2) / a).min(255) as u8;
            rgba[i + 2] = ((rgba[i + 2] as u16 * 255 + a / 2) / a).min(255) as u8;
        }
    }

    // 9. Scale hotspot from points → pixels
    let sx = cg_w as f64 / ns_size.x;
    let sy = cg_h as f64 / ns_size.y;
    let hx = (hotspot.x * sx).round() as i32;
    let hy = (hotspot.y * sy).round() as i32;

    debug!(
        "NSCursor image: {}x{} px, logical {:.0}x{:.0} pt, scale {:.1}x, hotspot=({},{})",
        cg_w, cg_h, ns_size.x, ns_size.y, sx, hx, hy
    );
    Some((rgba, cg_w as u32, cg_h as u32, hx, hy))
}

/// Bilinear-interpolation upscale (fallback when NSCursor is unavailable).
fn bilinear_scale(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    let x_ratio = sw as f64 / dw as f64;
    let y_ratio = sh as f64 / dh as f64;
    for y in 0..dh {
        let gy = y as f64 * y_ratio;
        let yi = (gy as u32).min(sh - 1);
        let yi1 = (yi + 1).min(sh - 1);
        let yw = gy - yi as f64;
        for x in 0..dw {
            let gx = x as f64 * x_ratio;
            let xi = (gx as u32).min(sw - 1);
            let xi1 = (xi + 1).min(sw - 1);
            let xw = gx - xi as f64;

            let idx = |ix: u32, iy: u32| (iy * sw + ix) as usize * 4;
            let di = (y * dw + x) as usize * 4;

            for c in 0..4 {
                let c00 = src[idx(xi, yi) + c] as f64;
                let c10 = src[idx(xi1, yi) + c] as f64;
                let c01 = src[idx(xi, yi1) + c] as f64;
                let c11 = src[idx(xi1, yi1) + c] as f64;
                let v = c00 * (1.0 - xw) * (1.0 - yw)
                    + c10 * xw * (1.0 - yw)
                    + c01 * (1.0 - xw) * yw
                    + c11 * xw * yw;
                dst[di + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    dst
}

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

        let bytes_per_pixel = ((components * bits_per_component + 7) / 8) as usize;

        // Derive ACTUAL pixel dimensions from the data buffer layout.
        // On macOS Retina, rect.size may report logical-point dimensions
        // while the pixel data is at the display's backing scale (2x).
        // row_bytes / bpp gives the true pixel width per row.
        let rect_w = rect.size.width as u32;
        let rect_h = rect.size.height as u32;

        let (width, height) = if bytes_per_pixel > 0 && row_bytes > 0 {
            let pixel_w = (row_bytes as u32) / (bytes_per_pixel as u32);
            let pixel_h = if row_bytes > 0 {
                (actual_size as u32) / (row_bytes as u32)
            } else {
                rect_h
            };

            if pixel_w != rect_w || pixel_h != rect_h {
                debug!(
                    "CGS rect {}x{} differs from buffer {}x{} (row_bytes={}, bpp={}, data_size={}). Using buffer dimensions.",
                    rect_w, rect_h, pixel_w, pixel_h,
                    row_bytes, bytes_per_pixel, actual_size
                );
            }
            (pixel_w, pixel_h)
        } else {
            (rect_w, rect_h)
        };

        if width == 0 || height == 0 {
            return Err(anyhow!("Cursor has zero dimensions ({}x{})", width, height));
        }

        let hotspot_x = hotspot.x as i32;
        let hotspot_y = hotspot.y as i32;

        // Scale hotspot if the pixel dimensions differ from rect (logical) dimensions.
        // If rect is in logical points and pixels are 2x, hotspot from CGS is in
        // logical coords and must be scaled up to match the pixel image.
        let (hotspot_x, hotspot_y) = if rect_w > 0 && rect_w != width {
            let sx = width as f64 / rect_w as f64;
            let sy = height as f64 / rect_h.max(1) as f64;
            ((hotspot_x as f64 * sx).round() as i32,
             (hotspot_y as f64 * sy).round() as i32)
        } else {
            (hotspot_x, hotspot_y)
        };

        debug!(
            "macOS cursor: {}x{} px (rect {}x{}), depth={}, comp={}, bpc={}, row_bytes={}, bpp={}, hotspot=({},{}), data_size={}",
            width, height, rect_w, rect_h, depth, components, bits_per_component,
            row_bytes, bytes_per_pixel, hotspot_x, hotspot_y, actual_size
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

        // Step 4 – obtain the best-resolution cursor image.
        //
        // On Retina displays CGS typically returns 1× (point) pixel data.
        // Instead of a crude nearest-neighbour upscale we now:
        //   a) check whether CGS already delivered native-res data,
        //   b) try the Cocoa NSCursor API which provides native @2× images,
        //   c) fall back to bilinear interpolation if NSCursor fails.
        let dpi = get_dpi_scale();
        let scale = dpi.round() as u32; // 1 or 2

        // Did CGS already return data at display resolution?
        let already_native = rect_w > 0 && width > rect_w;

        let (final_rgba, final_w, final_h, final_hx, final_hy) = if scale > 1 && !already_native {
            // CGS data is 1× on a Retina display – try NSCursor for the
            // crisp @2× image that macOS actually renders on screen.
            if let Some((ns_rgba, ns_w, ns_h, ns_hx, ns_hy)) = try_get_nscursor_rgba() {
                debug!(
                    "Using NSCursor high-res image {}x{} instead of CGS {}x{}",
                    ns_w, ns_h, width, height
                );
                (ns_rgba, ns_w, ns_h, ns_hx, ns_hy)
            } else {
                // NSCursor unavailable (custom cursor?) – bilinear upscale.
                let sw = width * scale;
                let sh = height * scale;
                let scaled = bilinear_scale(&rgba, width, height, sw, sh);
                let shx = hotspot_x * scale as i32;
                let shy = hotspot_y * scale as i32;
                debug!(
                    "Bilinear upscaled cursor {}x{} -> {}x{} (DPI scale {})",
                    width, height, sw, sh, scale
                );
                (scaled, sw, sh, shx, shy)
            }
        } else {
            (rgba.clone(), width, height, hotspot_x, hotspot_y)
        };

        let cursor_id = format!("cur_{}", &blake3::hash(&final_rgba).to_hex()[..12]);
        let webp_data = encode_static_webp(&final_rgba, final_w, final_h)?;

        let cached = CachedCursor {
            id: cursor_id,
            webp_data,
            width: final_w,
            height: final_h,
            hotspot_x: final_hx,
            hotspot_y: final_hy,
            is_animated: false,
            frame_count: 1,
            frame_delay_ms: 0,
        };

        let (cursor_id, _) = cache_cursor(cached);
        Ok(Some(CursorEvent::CursorChanged(cursor_id)))
    }
}
