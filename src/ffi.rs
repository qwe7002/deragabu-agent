//! C FFI interface for embedding deragabu-agent into Sunshine (or any C/C++ host).
//!
//! Exposes four `extern "C"` functions:
//! - `deragabu_agent_init`  — start the agent (tokio runtime + all subsystems)
//! - `deragabu_agent_shutdown` — stop the agent and tear down the runtime
//! - `deragabu_agent_set_display_cursor` — push Sunshine's `display_cursor` state
//! - `deragabu_agent_is_running` — health check

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tracing::info;

/// Whether the agent is currently running.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The tokio [`Runtime`] that owns all agent tasks.
/// Wrapped in a `Mutex<Option<..>>` so we can take it on shutdown.
static RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);

/// Start the agent.
///
/// `bind_addr` is the address for the WebRTC signaling HTTP server,
/// e.g. `"0.0.0.0:9000"`.  Pass `NULL` to use the default `"0.0.0.0:9000"`.
///
/// Returns `0` on success, `-1` on failure.
///
/// # Safety
/// `bind_addr` must be a valid null-terminated C string or `NULL`.
#[no_mangle]
pub extern "C" fn deragabu_agent_init(bind_addr: *const c_char) -> i32 {
    // Prevent double-init
    if RUNNING.load(Ordering::SeqCst) {
        return 0;
    }

    let addr = if bind_addr.is_null() {
        "0.0.0.0:9000".to_string()
    } else {
        match unsafe { CStr::from_ptr(bind_addr) }.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => "0.0.0.0:9000".to_string(),
        }
    };

    // Initialize tracing (ignore error if already initialised by the host)
    let _ = tracing_subscriber::fmt::try_init();

    // Build a multi-thread tokio runtime
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("deragabu_agent_init: failed to create tokio runtime: {e}");
            return -1;
        }
    };

    // Spawn the main subsystem future on the runtime
    rt.spawn(crate::start_all_subsystems(addr));

    // Store the runtime so it stays alive (and can be dropped on shutdown)
    if let Ok(mut guard) = RUNTIME.lock() {
        *guard = Some(rt);
    }

    RUNNING.store(true, Ordering::SeqCst);
    info!("deragabu_agent_init: agent started");
    0
}

/// Stop the agent and release all resources.
///
/// This blocks until the tokio runtime is fully shut down.
#[no_mangle]
pub extern "C" fn deragabu_agent_shutdown() {
    if !RUNNING.load(Ordering::SeqCst) {
        return;
    }

    RUNNING.store(false, Ordering::SeqCst);

    // Take the runtime out and drop it — this cancels all spawned tasks and
    // waits for blocking threads to finish.
    if let Ok(mut guard) = RUNTIME.lock() {
        if let Some(rt) = guard.take() {
            info!("deragabu_agent_shutdown: shutting down tokio runtime");
            rt.shutdown_background();
        }
    }
}

/// Push Sunshine's `display_cursor` state into the agent.
///
/// When `display` is `true`, the video stream contains the hardware cursor and
/// the agent tells clients to hide the overlay.  When `false`, the video stream
/// has no cursor and the agent tells clients to show the overlay.
///
/// This is safe to call from any thread at any time.
#[no_mangle]
pub extern "C" fn deragabu_agent_set_display_cursor(display: bool) {
    crate::sunshine_monitor::set_display_cursor_from_ffi(display);
}

/// Return `true` if the agent is running.
#[no_mangle]
pub extern "C" fn deragabu_agent_is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}
