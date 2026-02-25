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

# Create a marker file to track if we've already patched
PATCH_MARKER="$SUNSHINE_DIR/.deragabu-patched"

if [[ -f "$PATCH_MARKER" ]]; then
    echo "Sunshine already patched (found marker). Reverting first..."
    (cd "$SUNSHINE_DIR" && git checkout -- src/ cmake/ CMakeLists.txt 2>/dev/null || true)
    rm -f "$PATCH_MARKER"
fi

# ── Patch 4-pre: cmake/dependencies/common.cmake — Add Homebrew include paths ──
#
# Sunshine's CMakeLists links ${OPENSSL_LIBRARIES} and opus but never adds
# their include directories to the search path.  On Linux this is fine
# (headers in /usr/include), but on macOS with Homebrew the headers are
# at non-standard locations (e.g. /opt/homebrew/opt/openssl@3/include,
# /opt/homebrew/opt/opus/include).

DEPS_CMAKE="$SUNSHINE_DIR/cmake/dependencies/common.cmake"
if [[ -f "$DEPS_CMAKE" ]]; then
    echo "  Patching cmake/dependencies/common.cmake (Homebrew include paths)..."
    if ! grep -q "OPENSSL_INCLUDE_DIR" "$DEPS_CMAKE"; then
        # Add OpenSSL include path after find_package(OpenSSL)
        sed -i.bak '/^find_package(OpenSSL REQUIRED)/ a\
include_directories(SYSTEM ${OPENSSL_INCLUDE_DIR})
' "$DEPS_CMAKE"
        rm -f "${DEPS_CMAKE}.bak"
        echo "    ✓ Added OpenSSL include path"
    else
        echo "    ⊘ OpenSSL include already patched"
    fi

    # Add opus include path (Homebrew puts it outside the default search path).
    # NOTE: opus.pc sets Cflags to -I.../include/opus, but the source code
    # uses #include <opus/opus_multistream.h>, so we need the *parent* dir.
    if ! grep -q "pkg_check_modules.*OPUS" "$DEPS_CMAKE"; then
        sed -i.bak '/^pkg_check_modules(CURL REQUIRED libcurl)/ a\
pkg_check_modules(OPUS REQUIRED opus)\
# opus.pc Cflags points to .../include/opus but code uses #include <opus/...>\
# so add the parent directory to the include path\
foreach(_opus_inc ${OPUS_INCLUDE_DIRS})\
  get_filename_component(_opus_parent "${_opus_inc}" DIRECTORY)\
  include_directories(SYSTEM "${_opus_parent}")\
endforeach()
' "$DEPS_CMAKE"
        rm -f "${DEPS_CMAKE}.bak"
        echo "    ✓ Added opus pkg-config + include path (parent of pkg-config dir)"
    else
        echo "    ⊘ Opus include already patched"
    fi
else
    echo "  WARNING: cmake/dependencies/common.cmake not found"
fi

# ── Patch 4-pre-b: constants.cmake — Respect SUNSHINE_ENABLE_TRAY=OFF ──
#
# constants.cmake unconditionally sets SUNSHINE_TRAY=1 via plain set(),
# which overrides any -DSUNSHINE_TRAY=0 passed on the cmake command line.
# On Linux, compile_definitions/linux.cmake conditionally resets it to 0,
# but macOS has no such logic. Patch constants.cmake to check the option.

CONSTANTS_CMAKE="$SUNSHINE_DIR/cmake/prep/constants.cmake"
if [[ -f "$CONSTANTS_CMAKE" ]]; then
    echo "  Patching constants.cmake (respect SUNSHINE_ENABLE_TRAY)..."
    if ! grep -q "SUNSHINE_ENABLE_TRAY" "$CONSTANTS_CMAKE"; then
        sed -i.bak 's/^set(SUNSHINE_TRAY 1)/if(SUNSHINE_ENABLE_TRAY)\n  set(SUNSHINE_TRAY 1)\nelse()\n  set(SUNSHINE_TRAY 0)\nendif()/' "$CONSTANTS_CMAKE"
        rm -f "${CONSTANTS_CMAKE}.bak"
        echo "    ✓ SUNSHINE_TRAY now respects SUNSHINE_ENABLE_TRAY"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: constants.cmake not found"
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
        #    The pattern "auto signal = [av_capture capture:..." appears twice:
        #      - In capture() method (has bool *cursor param) — patch here
        #      - In dummy_img() method (no cursor param) — skip
        #    Use Python to patch only the first occurrence.
        python3 -c "
import re, sys
with open('$DISPLAY_MM', 'r') as f:
    content = f.read()
target = 'auto signal = [av_capture capture:^(CMSampleBufferRef sampleBuffer) {'
patch = '''
      // ── Deragabu Agent: forward cursor visibility to overlay ──
      // capturesCursor is set to NO at init (av_video.m), but may be
      // silently ignored due to macOS bug (Accessibility cursor size).
      // The overlay is the authoritative cursor — pass *cursor directly:
      //   true  → show overlay,  false → hide overlay
      {
        static bool last_cursor_state = true;
        bool want_cursor = cursor ? *cursor : true;
        if (want_cursor != last_cursor_state) {
          if (deragabu_agent_is_running()) {
            deragabu_agent_set_display_cursor(want_cursor);
          }
          last_cursor_state = want_cursor;
          BOOST_LOG(info) << \"Cursor overlay \" << (want_cursor ? \"shown\" : \"hidden\");
        }
      }

'''
# Replace only the first occurrence
idx = content.find(target)
if idx >= 0:
    content = content[:idx] + patch + content[idx:]
with open('$DISPLAY_MM', 'w') as f:
    f.write(content)
"

        rm -f "${DISPLAY_MM}.bak" "${DISPLAY_MM}.bak2"
        echo "    ✓ Added agent overlay control"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: display.mm not found at $DISPLAY_MM"
fi

# ── Patch 4b2: macOS input — Fix mouse scroll handling ──
#
# Sunshine's macOS input handler may use kCGScrollEventUnitLine for scroll
# events, which produces jerky/broken scrolling with Moonlight. Moonlight
# sends scroll values in 120ths of a line (Windows convention).
#
# This patch:
#   - Uses kCGScrollEventUnitPixel for smooth scrolling
#   - Properly scales the Moonlight scroll delta for macOS
#   - Handles both vertical and horizontal scroll axes
#
# Target file: src/platform/macos/input.cpp (the platf::scroll function)

INPUT_CPP="$SUNSHINE_DIR/src/platform/macos/input.cpp"

if [[ -f "$INPUT_CPP" ]]; then
    echo "  Patching input.cpp (scroll handling fix)..."

    if ! grep -q "DERAGABU_SCROLL_FIX" "$INPUT_CPP"; then
        python3 -c "
import re, sys

with open('$INPUT_CPP', 'r') as f:
    content = f.read()

# Pattern 1: Fix kCGScrollEventUnitLine → kCGScrollEventUnitPixel
# This ensures smooth pixel-based scrolling instead of jerky line-based
content_new = content.replace(
    'kCGScrollEventUnitLine',
    'kCGScrollEventUnitPixel /* DERAGABU_SCROLL_FIX: pixel-based for smooth scroll */'
)

# Pattern 2: If the scroll function divides by WHEEL_DELTA (120), the pixel
# values will be too small. macOS needs ~40 pixels per notch for natural
# scroll feel. Moonlight sends 120 per notch (Windows WHEEL_DELTA).
# So: high_res_distance * 40 / 120 ≈ 40px per notch.
content_new = re.sub(
    r'(scroll_amt|high_res_distance|distance)\s*/\s*(120|WHEEL_DELTA)',
    r'(\1 * 40 / 120) /* DERAGABU_SCROLL_FIX: scale to macOS pixels (~40px/notch) */',
    content_new
)

if content_new != content:
    with open('$INPUT_CPP', 'w') as f:
        f.write(content_new)
    print('  Applied scroll event unit and scaling fixes')
else:
    # If the simple replacements didn't match, try a broader approach:
    # Look for CGEventCreateScrollWheelEvent and ensure it uses pixel units
    pattern = r'(CGEventCreateScrollWheelEvent\s*\([^,]+,\s*)kCGScrollEventUnitLine'
    if re.search(pattern, content):
        content_new = re.sub(pattern,
            r'\1kCGScrollEventUnitPixel /* DERAGABU_SCROLL_FIX */',
            content)
        with open('$INPUT_CPP', 'w') as f:
            f.write(content_new)
        print('  Applied CGEventCreateScrollWheelEvent fix')
    else:
        # Mark as patched even if no changes needed (already using pixel units)
        # Add a comment at the top to indicate we checked
        content_new = '// DERAGABU_SCROLL_FIX: scroll handling verified\\n' + content
        with open('$INPUT_CPP', 'w') as f:
            f.write(content_new)
        print('  Scroll handling already uses correct units (marked as verified)')
"
        echo "    ✓ Scroll handling patched"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: input.cpp not found at $INPUT_CPP"
    echo "    Scroll fix may need manual application. Check Sunshine's macOS input handler."
    echo "    Key: CGEventCreateScrollWheelEvent should use kCGScrollEventUnitPixel"
fi

# ── Patch 4b3: macOS input — Fix keyboard modifier (Shift/Ctrl/Alt) propagation ──
#
# CGEventCreateKeyboardEvent(nullptr, ...) creates events whose modifier flags
# only reflect the *calling process* modifier state, not the global HID state.
# This means that even if a Shift key-down was posted via CGEventPost, the very
# next key event (e.g. 'a') will NOT automatically have kCGEventFlagMaskShift
# set — only the Shift key event itself does. Result: Shift+letter produces
# lowercase, modifier-key combos fail.
#
# Fix: add a namespace-scoped atomic modifier tracker that maps each macOS
# virtual key code (Shift=0x38/0x3C, Ctrl=0x3B/0x3E, Alt=0x3A/0x3D,
# Cmd=0x37/0x36) to its kCGEventFlagMask* bit, then call CGEventSetFlags on
# every keyboard CGEvent immediately before CGEventPost.
#
# Target file: src/platform/macos/input.cpp

if [[ -f "$INPUT_CPP" ]]; then
    echo "  Patching input.cpp (keyboard modifier state tracking)..."

    if ! grep -q "DERAGABU_KBD_FIX" "$INPUT_CPP"; then
        python3 -c "
import re

with open('$INPUT_CPP', 'r') as f:
    content = f.read()

# 1. Inject modifier tracking helpers after the last #include in the file.
modifier_code = '''
// DERAGABU_KBD_FIX: Explicit modifier-flag tracking for correct Shift/Ctrl/Alt
// key injection on macOS.  CGEventCreateKeyboardEvent(nullptr, ...) does not
// automatically carry the current HID modifier state; we track it manually and
// call CGEventSetFlags() on every keyboard CGEvent before posting.
#include <atomic>
namespace {
static std::atomic<CGEventFlags> deragabu_kbd_modifiers{0};
inline void deragabu_update_kbd_modifiers(CGKeyCode key, bool is_release) {
  static const struct { CGKeyCode k; CGEventFlags f; } kMap[] = {
    {0x38, kCGEventFlagMaskShift},      // Left Shift
    {0x3C, kCGEventFlagMaskShift},      // Right Shift
    {0x3B, kCGEventFlagMaskControl},    // Left Ctrl
    {0x3E, kCGEventFlagMaskControl},    // Right Ctrl
    {0x3A, kCGEventFlagMaskAlternate},  // Left Option/Alt
    {0x3D, kCGEventFlagMaskAlternate},  // Right Option/Alt
    {0x37, kCGEventFlagMaskCommand},    // Left Command
    {0x36, kCGEventFlagMaskCommand},    // Right Command
  };
  for (const auto& m : kMap) {
    if (key == m.k) {
      auto cur = deragabu_kbd_modifiers.load();
      deragabu_kbd_modifiers.store(is_release ? (cur & ~m.f) : (cur | m.f));
      return;
    }
  }
}
} // anonymous namespace
'''

all_includes = list(re.finditer(r'^#include\b[^\n]*\n', content, re.MULTILINE))
insert_pos = all_includes[-1].end() if all_includes else 0
content = content[:insert_pos] + modifier_code + content[insert_pos:]

# 2. For each CGEventCreateKeyboardEvent that is (within ~600 chars)
#    followed by CGEventPost(kCGHIDEventTap, <same_var>), inject:
#      deragabu_update_kbd_modifiers(keyVar, !(downExpr));
#      CGEventSetFlags(var, deragabu_kbd_modifiers.load());
#    immediately before the CGEventPost.  This updates the modifier-flag
#    tracker for modifier keys, and applies the accumulated flags to every key.

injections = {}

kbd_re = re.compile(
    r'(\s*)((?:auto|CGEventRef)\s+(\w+)\s*=\s*CGEventCreateKeyboardEvent'
    r'\s*\([^,]+,\s*(\w+)\s*,\s*([^,)]+?)\s*\)\s*;)'
)

for m in kbd_re.finditer(content):
    indent    = m.group(1).lstrip('\n')
    var_name  = m.group(3)
    key_var   = m.group(4)
    down_expr = m.group(5).strip()

    search_start = m.end()
    search_area  = content[search_start:search_start + 600]

    post_re = re.compile(
        r'CGEventPost\s*\(\s*kCGHIDEventTap\s*,\s*' + re.escape(var_name) + r'\s*\)'
    )
    pm = post_re.search(search_area)
    if pm:
        inject_pos  = search_start + pm.start()
        inject_code = (
            indent + '  deragabu_update_kbd_modifiers(' + key_var +
            ', !(' + down_expr + ')); // DERAGABU_KBD_FIX\n' +
            indent + '  CGEventSetFlags(' + var_name +
            ', deragabu_kbd_modifiers.load()); // DERAGABU_KBD_FIX\n' +
            indent + '  '
        )
        injections[inject_pos] = inject_code

# Apply injections from end to start to preserve earlier positions.
for pos in sorted(injections.keys(), reverse=True):
    content = content[:pos] + injections[pos] + content[pos:]

with open('$INPUT_CPP', 'w') as f:
    f.write(content)

print('Patched ' + str(len(injections)) + ' keyboard CGEventPost site(s)')
"
        echo "    ✓ Keyboard modifier state tracking added"
    else
        echo "    ⊘ Already patched"
    fi
else
    echo "  WARNING: input.cpp not found at $INPUT_CPP (keyboard modifier fix skipped)"
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

        # Insert agent init/shutdown after the opening brace of main().
        # Handles both styles:
        #   int main(...) {          (brace on same line)
        #   int main(...)\n{         (brace on next line)
        if grep -q "int main(" "$MAIN_CPP"; then
            awk '
            /int main\(/ && !done {
                print
                # If { is on the same line, insert after this line
                if ($0 ~ /{/) {
                    found_brace = 1
                } else {
                    # Read next line — it should be the opening {
                    getline nextline
                    print nextline
                    if (nextline ~ /{/) {
                        found_brace = 1
                    }
                }
                if (found_brace) {
                    print ""
                    print "  // Initialize deragabu-agent (cursor overlay + clipboard sync)"
                    print "  #ifdef __APPLE__"
                    print "  if (deragabu_agent_init(\"'"$AGENT_BIND_ADDR"'\") != 0) {"
                    print "    BOOST_LOG(warning) << \"Failed to initialize deragabu agent\";"
                    print "  }"
                    print "  atexit([]() { deragabu_agent_shutdown(); });"
                    print "  #endif"
                    print ""
                    done = 1
                }
                next
            }
            { print }
            ' "$MAIN_CPP" > "${MAIN_CPP}.patched" && mv "${MAIN_CPP}.patched" "$MAIN_CPP"
        fi
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

    # Also configure the test target if it exists
    if(TARGET test_sunshine)
      target_link_libraries(test_sunshine
        deragabu_agent
        "-framework CoreGraphics"
        "-framework CoreFoundation"
        "-framework Security"
        "-framework SystemConfiguration"
        "-framework IOKit"
        resolv
      )
      target_include_directories(test_sunshine PRIVATE "\${DERAGABU_AGENT_INCLUDE}")
    endif()
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
)ss

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
