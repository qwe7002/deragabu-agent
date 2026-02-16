use anyhow::{anyhow, Result};
use image::{Rgba, RgbaImage};
use std::collections::HashMap;
use std::io::Cursor;
use std::mem;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, SelectObject, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HCURSOR,
    ICONINFO,
};

use crate::cursor::{CursorMessage, MessageType};

/// Image encoding format
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageFormat {
    Png,
    WebP,
}

impl ImageFormat {
    /// Get format from environment variable
    pub fn from_env() -> Self {
        match std::env::var("IMAGE_FORMAT").as_deref() {
            Ok("webp") | Ok("WEBP") => ImageFormat::WebP,
            Ok("png") | Ok("PNG") => ImageFormat::Png,
            _ => ImageFormat::WebP, // Default to WebP for better compression
        }
    }

    /// Convert to protobuf enum value
    fn to_proto_enum(&self) -> i32 {
        match self {
            ImageFormat::Png => 0,  // IMAGE_FORMAT_PNG
            ImageFormat::WebP => 1, // IMAGE_FORMAT_WEBP
        }
    }
}

/// Cursor cache entry
#[derive(Clone)]
struct CachedCursor {
    id: String,
    image_data: Vec<u8>,
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    format: ImageFormat,
}

/// Global cursor cache
static CURSOR_CACHE: Mutex<Option<HashMap<isize, CachedCursor>>> = Mutex::new(None);

/// Last cursor handle for detecting cursor changes
static mut LAST_CURSOR_HANDLE: isize = 0;
/// Frame counter for forcing updates (for animated cursors)
static mut FRAME_COUNTER: u32 = 0;
/// Force update every N frames (approximately every 500ms at 60fps)
const FORCE_UPDATE_INTERVAL: u32 = 15;

/// Run cursor capture loop
pub async fn run_cursor_capture(tx: mpsc::Sender<CursorMessage>) -> Result<()> {
    // Initialize cache (within a scope to ensure lock is released)
    {
        let mut cache = CURSOR_CACHE.lock().unwrap();
        if cache.is_none() {
            *cache = Some(HashMap::new());
            info!("Cursor cache initialized");
        }
    } // Lock released here

    let image_format = ImageFormat::from_env();
    info!("Starting cursor capture... (format: {:?})", image_format);

    let mut poll_interval = interval(Duration::from_millis(16)); // ~60fps

    loop {
        poll_interval.tick().await;

        match capture_cursor(image_format) {
            Ok(Some(message)) => {
                if tx.send(message).await.is_err() {
                    warn!("Receiver closed, stopping cursor capture");
                    break;
                }
            }
            Ok(None) => {
                // Cursor unchanged, do not send
            }
            Err(e) => {
                debug!("Failed to capture cursor: {}", e);
            }
        }
    }

    Ok(())
}

/// Capture current cursor and convert to message
fn capture_cursor(image_format: ImageFormat) -> Result<Option<CursorMessage>> {
    unsafe {
        // Get cursor information
        let mut cursor_info = CURSORINFO {
            cbSize: mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };

        if GetCursorInfo(&mut cursor_info).is_err() {
            return Err(anyhow!("GetCursorInfo failed"));
        }

        // Check if cursor is visible
        if cursor_info.flags.0 & CURSOR_SHOWING.0 == 0 {
            // Cursor hidden, send hide message
            if LAST_CURSOR_HANDLE != 0 {
                LAST_CURSOR_HANDLE = 0;
                FRAME_COUNTER = 0;
                return Ok(Some(CursorMessage {
                    r#type: MessageType::CursorHide.into(),
                    image_data: vec![],
                    hotspot_x: 0,
                    hotspot_y: 0,
                    width: 0,
                    height: 0,
                    timestamp: get_timestamp(),
                    image_format: 0, // Not applicable for hide message
                    cursor_id: String::new(),
                    is_full_update: false,
                }));
            }
            return Ok(None);
        }

        let hcursor = cursor_info.hCursor;
        let cursor_handle = hcursor.0 as isize;

        // Check if cursor handle has changed
        let cursor_changed = cursor_handle != LAST_CURSOR_HANDLE;

        if cursor_changed {
            // Cursor changed - reset counter and update
            FRAME_COUNTER = 0;
            LAST_CURSOR_HANDLE = cursor_handle;
            // Will send full update below
        } else {
            // Same cursor - check if we can use cache
            FRAME_COUNTER += 1;

            // Check cache first (within a scope to release lock quickly)
            let cached_data = {
                let cache_guard = CURSOR_CACHE.lock().unwrap();
                let cache = cache_guard.as_ref().unwrap();

                // Check if we have this cursor cached
                if let Some(cached) = cache.get(&cursor_handle) {
                    // Send cache reference unless it's time for forced update
                    if FRAME_COUNTER < FORCE_UPDATE_INTERVAL {
                        // Clone the data we need before releasing the lock
                        Some((
                            cached.id.clone(),
                            cached.hotspot_x,
                            cached.hotspot_y,
                            cached.width,
                            cached.height,
                            cached.format,
                        ))
                    } else {
                        // Time for forced update (for animated cursors)
                        None
                    }
                } else {
                    None
                }
            }; // Lock released here

            // If we have cached data, send cache reference
            if let Some((cursor_id, hotspot_x, hotspot_y, width, height, format)) = cached_data {
                debug!("Cache HIT: cursor_handle={}, frame={}", cursor_handle, FRAME_COUNTER);
                return Ok(Some(CursorMessage {
                    r#type: MessageType::CursorUpdate.into(),
                    image_data: vec![], // Empty - use cached
                    hotspot_x,
                    hotspot_y,
                    width: width as i32,
                    height: height as i32,
                    timestamp: get_timestamp(),
                    image_format: format.to_proto_enum(),
                    cursor_id,
                    is_full_update: false,
                }));
            }

            // If we reach here, either not cached or time for forced update
            // Reset counter for next cycle
            FRAME_COUNTER = 0;
            debug!("Cache MISS or forced update: cursor_handle={}, frame={}", cursor_handle, FRAME_COUNTER);
        }

        // Get cursor image
        let (image, hotspot_x, hotspot_y) = get_cursor_image(hcursor)?;

        // Encode image based on format
        let image_data = match image_format {
            ImageFormat::Png => encode_png(&image)?,
            ImageFormat::WebP => encode_webp(&image)?,
        };

        // Calculate cursor ID (hash of image data)
        let cursor_id = calculate_cursor_id(&image_data);

        // Cache this cursor (within a scope to release lock quickly)
        {
            let cached_cursor = CachedCursor {
                id: cursor_id.clone(),
                image_data: image_data.clone(),
                width: image.width(),
                height: image.height(),
                hotspot_x,
                hotspot_y,
                format: image_format,
            };

            let mut cache_guard = CURSOR_CACHE.lock().unwrap();
            let cache = cache_guard.as_mut().unwrap();
            let is_new = !cache.contains_key(&cursor_handle);
            cache.insert(cursor_handle, cached_cursor);

            if is_new {
                debug!("Cached NEW cursor: handle={}, id={}, total={}",
                    cursor_handle, &cursor_id[..8], cache.len());
            } else {
                debug!("Updated cached cursor: handle={}, id={}",
                    cursor_handle, &cursor_id[..8]);
            }

            // Limit cache size to prevent memory leaks
            if cache.len() > 50 {
                // Remove oldest entries (simple strategy: clear half)
                let keys: Vec<_> = cache.keys().copied().collect();
                for key in keys.iter().take(25) {
                    cache.remove(key);
                }
                debug!("Cache trimmed to {} entries", cache.len());
            }
        } // Lock released here

        Ok(Some(CursorMessage {
            r#type: MessageType::CursorUpdate.into(),
            image_data,
            hotspot_x,
            hotspot_y,
            width: image.width() as i32,
            height: image.height() as i32,
            timestamp: get_timestamp(),
            image_format: image_format.to_proto_enum(),
            cursor_id,
            is_full_update: true,
        }))
    }
}

/// Get image data from HCURSOR
unsafe fn get_cursor_image(hcursor: HCURSOR) -> Result<(RgbaImage, i32, i32)> {
    // Copy icon to get a workable copy
    let hicon = CopyIcon(hcursor)?;

    // Get icon information
    let mut icon_info = ICONINFO::default();
    if GetIconInfo(hicon, &mut icon_info).is_err() {
        DestroyIcon(hicon)?;
        return Err(anyhow!("GetIconInfo failed"));
    }

    let hotspot_x = icon_info.xHotspot as i32;
    let hotspot_y = icon_info.yHotspot as i32;

    // Get bitmap information
    let hbm_color = icon_info.hbmColor;
    let hbm_mask = icon_info.hbmMask;

    let result = if !hbm_color.is_invalid() {
        // Color cursor
        get_bitmap_image(hbm_color, Some(hbm_mask))
    } else {
        // Monochrome cursor
        get_monochrome_cursor_image(hbm_mask)
    };

    // Clean up resources
    if !hbm_color.is_invalid() {
        let _ = DeleteObject(hbm_color);
    }
    if !hbm_mask.is_invalid() {
        let _ = DeleteObject(hbm_mask);
    }
    DestroyIcon(hicon)?;

    let image = result?;
    Ok((image, hotspot_x, hotspot_y))
}

/// Get image from color bitmap
unsafe fn get_bitmap_image(hbm_color: HBITMAP, hbm_mask: Option<HBITMAP>) -> Result<RgbaImage> {
    unsafe {
        // Get bitmap dimensions
        let mut bitmap = BITMAP::default();
        let size = mem::size_of::<BITMAP>() as i32;
        if GetObjectW(hbm_color, size, Some(&mut bitmap as *mut BITMAP as *mut _)) == 0 {
            return Err(anyhow!("GetObjectW failed"));
        }

        let width = bitmap.bmWidth as u32;
        let height = bitmap.bmHeight as u32;

        if width == 0 || height == 0 {
            return Err(anyhow!("Invalid bitmap dimensions"));
        }

        // Create DC
        let hdc_screen = CreateCompatibleDC(None);
        if hdc_screen.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }

        // Prepare BITMAPINFO
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // Negative value means top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            ..Default::default()
        };

        // Get color data
        let mut color_bits: Vec<u8> = vec![0u8; (width * height * 4) as usize];
        let old_bm = SelectObject(hdc_screen, hbm_color);

        if GetDIBits(
            hdc_screen,
            hbm_color,
            0,
            height,
            Some(color_bits.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        ) == 0
        {
            SelectObject(hdc_screen, old_bm);
            let _ = DeleteDC(hdc_screen);
            return Err(anyhow!("GetDIBits (color) failed"));
        }

        // Get mask data (if available)
        let mut mask_bits: Option<Vec<u8>> = None;
        if let Some(hbm_mask) = hbm_mask {
            if !hbm_mask.is_invalid() {
                let mut mask_data: Vec<u8> = vec![0u8; (width * height * 4) as usize];
                SelectObject(hdc_screen, hbm_mask);
                if GetDIBits(
                    hdc_screen,
                    hbm_mask,
                    0,
                    height,
                    Some(mask_data.as_mut_ptr() as *mut _),
                    &mut bmi,
                    DIB_RGB_COLORS,
                ) != 0
                {
                    mask_bits = Some(mask_data);
                }
            }
        }

        SelectObject(hdc_screen, old_bm);
        let _ = DeleteDC(hdc_screen);

        // Create RGBA image
        let mut image = RgbaImage::new(width, height);

        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;

                let b = color_bits[idx];
                let g = color_bits[idx + 1];
                let r = color_bits[idx + 2];
                let mut a = color_bits[idx + 3];

                // If mask exists, use it to determine transparency
                if let Some(ref mask) = mask_bits {
                    let mask_val = mask[idx]; // Any channel of the mask
                    if mask_val != 0 {
                        // White in mask means transparent
                        if a == 0 {
                            a = 0; // Keep transparent
                        }
                    } else {
                        // Black in mask means opaque
                        if a == 0 {
                            a = 255;
                        }
                    }
                } else if a == 0 && (r != 0 || g != 0 || b != 0) {
                    // If no alpha but has color data, make it opaque
                    a = 255;
                }

                image.put_pixel(x, y, Rgba([r, g, b, a]));
            }
        }

        Ok(image)
    }
}

/// Get cursor image from monochrome bitmap
unsafe fn get_monochrome_cursor_image(hbm_mask: HBITMAP) -> Result<RgbaImage> {
    unsafe {
        let mut bitmap = BITMAP::default();
        let size = mem::size_of::<BITMAP>() as i32;
        if GetObjectW(hbm_mask, size, Some(&mut bitmap as *mut BITMAP as *mut _)) == 0 {
            return Err(anyhow!("GetObjectW (mask) failed"));
        }

        let width = bitmap.bmWidth as u32;
        // Monochrome cursor bitmap height is twice the actual height (AND mask + XOR mask)
        let height = (bitmap.bmHeight / 2) as u32;

        if width == 0 || height == 0 {
            return Err(anyhow!("Invalid monochrome bitmap dimensions"));
        }

        let hdc_screen = CreateCompatibleDC(None);
        if hdc_screen.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -((height * 2) as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: Vec<u8> = vec![0u8; (width * height * 2 * 4) as usize];

        if GetDIBits(
            hdc_screen,
            hbm_mask,
            0,
            height * 2,
            Some(bits.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        ) == 0
        {
            let _ = DeleteDC(hdc_screen);
            return Err(anyhow!("GetDIBits (monochrome) failed"));
        }

        let _ = DeleteDC(hdc_screen);

        // Create image
        let mut image = RgbaImage::new(width, height);
        let pixel_count = (width * height) as usize;

        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                let and_mask_idx = idx;
                let xor_mask_idx = idx + pixel_count * 4;

                let and_val = bits[and_mask_idx]; // AND mask
                let xor_val = bits[xor_mask_idx]; // XOR mask

                let (r, g, b, a) = if and_val != 0 && xor_val != 0 {
                    // Invert screen pixels - use semi-transparent
                    (128, 128, 128, 128)
                } else if and_val != 0 && xor_val == 0 {
                    // Transparent
                    (0, 0, 0, 0)
                } else if and_val == 0 && xor_val != 0 {
                    // White
                    (255, 255, 255, 255)
                } else {
                    // Black
                    (0, 0, 0, 255)
                };

                image.put_pixel(x, y, Rgba([r, g, b, a]));
            }
        }

        Ok(image)
    }
}

/// Encode image as PNG
fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut cursor = Cursor::new(&mut buffer);

    image
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| anyhow!("PNG encoding failed: {}", e))?;

    Ok(buffer)
}

/// Encode image as WebP with quality control
fn encode_webp(image: &RgbaImage) -> Result<Vec<u8>> {
    // Get quality setting from environment (default: 80, range: 0-100)
    let quality = std::env::var("WEBP_QUALITY")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(80.0)
        .clamp(0.0, 100.0);

    // Convert image to raw RGBA data
    let width = image.width();
    let height = image.height();
    let data = image.as_raw();

    // Create WebP encoder
    let encoder = webp::Encoder::from_rgba(data, width, height);

    // Encode with specified quality
    // quality > 0 uses lossy compression, quality = 0 uses lossless
    let webp_data = if quality > 0.0 {
        encoder.encode(quality)
    } else {
        encoder.encode_lossless()
    };

    Ok(webp_data.to_vec())
}

/// Get current timestamp (milliseconds)
fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Calculate cursor ID from image data (using BLAKE3 hash)
fn calculate_cursor_id(data: &[u8]) -> String {
    let hash = blake3::hash(data);
    // Use first 16 characters of hex hash for cursor ID
    format!("{:.16}", hash.to_hex())
}

