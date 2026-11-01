use std::path::PathBuf;

use anyhow::Result;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

/// Metadata describing a mounted removable USB drive.
#[derive(Debug, Clone)]
pub struct UsbDriveInfo {
    /// Human-readable volume name (e.g., "KINGSTON" or "USB_DISK").
    pub name: String,
    /// Mounted filesystem path (e.g., `/Volumes/KINGSTON` on macOS or `E:\` on Windows).
    pub mount_point: PathBuf,
    /// OS device identifier (e.g., `disk4s1` on macOS or `E:` on Windows).
    pub device_id: String,
}

/// OS-level USB volume lifecycle events.
#[derive(Debug, Clone)]
pub enum UsbEvent {
    Connected(UsbDriveInfo),
    Disconnected(PathBuf),
}

/// Starts a background OS event listener for USB drive arrival and removal events.
/// Uses `DiskArbitration` on macOS and `WM_DEVICECHANGE` on Windows for near-zero idle CPU usage.
pub fn start_usb_monitor<F>(callback: F)
where
    F: Fn(UsbEvent) + Send + Sync + 'static,
{
    #[cfg(target_os = "macos")]
    macos::start_monitor(callback);

    #[cfg(target_os = "windows")]
    windows::start_monitor(callback);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = callback;
        log::warn!("Native USB event monitoring is only implemented for macOS and Windows.");
    }
}

/// Flushes pending disk writes and safely unmounts/ejects the USB drive.
pub fn flush_and_eject_drive(drive: &UsbDriveInfo) -> Result<()> {
    #[cfg(target_os = "macos")]
    return macos::flush_and_eject(drive);

    #[cfg(target_os = "windows")]
    return windows::flush_and_eject(drive);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = drive;
        anyhow::bail!("Auto-eject is only supported on macOS and Windows")
    }
}
