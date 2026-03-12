#!/bin/bash
set -e

# Build script for DeepTerm macOS .app bundle

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
APP_NAME="DeepTerm"
APP_BUNDLE="${APP_NAME}.app"
BUILD_DIR="${PROJECT_DIR}/target/release"

echo "=== Building DeepTerm.app ==="

# Build release binary
echo "Building release binary..."
cd "$PROJECT_DIR"
cargo build --release

# Create app bundle structure
echo "Creating app bundle..."
mkdir -p "${APP_BUNDLE}/Contents/MacOS"
mkdir -p "${APP_BUNDLE}/Contents/Resources"

# Copy Info.plist
cp "${PROJECT_DIR}/${APP_BUNDLE}/Contents/Info.plist" "${APP_BUNDLE}/Contents/Info.plist"

# Copy binary
cp "${BUILD_DIR}/${APP_NAME}" "${APP_BUNDLE}/Contents/MacOS/${APP_NAME}"

# Copy assets
echo "Copying assets..."
if [ -d "${PROJECT_DIR}/assets" ]; then
    cp -r "${PROJECT_DIR}/assets" "${APP_BUNDLE}/Contents/Resources/"
fi

echo "=== Build complete: ${APP_BUNDLE} ==="
echo "You can now double-click ${APP_BUNDLE} to run DeepTerm without opening a terminal."
