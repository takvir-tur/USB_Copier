# USB Copier (`usb-tray-sync`)

[![Rust](https://img.shields.io/badge/Language-Rust-orange?logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS-blue)](#installation--build-instructions)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](https://opensource.org/licenses/MIT)

## Overview

A lightweight, silent background utility built in Rust that monitors USB drive insertions, automatically mirrors the entire contents into a dated target folder, and safely ejects the drive upon completion.

---

## Key Features

- **Near-Zero Idle Resource Consumption**: Uses event-driven OS notifications via `IOKit` / `DiskArbitration` on macOS and `WM_DEVICECHANGE` on Windows rather than high-frequency polling.
- **System Tray / Menu Bar Integration**: Runs quietly in the background with live status indicators (`Status: Idle` / `Status: Copying...`) and dynamic tray icon/tooltip updates.
- **Native Folder Picker & Persistent Settings**: Allows selecting any backup destination directory via a native OS folder dialog (`rfd`) and persists configuration across reboots in the OS standard AppData / Application Support directory (`directories`).
- **Comprehensive Recursive Copy**: Mirrors all files, subdirectories, hidden dotfiles, and symbolic links into a timestamped folder (`USB_Backup_YYYY-MM-DD_HH-mm-ss`) while verifying the source is not a system disk and handling read errors gracefully.
- **Automatic Safe Drive Unmounting / Ejection**: Flushes pending filesystem buffers and safely unmounts/ejects the USB volume (`DiskArbitration` / `diskutil` on macOS; `FSCTL_LOCK_VOLUME`, `FSCTL_DISMOUNT_VOLUME`, `IOCTL_STORAGE_EJECT_MEDIA`, and `CM_Request_Device_EjectW` on Windows).
- **Native Desktop Notifications**: Alerts the user upon backup start, copy completion, safe ejection, or any errors.

---

## Architecture & Tech Stack

### Tech Stack
- **Core Language**: Rust (2024 Edition)
- **System Tray & Event Loop**: `tray-icon` and `tao`
- **Native OS Bindings**:
  - **macOS (`#[cfg(target_os = "macos")]`)**: `core-foundation`, `core-foundation-sys`, `libc`, and direct FFI bindings to `DiskArbitration.framework` (`DARegisterDiskAppearedCallback`, `DARegisterDiskDescriptionChangedCallback`, `DARegisterDiskDisappearedCallback`, `DADiskUnmount`, `DADiskEject`).
  - **Windows (`#[cfg(target_os = "windows")]`)**: `windows-sys` (`WM_DEVICECHANGE`, `DBT_DEVICEARRIVAL`, `GetDriveTypeW`, `DeviceIoControl`, `IOCTL_STORAGE_EJECT_MEDIA`, `CM_Request_Device_EjectW`).
- **Configuration & Dialogs**: `rfd`, `directories`, `serde`, `serde_json`, `chrono`, `open`

### Repository Layout
```text
usb-tray-sync/
├── Cargo.toml                        # Dependencies, release size optimizations, cargo-bundle metadata
├── src/
│   ├── main.rs                       # System tray UI (`tray-icon` + `tao`) and background event orchestration
│   ├── config.rs                     # Persistent JSON settings in OS AppData / Application Support
│   ├── copier.rs                     # System-disk guard & recursive copy (hidden files, symlinks, error handling)
│   ├── notifier.rs                   # Native desktop notifications (macOS Notification Center / Windows Toast)
│   └── usb/
│       ├── mod.rs                    # Cross-platform USB detection and ejection interface
│       ├── macos.rs                  # macOS `DiskArbitration` callbacks & safe unmount/eject
│       └── windows.rs                # Windows `WM_DEVICECHANGE` listener & native volume/device eject
├── packaging/
│   ├── macos/
│   │   ├── Info.plist                # macOS App Bundle configuration (`LSUIElement = true`)
│   │   └── build-macos-dmg.sh        # Builds `USBBackup.app` and packages `USBBackup.dmg`
│   └── windows/
│       ├── installer.iss             # Inno Setup script (`USBBackup-Setup-0.1.0.exe`)
│       └── build-windows.ps1         # Builds standalone `.exe`, Inno Setup `.exe`, and WiX `.msi`
└── wix/
    └── main.wxs                      # WiX Toolset manifest for `cargo wix` (`.msi` installer)
```

### Aggressive Release Optimizations
`Cargo.toml` is configured for minimal binary size and stripped symbols (~1.3 MB standalone binary):
```toml
[profile.release]
opt-level = "z"
lto = true
strip = true
codegen-units = 1
panic = "abort"
```

---

## Installation & Build Instructions

### Prerequisites
- [Rust & Cargo](https://rustup.rs/) (stable toolchain)

### macOS (`USBBackup.app` & `USBBackup.dmg`)
Run the macOS packaging script to compile the stripped release binary, construct the `USBBackup.app` bundle (configured with `LSUIElement = true` so it runs solely in the menu bar without a Dock icon), apply an ad-hoc code signature, and generate an installable `.dmg`:

```bash
chmod +x packaging/macos/build-macos-dmg.sh
./packaging/macos/build-macos-dmg.sh
```

Generated artifacts:
- **App Bundle**: `dist/macos/USBBackup.app`
- **Disk Image Installer**: `dist/macos/USBBackup.dmg`

### Windows (`.exe` & Installer)
On Windows with the `x86_64-pc-windows-msvc` toolchain installed:

1. **Automated Build & Packaging Script**:
   ```powershell
   .\packaging\windows\build-windows.ps1
   ```
2. **Standalone Executable Only**:
   ```powershell
   cargo build --release
   # Output: target\release\usb-tray-sync.exe
   ```
3. **Inno Setup Installer (`USBBackup-Setup-0.1.0.exe`)**:
   ```powershell
   ISCC.exe packaging\windows\installer.iss
   # Output: dist\windows\USBBackup-Setup-0.1.0.exe
   ```
4. **WiX `.msi` Installer (Optional)**:
   ```powershell
   cargo install cargo-wix
   cargo wix --nocapture
   # Output: target\wix\usb-tray-sync-0.1.0-x86_64.msi
   ```

---

## Usage Guide

1. **Launch the Utility**:
   - **macOS**: Open `USBBackup.app` (or run `cargo run --release`). A USB icon appears in the top-right macOS Menu Bar.
   - **Windows**: Launch `USBBackup.exe` (or install via `USBBackup-Setup-0.1.0.exe`). A USB icon appears in the Windows System Tray.
2. **Configure Backup Destination**:
   - Click the tray icon and select **"Set Backup Destination"**.
   - Choose your desired target directory in the native folder picker dialog.
   - Your choice is saved automatically (defaulting to `~/Documents/USB_Backups` if not yet customized) and persists across system restarts.
3. **Automated Backup & Eject Workflow**:
   - Insert any removable USB flash drive.
   - The utility detects the mount event immediately, updates the tray status to **`Status: Copying (<Drive Name>)...`**, and displays a desktop notification.
   - All contents of the USB drive are copied into `[Destination]/USB_Backup_YYYY-MM-DD_HH-mm-ss/`.
   - Once all files are synced to disk, the drive is automatically unmounted/ejected and a final notification confirms: **"Copy complete. USB drive safely ejected."**
4. **View Backups**:
   - Click **"Open Destination Folder"** in the tray menu at any time to inspect your backed-up folders in Finder or File Explorer.

---

## License

Released under the [MIT License](https://opensource.org/licenses/MIT).
