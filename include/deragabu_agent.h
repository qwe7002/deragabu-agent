/**
 * @file deragabu_agent.h
 * @brief C FFI interface for the Deragabu Agent (Rust static library).
 *
 * Link against `libderagabu_agent.a` (macOS/Linux) or `deragabu_agent.lib` (Windows).
 *
 * The agent provides:
 *   - System cursor capture at ~60 FPS (WebP encoded)
 *   - WebRTC data-channel streaming of cursor overlay + clipboard sync
 *   - Sunshine `display_cursor` state forwarding to connected clients
 *
 * Typical usage from Sunshine (macOS):
 *
 *   #include "deragabu_agent.h"
 *
 *   // On startup:
 *   deragabu_agent_init("0.0.0.0:9000");
 *
 *   // When Moonlight client toggles cursor visibility:
 *   deragabu_agent_set_display_cursor(input::display_cursor);
 *
 *   // On shutdown:
 *   deragabu_agent_shutdown();
 */

#pragma once

#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Start the agent (cursor capture + WebRTC server + clipboard sync).
 *
 * Creates a new tokio multi-thread runtime internally.  All agent tasks
 * run on that runtime and do not block the calling thread.
 *
 * @param bind_addr  WebRTC signaling HTTP server bind address,
 *                   e.g. "0.0.0.0:9000".  Pass NULL to use the default.
 * @return 0 on success, -1 on failure.
 */
int deragabu_agent_init(const char *bind_addr);

/**
 * Stop the agent and release all resources.
 *
 * Cancels all spawned tasks and shuts down the tokio runtime.
 * Safe to call even if the agent was never started (no-op).
 */
void deragabu_agent_shutdown(void);

/**
 * Push the current `display_cursor` state from Sunshine into the agent.
 *
 * @param display  true  — Sunshine draws the cursor in the video stream;
 *                          the agent tells clients to hide the overlay.
 *                 false — Sunshine does NOT draw the cursor;
 *                          the agent tells clients to show the overlay.
 *
 * This is thread-safe and lock-free.  The change is picked up by the
 * sunshine-monitor task within ~100 ms and broadcast to all connected
 * WebRTC clients as a `SettingsData { draw_cursor }` protobuf message.
 */
void deragabu_agent_set_display_cursor(bool display);

/**
 * Check whether the agent is currently running.
 *
 * @return true if `deragabu_agent_init` succeeded and `deragabu_agent_shutdown`
 *         has not yet been called.
 */
bool deragabu_agent_is_running(void);

#ifdef __cplusplus
}
#endif
