#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════════════
# build-sunshine-mac.sh
#
# Build Sunshine on macOS with deragabu-agent integrated as a static library.
#
# What this script does:
#   1. Install macOS dependencies via Homebrew (if missing)
#   2. Clone Sunshine (or use existing checkout)
#   3. Build deragabu-agent as a static library (libderagabu_agent.a)
#   4. Patch Sunshine source to integrate the agent:
#      - av_video.m: Set capturesCursor=NO at session init (best-effort)
#      - display.mm: Forward *cursor state to agent FFI (overlay control)
#      - main.cpp: Initialize/shutdown agent
#   5. Build Sunshine with CMake + Ninja, linking the agent
#
# NOTE on capturesCursor:
#   AVCaptureScreenInput.capturesCursor is set to NO once at init time.
#   It is NOT toggled dynamically because:
#     - Dynamic changes require beginConfiguration/commitConfiguration (session pause)
#     - Known macOS bug: if accessibility cursor size was changed,
#       capturesCursor=NO is silently ignored and cursor still appears.
#   The agent's soft cursor overlay is the primary cursor mechanism.
#   The *cursor toggle from Moonlight only controls overlay visibility.
#
# Usage:
#   chmod +x build-sunshine-mac.sh
#   ./build-sunshine-mac.sh [--sunshine-dir <path>] [--agent-dir <path>] [--skip-deps]
#
# ═══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

# ─── Configuration ───────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENT_DIR="${AGENT_DIR:-$SCRIPT_DIR}"
SUNSHINE_DIR="${SUNSHINE_DIR:-$SCRIPT_DIR/../Sunshine}"
SKIP_DEPS=false
AGENT_BIND_ADDR="0.0.0.0:9000"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --sunshine-dir) SUNSHINE_DIR="$2"; shift 2 ;;
        --agent-dir)    AGENT_DIR="$2"; shift 2 ;;
        --skip-deps)    SKIP_DEPS=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--sunshine-dir <path>] [--agent-dir <path>] [--skip-deps]"
            echo ""
            echo "  --sunshine-dir   Path to Sunshine source (default: ../Sunshine)"
            echo "  --agent-dir      Path to deragabu-agent source (default: script directory)"
            echo "  --skip-deps      Skip Homebrew dependency installation"
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Resolve to absolute paths
AGENT_DIR="$(cd "$AGENT_DIR" && pwd)"
SUNSHINE_DIR="$(mkdir -p "$(dirname "$SUNSHINE_DIR")" && cd "$(dirname "$SUNSHINE_DIR")" && echo "$(pwd)/$(basename "$SUNSHINE_DIR")")"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Sunshine + Deragabu Agent — macOS Unified Build            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "  Agent source:    $AGENT_DIR"
echo "  Sunshine source: $SUNSHINE_DIR"
echo ""

# ─── Step 1: Install Dependencies ───────────────────────────────────────────

if [[ "$SKIP_DEPS" == false ]]; then
    echo "━━━ Step 1: Installing macOS dependencies via Homebrew ━━━"

    if ! command -v brew &>/dev/null; then
        echo "ERROR: Homebrew is required. Install from https://brew.sh/"
        exit 1
    fi

    dependencies=(
        "boost"
        "cmake"
        "icu4c"
        "miniupnpc"
        "ninja"
        "node"
        "openssl@3"
        "opus"
        "pkg-config"
    )

    echo "Installing: ${dependencies[*]}"
    brew install "${dependencies[@]}" 2>/dev/null || true

    # Ensure Rust toolchain is installed
    if ! command -v cargo &>/dev/null; then
        echo "Installing Rust toolchain..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
    fi

    echo "✓ Dependencies installed"
    echo ""
else
    echo "━━━ Step 1: Skipping dependency installation ━━━"
    echo ""
fi

# ─── Step 2: Clone Sunshine ─────────────────────────────────────────────────

echo "━━━ Step 2: Preparing Sunshine source ━━━"

if [[ ! -d "$SUNSHINE_DIR/.git" ]]; then
    echo "Cloning Sunshine..."
    git clone https://github.com/LizardByte/Sunshine.git \
        --recurse-submodules \
        "$SUNSHINE_DIR"
else
    echo "Sunshine already cloned at $SUNSHINE_DIR"
    echo "Updating submodules..."
    (cd "$SUNSHINE_DIR" && git submodule update --init --recursive)
fi

echo "✓ Sunshine source ready"
echo ""

# ─── Step 3: Build deragabu-agent as static library ─────────────────────────

echo "━━━ Step 3: Building deragabu-agent static library ━━━"

(cd "$AGENT_DIR" && cargo rustc --release --lib --crate-type staticlib)

AGENT_LIB="$AGENT_DIR/target/release/libderagabu_agent.a"
AGENT_HEADER="$AGENT_DIR/include/deragabu_agent.h"

if [[ ! -f "$AGENT_LIB" ]]; then
    echo "ERROR: Static library not found at $AGENT_LIB"
    exit 1
fi
if [[ ! -f "$AGENT_HEADER" ]]; then
    echo "ERROR: Header file not found at $AGENT_HEADER"
    exit 1
fi

echo "✓ Static library: $AGENT_LIB ($(du -h "$AGENT_LIB" | cut -f1))"
echo ""

# ─── Step 4: Patch Sunshine source ──────────────────────────────────────────

echo "━━━ Step 4: Patching Sunshine source ━━━"

# Create a marker file to track if we've already patched
PATCH_MARKER="$SUNSHINE_DIR/.deragabu-patched"

if [[ -f "$PATCH_MARKER" ]]; then
    echo "Sunshine already patched (found marker). Reverting first..."
    (cd "$SUNSHINE_DIR" && git checkout -- src/ 2>/dev/null || true)
    rm -f "$PATCH_MARKER"
fi

# ── Patch 4a: av_video.m — Set capturesCursor=NO at session init ──
#
# This is a best-effort patch. Known limitation:
#   macOS bug — if the user has changed cursor size in Accessibility settings,
#   capturesCursor=NO may be silently ignored and the hardware cursor will
#   still appear in the captured video. There is no known workaround.
#   The agent's soft cursor overlay compensates by always being the primary
#   cursor mechanism.

AV_VIDEO_M="$SUNSHINE_DIR/src/platform/macos/av_video.m"

if [[ -f "$AV_VIDEO_M" ]]; then
    echo "  Patching av_video.m (capturesCursor=NO at init)..."

    if ! grep -q "capturesCursor" "$AV_VIDEO_M"; then
        # After the line that creates the screenInput, add capturesCursor=NO
        # wrapped in beginConfiguration/commitConfiguration.
        #
        # Original:   AVCaptureScreenInput *screenInput = [[AVCaptureScreenInput alloc] initWithDisplayID:self.displayID];
        # After:      + [screenInput setCapturesCursor:NO];
        #
        # We also wrap the session addInput in beginConfiguration/commitConfiguration
        # to ensure the capturesCursor change is applied atomically.
        sed -i.bak '/AVCaptureScreenInput \*screenInput = \[\[AVCaptureScreenInput alloc\] initWithDisplayID:self\.displayID\];/ a\
\
  // Deragabu Agent: hide hardware cursor from capture.\
  // The agent provides a low-latency soft cursor overlay via WebRTC.\
  // NOTE: Known macOS bug — if Accessibility cursor size != default,\
  //       this setting may be silently ignored.\
  [screenInput setCapturesCursor:NO];
' "$AV_VIDEO_M"

        rm -f "${AV_VIDEO_M}.bak"
        echo "    ✓ Set capturesCursor=NO at session init"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: av_video.m not found at $AV_VIDEO_M"
fi

# ── Patch 4b: display.mm — Forward *cursor to agent (overlay control) ──
#
# Hardware cursor is already set to NO at init (av_video.m patch above).
# Here we forward the *cursor flag to the agent so the soft cursor
# overlay can be shown/hidden.
#
# Direct mapping (no inversion needed):
#   *cursor=true  (user wants visible cursor) → set_display_cursor(true)
#     → agent sends draw_cursor=true → client shows overlay ✓
#   *cursor=false (user wants no cursor)      → set_display_cursor(false)
#     → agent sends draw_cursor=false → client hides overlay ✓

DISPLAY_MM="$SUNSHINE_DIR/src/platform/macos/display.mm"

if [[ -f "$DISPLAY_MM" ]]; then
    echo "  Patching display.mm (agent overlay control)..."

    if ! grep -q "deragabu_agent" "$DISPLAY_MM"; then
        # 1. Add #include for the agent header at the top (after existing includes)
        sed -i.bak '/#include "src\/video.h"/ a\
\
// Deragabu Agent — cursor overlay + clipboard sync\
extern "C" {\
#include "deragabu_agent.h"\
}' "$DISPLAY_MM"

        # 2. In the capture method, forward *cursor to the agent directly.
        #    We do NOT touch capturesCursor here — it is set once at init.
        #    Find the line:  auto signal = [av_capture capture:^(CMSampleBufferRef sampleBuffer) {
        #    And insert our agent notification BEFORE it.
        sed -i.bak2 '/auto signal = \[av_capture capture:\^(CMSampleBufferRef sampleBuffer)/ i\
\
      // ── Deragabu Agent: forward cursor visibility to overlay ──\
      // capturesCursor is set to NO at init (av_video.m), but may be\
      // silently ignored due to macOS bug (Accessibility cursor size).\
      // The overlay is the authoritative cursor — pass *cursor directly:\
      //   true  → show overlay,  false → hide overlay\
      {\
        static bool last_cursor_state = true;\
        bool want_cursor = cursor ? *cursor : true;\
        if (want_cursor != last_cursor_state) {\
          if (deragabu_agent_is_running()) {\
            deragabu_agent_set_display_cursor(want_cursor);\
          }\
          last_cursor_state = want_cursor;\
          BOOST_LOG(info) << "Cursor overlay " << (want_cursor ? "shown" : "hidden");\
        }\
      }\
' "$DISPLAY_MM"

        rm -f "${DISPLAY_MM}.bak" "${DISPLAY_MM}.bak2"
        echo "    ✓ Added agent overlay control"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: display.mm not found at $DISPLAY_MM"
fi

# ── Patch 4c: Sunshine main — Initialize/shutdown agent ──

# Find the main entry point (could be src/main.cpp or src/entry_handler.cpp)
MAIN_CPP=""
for candidate in \
    "$SUNSHINE_DIR/src/main.cpp" \
    "$SUNSHINE_DIR/src/entry_handler.cpp" \
    "$SUNSHINE_DIR/src/main.h"; do
    if [[ -f "$candidate" ]]; then
        MAIN_CPP="$candidate"
        break
    fi
done

if [[ -n "$MAIN_CPP" && -f "$MAIN_CPP" ]]; then
    echo "  Patching $(basename "$MAIN_CPP") for agent init/shutdown..."

    if ! grep -q "deragabu_agent" "$MAIN_CPP"; then
        # Add include at top
        sed -i.bak '1 i\
// Deragabu Agent\
#ifdef __APPLE__\
extern "C" {\
#include "deragabu_agent.h"\
}\
#endif\
' "$MAIN_CPP"

        # Try to find main() or the entry function and add init after first {
        # This is a best-effort patch — may need manual adjustment
        if grep -q "int main(" "$MAIN_CPP"; then
            sed -i.bak2 '/int main(/ {
                n
                /^{/ a\
\
  // Initialize deragabu-agent (cursor overlay + clipboard sync)\
  #ifdef __APPLE__\
  if (deragabu_agent_init("'"$AGENT_BIND_ADDR"'") != 0) {\
    BOOST_LOG(warning) << "Failed to initialize deragabu agent";\
  }\
  #endif\

            }' "$MAIN_CPP"

            # Add shutdown before return in main
            # This is best-effort — add atexit instead for robustness
            sed -i.bak3 '/int main(/ {
                n
                /^{/ a\
  #ifdef __APPLE__\
  atexit([]() { deragabu_agent_shutdown(); });\
  #endif\

            }' "$MAIN_CPP"
        fi

        rm -f "${MAIN_CPP}.bak" "${MAIN_CPP}.bak2" "${MAIN_CPP}.bak3"
        echo "    ✓ Added agent init/shutdown"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: Could not find main entry point. Manual patching needed."
    echo "  Add to Sunshine's main():"
    echo '    #include "deragabu_agent.h"'
    echo '    deragabu_agent_init("0.0.0.0:9000");'
    echo '    atexit([]() { deragabu_agent_shutdown(); });'
fi

# ── Patch 4d: CMakeLists.txt — Link agent static library ──

CMAKE_FILE="$SUNSHINE_DIR/CMakeLists.txt"

if [[ -f "$CMAKE_FILE" ]]; then
    echo "  Patching CMakeLists.txt..."

    if ! grep -q "deragabu_agent" "$CMAKE_FILE"; then
        # Add the agent library linking in the macOS section
        # We append to the end of the file inside an APPLE guard
        cat >> "$CMAKE_FILE" << CMAKE_EOF

# ── Deragabu Agent (Rust static library) ──────────────────────────────────
if(APPLE)
  # Path to pre-built agent static library
  set(DERAGABU_AGENT_LIB "$AGENT_LIB")
  set(DERAGABU_AGENT_INCLUDE "$AGENT_DIR/include")

  if(EXISTS "\${DERAGABU_AGENT_LIB}")
    message(STATUS "Deragabu Agent: \${DERAGABU_AGENT_LIB}")

    # Add the static library as an imported target
    add_library(deragabu_agent STATIC IMPORTED)
    set_target_properties(deragabu_agent PROPERTIES
      IMPORTED_LOCATION "\${DERAGABU_AGENT_LIB}"
    )

    # Link agent + system frameworks it depends on
    # NOTE: Sunshine uses the plain signature of target_link_libraries in
    # cmake/targets/common.cmake, so we must also use the plain form here
    # (mixing plain and keyword signatures on the same target is a CMake error).
    target_link_libraries(sunshine
      deragabu_agent
      "-framework CoreGraphics"
      "-framework CoreFoundation"
      "-framework Security"
      "-framework SystemConfiguration"
      "-framework IOKit"
      resolv
    )
    target_include_directories(sunshine PRIVATE "\${DERAGABU_AGENT_INCLUDE}")
  else()
    message(WARNING "Deragabu Agent library not found at \${DERAGABU_AGENT_LIB}")
  endif()
endif()
CMAKE_EOF
        echo "    ✓ Added agent linking to CMakeLists.txt"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: CMakeLists.txt not found"
fi

# Create patch marker
touch "$PATCH_MARKER"
echo ""
echo "✓ All patches applied"
echo ""

# ─── Step 5: Build Sunshine ─────────────────────────────────────────────────

echo "━━━ Step 5: Building Sunshine ━━━"

mkdir -p "$SUNSHINE_DIR/build"

(cd "$SUNSHINE_DIR" && \
    cmake -B build -G Ninja -S . \
        -DCMAKE_BUILD_TYPE=Release \
        -DSUNSHINE_ENABLE_TRAY=OFF \
        2>&1 | tail -20
)

echo ""
echo "Running ninja..."

(cd "$SUNSHINE_DIR" && ninja -C build 2>&1) || {
    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║  BUILD FAILED                                              ║"
    echo "║                                                            ║"
    echo "║  The automated patches may need manual adjustment for      ║"
    echo "║  your version of Sunshine. Check the errors above.         ║"
    echo "║                                                            ║"
    echo "║  Key files to review:                                      ║"
    echo "║    src/platform/macos/av_video.m    (capturesCursor=NO)    ║"
    echo "║    src/platform/macos/display.mm    (agent overlay ctrl)   ║"
    echo "║    src/main.cpp                     (agent init/shutdown)  ║"
    echo "║    CMakeLists.txt                   (agent linking)        ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    exit 1
}

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  BUILD SUCCESSFUL!                                         ║"
echo "╠══════════════════════════════════════════════════════════════╣"
echo "║                                                            ║"
echo "║  Sunshine binary: $SUNSHINE_DIR/build/sunshine"
echo "║  Agent WebRTC:    http://$AGENT_BIND_ADDR"
echo "║                                                            ║"
echo "║  What's integrated:                                        ║"
echo "║  ✓ Cursor overlay via WebRTC (replaces system cursor)      ║"
echo "║  ✓ capturesCursor=NO at init (best-effort HW cursor hide)  ║"
echo "║  ✓ Clipboard sync (text + images, bidirectional)           ║"
echo "║  ✓ Agent auto-start/stop with Sunshine lifecycle           ║"
echo "║                                                            ║"
echo "║  To package:                                               ║"
echo "║    cd $SUNSHINE_DIR"
echo "║    cpack -G DragNDrop --config ./build/CPackConfig.cmake   ║"
echo "║                                                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
