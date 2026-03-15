#!/bin/bash
set -e

# Enhanced DMG build script for DeepTerm with better visual appearance
# Requires: create-dmg (brew install create-dmg)

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
NC='\033[0m'

echo "=== Building Enhanced DeepTerm DMG Installer ==="

# Check if create-dmg is installed
if ! command -v create-dmg &> /dev/null; then
    echo -e "${YELLOW}create-dmg not found. Installing...${NC}"
    if command -v brew &> /dev/null; then
        brew install create-dmg
    else
        echo -e "${RED}Homebrew not found. Please install create-dmg manually:${NC}"
        echo "  brew install create-dmg"
        echo ""
        echo "Falling back to basic hdiutil method..."
        exec "$SCRIPT_DIR/build-dmg.sh"
    fi
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
    echo -e "${RED}Error: App bundle not found${NC}"
    exit 1
fi

# Copy bundle to project root
cp -R "${BUNDLE_DIR}/${APP_BUNDLE}" "${PROJECT_DIR}/${APP_BUNDLE}"

# Create DMG with create-dmg (enhanced visual appearance)
echo -e "${GREEN}Creating enhanced DMG...${NC}"

# Create temporary directory for DMG background if needed
TEMP_DIR=$(mktemp -d)

# Create a simple background image if it doesn't exist
if [ ! -f "${PROJECT_DIR}/assets/dmg-background.png" ]; then
    echo "Creating background image..."
    # Create a simple background using sips and osascript
    # For now, we'll use the default create-dmg styling
    create-dmg \
        --volname "${APP_NAME} Installer" \
        --window-size 800 400 \
        --icon-size 100 \
        --icon "${APP_BUNDLE}" 200 200 \
        --hide-extension "${APP_BUNDLE}" \
        --app-drop-link 600 200 \
        --format UDZO \
        --eula "${PROJECT_DIR}/LICENSE" 2>/dev/null || true \
        "${PROJECT_DIR}/${DMG_NAME}.dmg" \
        "${PROJECT_DIR}/${APP_BUNDLE}" || {
            echo -e "${YELLOW}create-dmg failed, falling back to hdiutil...${NC}"
            rm -rf "${PROJECT_DIR}/${DMG_NAME}.dmg"
            hdiutil create \
                -srcfolder "${PROJECT_DIR}/${APP_BUNDLE}" \
                -volname "${APP_NAME}" \
                -fs HFS+ \
                -format UDZO \
                -o "${PROJECT_DIR}/${DMG_NAME}.dmg"
        }
else
    # Use custom background
    create-dmg \
        --volname "${APP_NAME} Installer" \
        --window-size 800 400 \
        --background "${PROJECT_DIR}/assets/dmg-background.png" \
        --icon-size 100 \
        --icon "${APP_BUNDLE}" 200 200 \
        --hide-extension "${APP_BUNDLE}" \
        --app-drop-link 600 200 \
        --format UDZO \
        "${PROJECT_DIR}/${DMG_NAME}.dmg" \
        "${PROJECT_DIR}/${APP_BUNDLE}"
fi

# Cleanup
rm -rf "${TEMP_DIR}"
rm -rf "${PROJECT_DIR}/${APP_BUNDLE}"

# Sign if available
if [ -n "$DEVELOPER_ID" ]; then
    echo -e "${GREEN}Signing DMG...${NC}"
    codesign --sign "$DEVELOPER_ID" --force "${PROJECT_DIR}/${DMG_NAME}.dmg"
fi

echo -e "${GREEN}=== Build complete: ${DMG_NAME}.dmg ===${NC}"
ls -lh "${PROJECT_DIR}/${DMG_NAME}.dmg"
