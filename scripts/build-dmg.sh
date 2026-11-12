#!/bin/bash
set -e

# Build script for DeepTerm macOS DMG installer

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
APP_NAME="DeepTerm"
APP_BUNDLE="${APP_NAME}.app"
DMG_NAME="${APP_NAME}-0.1.0-macOS"
BUILD_DIR="${PROJECT_DIR}/target/release"
BUNDLE_DIR="${BUILD_DIR}/bundle/osx"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "=== Building DeepTerm DMG Installer ==="

# Check if cargo-bundle is installed
if ! command -v cargo-bundle &> /dev/null; then
    echo -e "${YELLOW}Installing cargo-bundle...${NC}"
    cargo install cargo-bundle
fi

# Clean previous builds
echo "Cleaning previous builds..."
rm -rf "${PROJECT_DIR}/${DMG_NAME}.dmg"
rm -rf "${PROJECT_DIR}/${APP_BUNDLE}"

# Build release binary with bundle
echo -e "${GREEN}Building release bundle...${NC}"
cd "$PROJECT_DIR"
cargo bundle --release

# Check if bundle was created
if [ ! -d "${BUNDLE_DIR}/${APP_BUNDLE}" ]; then
    echo -e "${RED}Error: App bundle not found at ${BUNDLE_DIR}/${APP_BUNDLE}${NC}"
    exit 1
fi

# Copy bundle to project root for DMG creation
echo "Preparing app bundle..."
cp -R "${BUNDLE_DIR}/${APP_BUNDLE}" "${PROJECT_DIR}/${APP_BUNDLE}"

# 终端守护进程要进 Contents/MacOS/（与主可执行文件同目录）。
#
# cargo-bundle 只打包 `package.metadata.bundle` 里那一个二进制，不会带上
# 别的。缺了它，应用能开窗，但"关掉窗口终端继续跑"会静默失效——
# 客户端找不到 deepcore 就退回进程内 PTY（这是有意的降级），
# 用户只会觉得功能没做。
CORE_BIN="${PROJECT_DIR}/target/release/deepcore"
if [ ! -f "${CORE_BIN}" ]; then
    echo -e "${RED}Error: deepcore not found at ${CORE_BIN}${NC}"
    echo "Run: cargo build --release --bins"
    exit 1
fi
cp "${CORE_BIN}" "${PROJECT_DIR}/${APP_BUNDLE}/Contents/MacOS/deepcore"
chmod +x "${PROJECT_DIR}/${APP_BUNDLE}/Contents/MacOS/deepcore"
echo -e "${GREEN}deepcore bundled.${NC}"

# Compile terminfo into app bundle (matching Alacritty's build process)
echo "Compiling terminfo..."
if ! command -v tic &> /dev/null; then
    echo -e "${YELLOW}Warning: tic not found. Install ncurses to enable terminfo support.${NC}"
else
    tic -xe deepterm \
        -o "${PROJECT_DIR}/${APP_BUNDLE}/Contents/Resources" \
        "${PROJECT_DIR}/assets/terminfo/deepterm.info"
    echo -e "${GREEN}Terminfo compiled into bundle.${NC}"
fi

# Inject the Local Network usage description. cargo-bundle cannot set this
# key, and without it macOS silently denies every LAN connection instead of
# showing the permission prompt (observed as EHOSTUNREACH "No route to host").
echo "Patching Info.plist for Local Network permission..."
PLIST="${PROJECT_DIR}/${APP_BUNDLE}/Contents/Info.plist"
LOCAL_NET_DESC="DeepTerm 需要访问本地网络,以连接局域网中的 SSH 服务器。"
/usr/libexec/PlistBuddy -c "Add :NSLocalNetworkUsageDescription string ${LOCAL_NET_DESC}" "${PLIST}" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Set :NSLocalNetworkUsageDescription ${LOCAL_NET_DESC}" "${PLIST}"
# Re-sign ad-hoc so the bundle signature matches the patched Info.plist
codesign --force --deep --sign - "${PROJECT_DIR}/${APP_BUNDLE}"

# Create DMG using create-dmg if available, otherwise use hdiutil
if command -v create-dmg &> /dev/null; then
    echo -e "${GREEN}Creating DMG with create-dmg...${NC}"
    create-dmg \
        --volname "${APP_NAME} Installer" \
        --window-pos 200 120 \
        --window-size 800 400 \
        --icon-size 100 \
        --icon "${APP_BUNDLE}" 200 190 \
        --hide-extension "${APP_BUNDLE}" \
        --app-drop-link 600 185 \
        "${PROJECT_DIR}/${DMG_NAME}.dmg" \
        "${PROJECT_DIR}/${APP_BUNDLE}"
else
    echo -e "${YELLOW}create-dmg not found, using hdiutil...${NC}"

    # Create temporary directory for DMG contents
    TEMP_DIR=$(mktemp -d)
    cp -R "${PROJECT_DIR}/${APP_BUNDLE}" "${TEMP_DIR}/"

    # Create Applications folder symlink
    ln -s /Applications "${TEMP_DIR}/Applications"

    # Create the DMG
    echo "Creating DMG archive..."
    # Size the temporary image from the actual payload plus headroom for
    # HFS+ metadata, so it keeps working as the bundle grows.
    APP_SIZE_MB=$(du -sm "${TEMP_DIR}" | cut -f1)
    DMG_SIZE_MB=$((APP_SIZE_MB + 30))
    hdiutil create \
        -srcfolder "${TEMP_DIR}" \
        -volname "${APP_NAME}" \
        -fs HFS+ \
        -format UDZO \
        -size "${DMG_SIZE_MB}m" \
        -o "${PROJECT_DIR}/${DMG_NAME}.dmg"

    # Cleanup temp directory
    rm -rf "${TEMP_DIR}"
fi

# Clean up
echo "Cleaning up..."
rm -rf "${PROJECT_DIR}/${APP_BUNDLE}"

# Sign the DMG if certificate is available (optional)
if [ -n "$DEVELOPER_ID" ]; then
    echo -e "${GREEN}Signing DMG...${NC}"
    codesign --sign "$DEVELOPER_ID" --force "${PROJECT_DIR}/${DMG_NAME}.dmg"
fi

echo -e "${GREEN}=== Build complete: ${DMG_NAME}.dmg ===${NC}"
echo "Location: ${PROJECT_DIR}/${DMG_NAME}.dmg"
echo ""
echo "To install:"
echo "1. Double-click ${DMG_NAME}.dmg to mount it"
echo "2. Drag DeepTerm.app to the Applications folder"
echo "3. Eject the DMG and launch DeepTerm from Applications"

# Show file info
ls -lh "${PROJECT_DIR}/${DMG_NAME}.dmg"
