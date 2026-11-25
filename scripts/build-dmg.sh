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
# `cargo bundle --bin deepterm` builds only the GUI. Build both release
# binaries explicitly so the nested core can never be an older artifact left
# over from a previous build.
cargo build --release --bins
# **必须显式指定 --bin deepterm**：cargo-bundle 在有多个二进制时会自己
# 挑一个（这里挑成了 deepcore），导致 bundle 的 CFBundleExecutable 指向
# 守护进程、GUI 二进制根本没进包——应用在 Dock 里点不开。
cargo bundle --release --bin deepterm

# Check if bundle was created
if [ ! -d "${BUNDLE_DIR}/${APP_BUNDLE}" ]; then
    echo -e "${RED}Error: App bundle not found at ${BUNDLE_DIR}/${APP_BUNDLE}${NC}"
    exit 1
fi

# Copy bundle to project root for DMG creation
echo "Preparing app bundle..."
cp -R "${BUNDLE_DIR}/${APP_BUNDLE}" "${PROJECT_DIR}/${APP_BUNDLE}"

# 终端守护进程必须是**独立 app bundle**，不能只作为 DeepTerm.app 里的裸
# Mach-O。macOS 的本地网络授权按 app 身份与 responsibility 归属；裸 core
# 会继承 DeepTerm.app，GUI 退出后授权失效。结构照 herdr-core：独立
# CFBundleIdentifier、独立 Info.plist、独立签名，再由 launchd 脱离 GUI
# coalition。
CORE_BIN="${PROJECT_DIR}/target/release/deepcore"
if [ ! -f "${CORE_BIN}" ]; then
    echo -e "${RED}Error: deepcore not found at ${CORE_BIN}${NC}"
    echo "Run: cargo build --release --bins"
    exit 1
fi
CORE_APP="${PROJECT_DIR}/${APP_BUNDLE}/Contents/Resources/DeepTerm Core.app"
mkdir -p "${CORE_APP}/Contents/MacOS"
cp "${CORE_BIN}" "${CORE_APP}/Contents/MacOS/deepcore"
chmod +x "${CORE_APP}/Contents/MacOS/deepcore"
CORE_VERSION=$("${CORE_BIN}" --version | awk '{print $2}')
sed "s/@VERSION@/${CORE_VERSION}/g" \
    "${PROJECT_DIR}/distribution/macos-core-Info.plist" \
    > "${CORE_APP}/Contents/Info.plist"
# Bind the stable identifier to the executable and then to its app bundle.
# A future Developer ID build keeps this same identifier for TCC persistence.
codesign --force --sign - --identifier dev.deepterm.core \
    "${CORE_APP}/Contents/MacOS/deepcore"
codesign --force --sign - "${CORE_APP}"
echo -e "${GREEN}DeepTerm Core.app bundled and signed.${NC}"

# holder 单独一个 app bundle：它注册 AppKit（换成 accessory app）之后活动
# 监视器显示的是 CFBundleDisplayName，和 core 共用 bundle 时会被显示成同一个
# 名字。二进制必须是同一份构建的副本——core 用 holder 的 hello.build 判断
# “新终端交给谁”，指纹不一致会让刚拉起的 holder 立刻 drain。
HOLDER_APP="${PROJECT_DIR}/${APP_BUNDLE}/Contents/Resources/DeepHolder.app"
mkdir -p "${HOLDER_APP}/Contents/MacOS"
cp "${CORE_BIN}" "${HOLDER_APP}/Contents/MacOS/deepholder"
chmod +x "${HOLDER_APP}/Contents/MacOS/deepholder"
sed "s/@VERSION@/${CORE_VERSION}/g" \
    "${PROJECT_DIR}/distribution/macos-holder-Info.plist" \
    > "${HOLDER_APP}/Contents/Info.plist"
codesign --force --sign - --identifier dev.deepterm.holder \
    "${HOLDER_APP}/Contents/MacOS/deepholder"
codesign --force --sign - "${HOLDER_APP}"
echo -e "${GREEN}DeepHolder.app bundled and signed.${NC}"

# **两份二进制必须来自同一份构建。** 签名会改写文件尾部，所以不能比较整个
# 文件的哈希；`identity::build_id()` 在 macOS 上取的是链接器写入的 LC_UUID，
# 签名不影响它。这里直接读 LC_UUID 断言，否则 holder 会被 core 判成旧构建。
uuid_of() {
    otool -l "$1" | awk '/LC_UUID/{found=1; next} found && $1=="uuid"{print $2; exit}'
}
CORE_UUID=$(uuid_of "${CORE_APP}/Contents/MacOS/deepcore")
HOLDER_UUID=$(uuid_of "${HOLDER_APP}/Contents/MacOS/deepholder")
if [ -z "${CORE_UUID}" ] || [ "${CORE_UUID}" != "${HOLDER_UUID}" ]; then
    echo -e "${RED}Error: core 与 holder 的 LC_UUID 不一致（${CORE_UUID} vs ${HOLDER_UUID}）${NC}"
    echo "两者必须来自同一次 cargo build，否则新 holder 会被判成旧构建并立刻 drain。"
    exit 1
fi
echo -e "${GREEN}core 与 holder 的构建指纹一致: ${CORE_UUID}${NC}"

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

# Inject privacy usage descriptions. cargo-bundle cannot set these keys;
# without them macOS silently returns EPERM instead of showing a prompt.
# Keep the outer app in sync with the holder's user-folder descriptions:
# terminal children may be attributed to either responsible process.
echo "Patching Info.plist for privacy permissions..."
PLIST="${PROJECT_DIR}/${APP_BUNDLE}/Contents/Info.plist"
set_plist_string() {
    local key="$1"
    local description="$2"
    /usr/libexec/PlistBuddy -c "Add :${key} string ${description}" "${PLIST}" 2>/dev/null \
        || /usr/libexec/PlistBuddy -c "Set :${key} ${description}" "${PLIST}"
}
set_plist_string "NSLocalNetworkUsageDescription" \
    "DeepTerm 需要访问本地网络，以连接局域网中的 SSH 服务器。"
set_plist_string "NSDocumentsFolderUsageDescription" \
    "DeepTerm 需要访问“文稿”文件夹，以打开项目、编辑文件并运行终端程序。"
set_plist_string "NSDesktopFolderUsageDescription" \
    "DeepTerm 需要访问桌面，以打开项目、编辑文件并运行终端程序。"
set_plist_string "NSDownloadsFolderUsageDescription" \
    "DeepTerm 需要访问“下载”文件夹，以打开文件并运行终端程序。"
# The nested core is already signed. Sign only the outer bundle so codesign
# does not replace the core's independently bound identifier.
codesign --force --sign - "${PROJECT_DIR}/${APP_BUNDLE}"

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
