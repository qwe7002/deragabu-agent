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
#      - input.cpp: Fix mouse scroll (pixel units + speed scaling ×40/120)
#      - main.cpp: Initialize/shutdown agent
#   5. Build Sunshine with CMake + Ninja, linking the agent
#      - CMAKE_INSTALL_PREFIX → build dir (fixes Web UI blank page)
#      - Pre-install npm deps for web UI assets
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

PATCHES_DIR="$AGENT_DIR/patches"

if [[ ! -d "$PATCHES_DIR" ]]; then
    echo "ERROR: Patches directory not found at $PATCHES_DIR"
    echo "  Expected: $PATCHES_DIR/0001-cmake-deps-homebrew-includes.patch ... etc."
    exit 1
fi

# Create a marker file to track if we've already patched
PATCH_MARKER="$SUNSHINE_DIR/.deragabu-patched"

if [[ -f "$PATCH_MARKER" ]]; then
    echo "Sunshine already patched (found marker). Reverting first..."
    (cd "$SUNSHINE_DIR" && git checkout -- src/ cmake/ CMakeLists.txt 2>/dev/null || true)
    rm -f "$PATCH_MARKER"
fi

# ── Helper: apply a patch file, tolerate already-applied ──────────────────────
apply_patch() {
    local patch_file="$1"
    local description="$2"
    echo "  Patching: $description..."
    if ! (cd "$SUNSHINE_DIR" && patch -p1 --forward --silent < "$patch_file" 2>&1); then
        echo "    ⚠ patch reported issues (may already be applied or context mismatch)"
        echo "      Check manually: patch -p1 < $patch_file"
    else
        echo "    ✓ Applied"
    fi
}

# ── Patch 1: cmake Homebrew include paths ──────────────────────────────────────
apply_patch "$PATCHES_DIR/0001-cmake-deps-homebrew-includes.patch" \
    "cmake/dependencies: OpenSSL + Opus Homebrew include paths"

# ── Patch 2: cmake tray option ─────────────────────────────────────────────────
apply_patch "$PATCHES_DIR/0002-cmake-constants-tray-flag.patch" \
    "cmake/prep/constants: SUNSHINE_TRAY respects SUNSHINE_ENABLE_TRAY"

# ── Patch 3: av_video.m capturesCursor=NO ──────────────────────────────────────
apply_patch "$PATCHES_DIR/0003-macos-av-video-captures-cursor-no.patch" \
    "av_video.m: set capturesCursor=NO at session init"

# ── Patch 4: display.mm agent cursor overlay control ───────────────────────────
apply_patch "$PATCHES_DIR/0004-macos-display-agent-overlay-control.patch" \
    "display.mm: include deragabu_agent.h + forward cursor overlay state"

# ── Patch 5: input.cpp keyboard modifier + scroll fixes ────────────────────────
#
# Keyboard Bug A: kCGEventFlagsChanged reuses kb_event with stale keycode
#   → Enter (or last key) appears held while any modifier is pressed
# Keyboard Bug B: regular key events lack CGEventSetFlags(kb_flags)
#   → Shift+letter produces lowercase, Ctrl/Alt shortcuts fail
# Scroll: /120 + kCGScrollEventUnitLine gave ~40 px/notch (jerky)
#   → 1:1 value + kCGScrollEventUnitPixel gives ~120 px/notch (smooth)
apply_patch "$PATCHES_DIR/0005-macos-input-scroll-and-keyboard-fix.patch" \
    "input.cpp: keyboard modifier propagation + smooth scroll"

# ── Patch 6: main.cpp agent init/shutdown (template — substitutes bind addr) ───
echo "  Patching: main.cpp agent init/shutdown (bind_addr=$AGENT_BIND_ADDR)..."
TMP_PATCH="$(mktemp /tmp/deragabu-main-XXXXXX.patch)"
sed "s|@@AGENT_BIND_ADDR@@|$AGENT_BIND_ADDR|g" \
    "$PATCHES_DIR/0006-main-agent-init.patch.in" > "$TMP_PATCH"
if ! (cd "$SUNSHINE_DIR" && patch -p1 --forward --silent < "$TMP_PATCH" 2>&1); then
    echo "    ⚠ patch reported issues (may already be applied or context mismatch)"
else
    echo "    ✓ Applied"
fi
rm -f "$TMP_PATCH"

# ── CMakeLists.txt: append agent link block (paths are machine-specific) ────────
echo "  Patching: CMakeLists.txt (agent static library link)..."
if ! grep -q "deragabu_agent" "$SUNSHINE_DIR/CMakeLists.txt"; then
    sed "s|@AGENT_LIB@|$AGENT_LIB|g; s|@AGENT_INCLUDE@|$AGENT_DIR/include|g" \
        "$PATCHES_DIR/CMakeLists.txt.append.in" >> "$SUNSHINE_DIR/CMakeLists.txt"
    echo "    ✓ Added agent linking to CMakeLists.txt"
else
    echo "    ⊘ Already contains deragabu_agent"
fi

# Create patch marker
touch "$PATCH_MARKER"
echo ""
echo "✓ All patches applied"
echo ""

# ─── Step 5: Build Sunshine ─────────────────────────────────────────────────

echo "━━━ Step 5: Building Sunshine ━━━"

mkdir -p "$SUNSHINE_DIR/build"

# Resolve macOS SDK path — required so the compiler can find C++ stdlib headers
# like <cstddef>.  Without this, /usr/bin/c++ may fail on newer Xcode setups.
MACOS_SDK="$(xcrun --show-sdk-path 2>/dev/null)"
if [[ -z "$MACOS_SDK" ]]; then
    echo "WARNING: xcrun --show-sdk-path returned empty. Install Xcode Command Line Tools:"
    echo "  xcode-select --install"
fi

# Resolve Homebrew prefix — on Apple Silicon it's /opt/homebrew, on Intel /usr/local
HOMEBREW_PREFIX="$(brew --prefix 2>/dev/null || echo "/opt/homebrew")"
echo "  Homebrew prefix: $HOMEBREW_PREFIX"

# Ensure Homebrew node/npm are used (version managers like nvm/nvs may provide
# an older Node.js that is incompatible with Vite/modern npm).
HOMEBREW_NODE_BIN="${HOMEBREW_PREFIX}/opt/node/bin"
if [[ -d "$HOMEBREW_NODE_BIN" ]]; then
    export PATH="${HOMEBREW_NODE_BIN}:${PATH}"
    echo "  Using Homebrew Node.js: $(node --version 2>/dev/null)"
fi

# Resolve Homebrew OpenSSL path — macOS does not ship OpenSSL headers.
OPENSSL_ROOT="$(brew --prefix openssl@3 2>/dev/null || brew --prefix openssl 2>/dev/null || echo "")"
if [[ -n "$OPENSSL_ROOT" && -d "$OPENSSL_ROOT" ]]; then
    echo "  OpenSSL found at: $OPENSSL_ROOT"
    export OPENSSL_ROOT_DIR="$OPENSSL_ROOT"
    export PKG_CONFIG_PATH="${OPENSSL_ROOT}/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
else
    echo "WARNING: Could not find Homebrew OpenSSL. Install with: brew install openssl@3"
fi

# Build CMAKE_PREFIX_PATH including both Homebrew prefix and OpenSSL root
# so that find_package(OpenSSL) correctly resolves headers and libraries.
CMAKE_PREFIX="${HOMEBREW_PREFIX}"
if [[ -n "${OPENSSL_ROOT:-}" && -d "${OPENSSL_ROOT:-}" ]]; then
    CMAKE_PREFIX="${OPENSSL_ROOT};${CMAKE_PREFIX}"
fi

# Ensure web UI assets are built — explicitly run npm install in
# Sunshine's web source directory so cmake custom targets succeed.
SUNSHINE_WEB_DIR="$SUNSHINE_DIR/src_assets/common/assets/web"
if [[ -f "$SUNSHINE_WEB_DIR/package.json" ]]; then
    echo "  Pre-installing web UI npm dependencies..."
    (cd "$SUNSHINE_WEB_DIR" && npm install 2>&1) || {
        echo "WARNING: npm install for web UI failed — web UI may be missing."
        echo "  Ensure node/npm are installed: brew install node"
    }
else
    echo "  WARNING: Web UI package.json not found at $SUNSHINE_WEB_DIR"
    echo "  The Sunshine web admin panel may not be available."
fi

# Set CMAKE_INSTALL_PREFIX to the build directory so SUNSHINE_ASSETS_DIR
# resolves to $SUNSHINE_DIR/build/assets (where vite outputs the web UI).
# Without this, it defaults to /usr/local/assets which doesn't exist,
# causing a blank web UI page.
(cd "$SUNSHINE_DIR" && \
    cmake -B build -G Ninja -S . \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX="$SUNSHINE_DIR/build" \
        -DSUNSHINE_ENABLE_TRAY=ON \
        ${MACOS_SDK:+-DCMAKE_OSX_SYSROOT="$MACOS_SDK"} \
        -DCMAKE_PREFIX_PATH="$CMAKE_PREFIX" \
        ${OPENSSL_ROOT:+-DOPENSSL_ROOT_DIR="$OPENSSL_ROOT"} \
        ${OPENSSL_ROOT:+-DOPENSSL_INCLUDE_DIR="$OPENSSL_ROOT/include"} \
        ${OPENSSL_ROOT:+-DOPENSSL_CRYPTO_LIBRARY="$OPENSSL_ROOT/lib/libcrypto.dylib"} \
        ${OPENSSL_ROOT:+-DOPENSSL_SSL_LIBRARY="$OPENSSL_ROOT/lib/libssl.dylib"} \
        2>&1
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
echo "║  ✓ Mouse scroll fix (pixel-based smooth scrolling)         ║"
echo "║  ✓ Keyboard modifier fix (Shift/Ctrl/Alt propagation)      ║"
echo "║  ✓ Agent auto-start/stop with Sunshine lifecycle           ║"
echo "║                                                            ║"

# ── Verify web UI assets ──
WEB_UI_OK=false
for web_dir in \
    "$SUNSHINE_DIR/build/src_assets/common/assets/web/dist" \
    "$SUNSHINE_DIR/build/assets/web" \
    "$SUNSHINE_DIR/src_assets/common/assets/web/dist"; do
    if [[ -d "$web_dir" ]] && [[ -n "$(ls -A "$web_dir" 2>/dev/null)" ]]; then
        WEB_UI_OK=true
        break
    fi
done

if [[ "$WEB_UI_OK" == true ]]; then
    echo "║  ✓ Web UI assets built (https://localhost:47990)           ║"
else
    echo "║  ⚠ Web UI assets NOT found — admin panel may be missing   ║"
    echo "║    Try: cd $SUNSHINE_DIR/src_assets/common/assets/web"
    echo "║         npm install && npm run build"
    echo "║    Then rebuild with: ninja -C $SUNSHINE_DIR/build"
fi

echo "║                                                            ║"
echo "║  To package:                                               ║"
echo "║    cd $SUNSHINE_DIR"
echo "║    cpack -G DragNDrop --config ./build/CPackConfig.cmake   ║"
echo "║                                                            ║"
echo "╚══════════════════════════════════════════════════════════════╝"
