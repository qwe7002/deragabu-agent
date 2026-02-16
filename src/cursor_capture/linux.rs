use anyhow::{anyhow, Result};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as XFixesConnectionExt};
use x11rb::rust_connection::RustConnection;

use super::{
    CachedCursor, CursorEvent, LAST_CURSOR_ID,
    cache_cursor, encode_static_webp, init_cache,
};

// ─── Platform state ─────────────────────────────────────────────────────────

/// Last cursor serial for detecting changes
static LAST_CURSOR_SERIAL: Mutex<u32> = Mutex::new(0);

// ─── Public API ─────────────────────────────────────────────────────────────

/// Get system DPI scale factor from X11 screen dimensions.
///
/// Computes DPI from physical screen size (mm) reported by X11. Falls back to
/// 1.0 when the X connection is unavailable or the reported size is zero.
pub fn get_dpi_scale() -> f32 {
    if let Ok((conn, screen_num)) = x11rb::connect(None) {
        let setup = conn.setup();
        if let Some(screen) = setup.roots.get(screen_num) {
            let width_px = screen.width_in_pixels as f32;
            let width_mm = screen.width_in_millimeters as f32;
            if width_mm > 0.0 {
                let dpi = (width_px * 25.4) / width_mm;
                return dpi / 96.0; // 96 DPI = scale 1.0
            }
        }
    }
    1.0
}

/// Run cursor capture loop (Linux / X11 implementation).
///
/// Requires the XFixes extension (version ≥ 2). On systems using pure Wayland
/// without XWayland this will fail at connection time.
///
/// System packages needed for building:
///   Debian/Ubuntu: `libxcb1-dev libxcb-xfixes0-dev`
///   Fedora/RHEL:   `libxcb-devel`
///   Arch:          `libxcb` (usually installed by default)
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorEvent>) -> Result<()> {
    init_cache();

    let dpi_scale = get_dpi_scale();
    info!("Starting cursor capture on Linux/X11 (DPI scale: {:.2})", dpi_scale);

    // Connect to X11
    let (conn, _screen_num) = x11rb::connect(None)
        .map_err(|e| anyhow!(
            "Failed to connect to X11 display: {}. \
             Make sure $DISPLAY is set. Pure Wayland (without XWayland) is not supported.",
            e
        ))?;

    // Initialise XFixes extension
    let xfixes_ver = conn
        .xfixes_query_version(6, 0)?
        .reply()
        .map_err(|e| anyhow!("XFixes query version failed: {}", e))?;

    info!(
        "XFixes version: {}.{}",
        xfixes_ver.major_version, xfixes_ver.minor_version
    );

    if xfixes_ver.major_version < 2 {
        return Err(anyhow!(
            "XFixes version 2+ required for cursor image capture (have {}.{})",
            xfixes_ver.major_version,
            xfixes_ver.minor_version
        ));
    }

    let mut poll_interval = interval(Duration::from_millis(16)); // ~60 fps

    loop {
        poll_interval.tick().await;

        match capture_cursor(&conn) {
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
fn capture_cursor(conn: &RustConnection) -> Result<Option<CursorEvent>> {
    // XFixesGetCursorImage returns the current cursor image + metadata
    let reply = conn
        .xfixes_get_cursor_image()
        .map_err(|e| anyhow!("XFixesGetCursorImage request failed: {}", e))?
        .reply()
        .map_err(|e| anyhow!("XFixesGetCursorImage reply failed: {}", e))?;

    let serial = reply.cursor_serial;

    // Quick-check: same cursor?
    {
        let mut last = LAST_CURSOR_SERIAL.lock().unwrap();
        if serial == *last {
            return Ok(None);
        }
        *last = serial;
    }

    let width = reply.width as u32;
    let height = reply.height as u32;
    let hotspot_x = reply.xhot as i32;
    let hotspot_y = reply.yhot as i32;

    if width == 0 || height == 0 {
        return Err(anyhow!("Cursor has zero dimensions"));
    }

    // Convert ARGB u32 pixels → straight RGBA u8 array
    let pixels = &reply.cursor_image;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let mut all_transparent = true;

    for (i, &pixel) in pixels.iter().enumerate() {
        if i >= (width * height) as usize {
            break;
        }
        // XFixes pixels are native-endian u32 in ARGB layout
        let a = ((pixel >> 24) & 0xFF) as u8;
        let r = ((pixel >> 16) & 0xFF) as u16;
        let g = ((pixel >> 8) & 0xFF) as u16;
        let b = (pixel & 0xFF) as u16;

        if a > 0 {
            all_transparent = false;
        }

        // Un-premultiply alpha (X11 cursor images are premultiplied)
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

        rgba[i * 4] = r;
        rgba[i * 4 + 1] = g;
        rgba[i * 4 + 2] = b;
        rgba[i * 4 + 3] = a;
    }

    // Detect "invisible" cursor (all pixels transparent → cursor hidden)
    if all_transparent {
        let mut last_id = LAST_CURSOR_ID.lock().unwrap();
        if last_id.is_some() {
            *last_id = None;
            debug!("Cursor appears hidden (fully transparent)");
            return Ok(Some(CursorEvent::CursorHidden));
        }
        return Ok(None);
    }

    // Hash → cache → event
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
