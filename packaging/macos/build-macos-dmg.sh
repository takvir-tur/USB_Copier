#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
APP_NAME="USBBackup"
BIN_NAME="usb-tray-sync"
DIST_DIR="${PROJECT_ROOT}/dist/macos"
APP_BUNDLE="${DIST_DIR}/${APP_NAME}.app"
DMG_PATH="${DIST_DIR}/${APP_NAME}.dmg"
STAGING_DIR="${DIST_DIR}/dmg-staging"

echo "==> [1/4] Building optimized release binary (${BIN_NAME})..."
cd "${PROJECT_ROOT}"
cargo build --release

echo "==> [2/4] Constructing ${APP_NAME}.app bundle..."
rm -rf "${APP_BUNDLE}" "${STAGING_DIR}" "${DMG_PATH}"
mkdir -p "${APP_BUNDLE}/Contents/MacOS"
mkdir -p "${APP_BUNDLE}/Contents/Resources"

cp "${PROJECT_ROOT}/target/release/${BIN_NAME}" "${APP_BUNDLE}/Contents/MacOS/${BIN_NAME}"
chmod +x "${APP_BUNDLE}/Contents/MacOS/${BIN_NAME}"
cp "${SCRIPT_DIR}/Info.plist" "${APP_BUNDLE}/Contents/Info.plist"
echo -n "APPL????" > "${APP_BUNDLE}/Contents/PkgInfo"

# Ad-hoc sign the app bundle so macOS Gatekeeper / Notification Center recognizes the bundle identity
if command -v codesign >/dev/null 2>&1; then
    echo "==> [3/4] Applying ad-hoc code signature to ${APP_NAME}.app..."
    codesign --force --deep --sign - "${APP_BUNDLE}"
fi

echo "==> [4/4] Packaging ${APP_NAME}.app into ${DMG_PATH}..."
mkdir -p "${STAGING_DIR}"
cp -R "${APP_BUNDLE}" "${STAGING_DIR}/"
ln -s /Applications "${STAGING_DIR}/Applications"

if command -v create-dmg >/dev/null 2>&1; then
    create-dmg \
        --volname "${APP_NAME} Installer" \
        --window-pos 200 120 \
        --window-size 600 360 \
        --icon-size 100 \
        --icon "${APP_NAME}.app" 160 170 \
        --hide-extension "${APP_NAME}.app" \
        --app-drop-link 440 170 \
        "${DMG_PATH}" \
        "${STAGING_DIR}" || hdiutil create -volname "${APP_NAME}" -srcfolder "${STAGING_DIR}" -ov -format UDZO "${DMG_PATH}"
else
    hdiutil create -volname "${APP_NAME}" -srcfolder "${STAGING_DIR}" -ov -format UDZO "${DMG_PATH}"
fi

rm -rf "${STAGING_DIR}"
echo ""
echo "Success!"
echo "  App Bundle: ${APP_BUNDLE}"
echo "  DMG Image : ${DMG_PATH}"
