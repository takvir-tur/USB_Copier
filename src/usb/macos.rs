use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionaryRef;
use core_foundation::runloop::{CFRunLoopGetCurrent, CFRunLoopRun, kCFRunLoopDefaultMode};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::{CFURL, CFURLRef};
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::base::{CFAllocatorRef, CFRelease, kCFAllocatorDefault};
use core_foundation_sys::dictionary::CFDictionaryGetValue;
use core_foundation_sys::runloop::CFRunLoopRef;
use log::{info, warn};

use crate::copier::is_system_disk;
use crate::usb::{UsbDriveInfo, UsbEvent};

type DASessionRef = *const c_void;
type DADiskRef = *const c_void;
type DADissenterRef = *const c_void;

type DADiskAppearedCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void);
type DADiskDescriptionChangedCallback =
    extern "C" fn(disk: DADiskRef, keys: CFArrayRef, context: *mut c_void);
type DADiskDisappearedCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void);
type DADiskUnmountCallback =
    extern "C" fn(disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void);
type DADiskEjectCallback =
    extern "C" fn(disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void);

const K_DA_DISK_UNMOUNT_OPTION_DEFAULT: u32 = 0x00000000;
const K_DA_DISK_UNMOUNT_OPTION_WHOLE: u32 = 0x00000001;
const K_DA_DISK_EJECT_OPTION_DEFAULT: u32 = 0x00000000;

#[link(name = "DiskArbitration", kind = "framework")]
unsafe extern "C" {
    static kDADiskDescriptionVolumeMountableKey: CFStringRef;
    static kDADiskDescriptionVolumePathKey: CFStringRef;
    static kDADiskDescriptionVolumeNameKey: CFStringRef;
    static kDADiskDescriptionMediaRemovableKey: CFStringRef;
    static kDADiskDescriptionDeviceInternalKey: CFStringRef;
    static kDADiskDescriptionDeviceProtocolKey: CFStringRef;
    static kDADiskDescriptionDeviceModelKey: CFStringRef;

    fn DASessionCreate(allocator: CFAllocatorRef) -> DASessionRef;
    fn DASessionScheduleWithRunLoop(
        session: DASessionRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn DASessionUnscheduleFromRunLoop(
        session: DASessionRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );

    fn DARegisterDiskAppearedCallback(
        session: DASessionRef,
        match_description: CFDictionaryRef,
        callback: DADiskAppearedCallback,
        context: *mut c_void,
    );
    fn DARegisterDiskDescriptionChangedCallback(
        session: DASessionRef,
        match_description: CFDictionaryRef,
        watch_keys: CFArrayRef,
        callback: DADiskDescriptionChangedCallback,
        context: *mut c_void,
    );
    fn DARegisterDiskDisappearedCallback(
        session: DASessionRef,
        match_description: CFDictionaryRef,
        callback: DADiskDisappearedCallback,
        context: *mut c_void,
    );

    fn DADiskCopyDescription(disk: DADiskRef) -> CFDictionaryRef;
    fn DADiskGetBSDName(disk: DADiskRef) -> *const c_char;
    fn DADiskCreateFromBSDName(
        allocator: CFAllocatorRef,
        session: DASessionRef,
        name: *const c_char,
    ) -> DADiskRef;
    fn DADiskCopyWholeDisk(disk: DADiskRef) -> DADiskRef;
    fn DADiskUnmount(
        disk: DADiskRef,
        options: u32,
        callback: Option<DADiskUnmountCallback>,
        context: *mut c_void,
    );
    fn DADiskEject(
        disk: DADiskRef,
        options: u32,
        callback: Option<DADiskEjectCallback>,
        context: *mut c_void,
    );
}

struct MonitorContext {
    callback: Box<dyn Fn(UsbEvent) + Send + Sync + 'static>,
    /// Maps BSD disk name (e.g. "disk4s1") -> mounted PathBuf so we only emit Connected once per mount
    active_mounts: Mutex<HashMap<String, PathBuf>>,
    /// Tracks whether initial startup scan is complete so pre-mounted drives at app startup don't re-trigger unless newly inserted
    initialized: Mutex<bool>,
}

pub fn start_monitor<F>(callback: F)
where
    F: Fn(UsbEvent) + Send + Sync + 'static,
{
    thread::Builder::new()
        .name("macos-disk-arbitration".into())
        .spawn(move || unsafe {
            let ctx = Arc::new(MonitorContext {
                callback: Box::new(callback),
                active_mounts: Mutex::new(HashMap::new()),
                initialized: Mutex::new(false),
            });

            let session = DASessionCreate(kCFAllocatorDefault);
            if session.is_null() {
                warn!("Failed to create DiskArbitration session");
                return;
            }

            let ctx_ptr = Arc::into_raw(ctx.clone()) as *mut c_void;

            DARegisterDiskAppearedCallback(
                session,
                std::ptr::null(),
                disk_appeared_callback,
                ctx_ptr,
            );
            DARegisterDiskDescriptionChangedCallback(
                session,
                std::ptr::null(),
                std::ptr::null(),
                disk_description_changed_callback,
                ctx_ptr,
            );
            DARegisterDiskDisappearedCallback(
                session,
                std::ptr::null(),
                disk_disappeared_callback,
                ctx_ptr,
            );

            let run_loop = CFRunLoopGetCurrent();
            DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);

            // Mark initialized after initial DiskArbitration enumeration burst (500ms)
            let init_ctx = ctx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(600));
                if let Ok(mut init) = init_ctx.initialized.lock() {
                    *init = true;
                    info!("macOS DiskArbitration USB monitor armed and listening for new drives.");
                }
            });

            CFRunLoopRun();

            DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
            CFRelease(session);
        })
        .expect("Failed to spawn macOS DiskArbitration thread");
}

extern "C" fn disk_appeared_callback(disk: DADiskRef, context: *mut c_void) {
    unsafe {
        if !disk.is_null() && !context.is_null() {
            let ctx = &*(context as *const MonitorContext);
            inspect_and_handle_disk(disk, ctx);
        }
    }
}

extern "C" fn disk_description_changed_callback(
    disk: DADiskRef,
    _keys: CFArrayRef,
    context: *mut c_void,
) {
    unsafe {
        if !disk.is_null() && !context.is_null() {
            let ctx = &*(context as *const MonitorContext);
            inspect_and_handle_disk(disk, ctx);
        }
    }
}

extern "C" fn disk_disappeared_callback(disk: DADiskRef, context: *mut c_void) {
    unsafe {
        if disk.is_null() || context.is_null() {
            return;
        }
        let ctx = &*(context as *const MonitorContext);
        let bsd_ptr = DADiskGetBSDName(disk);
        if bsd_ptr.is_null() {
            return;
        }
        let bsd_name = CStr::from_ptr(bsd_ptr).to_string_lossy().to_string();
        if let Ok(mut map) = ctx.active_mounts.lock()
            && let Some(mount_point) = map.remove(&bsd_name)
        {
            info!(
                "USB volume disappeared: {} ({})",
                bsd_name,
                mount_point.display()
            );
            (ctx.callback)(UsbEvent::Disconnected(mount_point));
        }
    }
}

unsafe fn inspect_and_handle_disk(disk: DADiskRef, ctx: &MonitorContext) {
    unsafe {
        let bsd_ptr = DADiskGetBSDName(disk);
        if bsd_ptr.is_null() {
            return;
        }
        let bsd_name = CStr::from_ptr(bsd_ptr).to_string_lossy().to_string();

        let desc = DADiskCopyDescription(disk);
        if desc.is_null() {
            return;
        }

        let result = parse_usb_disk_description(desc, &bsd_name);
        CFRelease(desc as CFTypeRef);

        if let Some(drive_info) = result {
            let mut map = match ctx.active_mounts.lock() {
                Ok(g) => g,
                Err(_) => return,
            };

            if map.contains_key(&bsd_name) {
                return;
            }

            let is_initialized = ctx.initialized.lock().map(|g| *g).unwrap_or(true);
            map.insert(bsd_name.clone(), drive_info.mount_point.clone());
            drop(map);

            if is_initialized {
                info!(
                    "Detected newly mounted USB drive: '{}' at {} ({})",
                    drive_info.name,
                    drive_info.mount_point.display(),
                    drive_info.device_id
                );
                (ctx.callback)(UsbEvent::Connected(drive_info));
            } else {
                info!(
                    "Registered pre-existing removable volume at startup: '{}' at {}",
                    drive_info.name,
                    drive_info.mount_point.display()
                );
            }
        }
    }
}

unsafe fn parse_usb_disk_description(
    desc: CFDictionaryRef,
    bsd_name: &str,
) -> Option<UsbDriveInfo> {
    unsafe {
        // 1. Must be mountable
        let mountable = dict_get_bool(desc, kDADiskDescriptionVolumeMountableKey).unwrap_or(false);
        if !mountable {
            return None;
        }

        // 2. Check if removable or external USB device
        let is_removable =
            dict_get_bool(desc, kDADiskDescriptionMediaRemovableKey).unwrap_or(false);
        let is_internal = dict_get_bool(desc, kDADiskDescriptionDeviceInternalKey).unwrap_or(true);
        let protocol = dict_get_string(desc, kDADiskDescriptionDeviceProtocolKey)
            .unwrap_or_default()
            .to_uppercase();
        let model = dict_get_string(desc, kDADiskDescriptionDeviceModelKey).unwrap_or_default();

        // Ignore mounted .dmg Disk Images
        if model.eq_ignore_ascii_case("Disk Image") {
            return None;
        }

        let is_usb_or_removable = is_removable || protocol == "USB" || !is_internal;
        if !is_usb_or_removable {
            return None;
        }

        // 3. Must have a mounted volume path (e.g., file:///Volumes/MY_USB/)
        let volume_url_ptr = CFDictionaryGetValue(desc, kDADiskDescriptionVolumePathKey as *const _);
        if volume_url_ptr.is_null() {
            return None;
        }
        let volume_url = CFURL::wrap_under_get_rule(volume_url_ptr as CFURLRef);
        let mount_point: PathBuf = volume_url.to_path()?;

        if !mount_point.starts_with("/Volumes/") || is_system_disk(&mount_point) {
            return None;
        }

        let volume_name = dict_get_string(desc, kDADiskDescriptionVolumeNameKey).unwrap_or_else(|| {
            mount_point
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "USB Drive".to_string())
        });

        Some(UsbDriveInfo {
            name: volume_name,
            mount_point,
            device_id: bsd_name.to_string(),
        })
    }
}

unsafe fn dict_get_bool(dict: CFDictionaryRef, key: CFStringRef) -> Option<bool> {
    unsafe {
        let val = CFDictionaryGetValue(dict, key as *const _);
        if val.is_null() {
            return None;
        }
        let cf_bool = CFBoolean::wrap_under_get_rule(val as *const _);
        Some(cf_bool == CFBoolean::true_value())
    }
}

unsafe fn dict_get_string(dict: CFDictionaryRef, key: CFStringRef) -> Option<String> {
    unsafe {
        let val = CFDictionaryGetValue(dict, key as *const _);
        if val.is_null() {
            return None;
        }
        let cf_str = CFString::wrap_under_get_rule(val as CFStringRef);
        Some(cf_str.to_string())
    }
}

/// Flushes pending disk writes via `libc::sync()` and unmounts/ejects the USB drive on macOS.
pub fn flush_and_eject(drive: &UsbDriveInfo) -> Result<()> {
    info!(
        "Flushing filesystem buffers prior to ejecting {} ({})...",
        drive.name, drive.device_id
    );
    unsafe {
        libc::sync();
    }

    // Brief pause to allow Spotlight/Finder handles to settle after sync
    thread::sleep(Duration::from_millis(500));

    // 1. Attempt native DiskArbitration unmount + whole-disk eject
    if let Ok(()) = try_disk_arbitration_eject(&drive.device_id) {
        info!(
            "Successfully unmounted and ejected {} via DiskArbitration",
            drive.device_id
        );
        return Ok(());
    }

    // 2. Fallback to `diskutil eject` / `diskutil unmountDisk`
    info!(
        "Falling back to diskutil eject for {} ({})...",
        drive.device_id,
        drive.mount_point.display()
    );

    let eject_status = Command::new("/usr/sbin/diskutil")
        .args(["eject", &drive.device_id])
        .output();

    if let Ok(output) = eject_status
        && output.status.success()
    {
        info!("diskutil eject succeeded for {}", drive.device_id);
        return Ok(());
    }

    let unmount_status = Command::new("/usr/sbin/diskutil")
        .args(["unmountDisk", &drive.device_id])
        .output()?;

    if unmount_status.status.success() {
        info!("diskutil unmountDisk succeeded for {}", drive.device_id);
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&unmount_status.stderr);
    bail!(
        "Failed to eject USB drive {} ({}): {}",
        drive.name,
        drive.device_id,
        stderr.trim()
    )
}

extern "C" fn da_op_callback(
    _disk: DADiskRef,
    dissenter: DADissenterRef,
    context: *mut c_void,
) {
    unsafe {
        if !context.is_null() {
            let result_ptr = context as *mut Option<bool>;
            *result_ptr = Some(dissenter.is_null());
        }
        core_foundation_sys::runloop::CFRunLoopStop(CFRunLoopGetCurrent());
    }
}

fn try_disk_arbitration_eject(bsd_name: &str) -> Result<()> {
    unsafe {
        let session = DASessionCreate(kCFAllocatorDefault);
        if session.is_null() {
            bail!("Could not create DASession for eject");
        }

        let run_loop = CFRunLoopGetCurrent();
        DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);

        let c_bsd = CString::new(bsd_name)?;
        let disk = DADiskCreateFromBSDName(kCFAllocatorDefault, session, c_bsd.as_ptr());
        if disk.is_null() {
            DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
            CFRelease(session);
            bail!("Could not create DADisk from BSD name {}", bsd_name);
        }

        let whole_disk = DADiskCopyWholeDisk(disk);
        let target_disk = if !whole_disk.is_null() {
            whole_disk
        } else {
            disk
        };

        // Step 1: Unmount whole disk
        let mut unmount_ok: Option<bool> = None;
        DADiskUnmount(
            target_disk,
            K_DA_DISK_UNMOUNT_OPTION_WHOLE | K_DA_DISK_UNMOUNT_OPTION_DEFAULT,
            Some(da_op_callback),
            &mut unmount_ok as *mut _ as *mut c_void,
        );
        CFRunLoopRun();

        if unmount_ok != Some(true) {
            if !whole_disk.is_null() {
                CFRelease(whole_disk);
            }
            CFRelease(disk);
            DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
            CFRelease(session);
            bail!("DADiskUnmount returned a dissenter for {}", bsd_name);
        }

        // Step 2: Eject whole disk
        let mut eject_ok: Option<bool> = None;
        DADiskEject(
            target_disk,
            K_DA_DISK_EJECT_OPTION_DEFAULT,
            Some(da_op_callback),
            &mut eject_ok as *mut _ as *mut c_void,
        );
        CFRunLoopRun();

        if !whole_disk.is_null() {
            CFRelease(whole_disk);
        }
        CFRelease(disk);
        DASessionUnscheduleFromRunLoop(session, run_loop, kCFRunLoopDefaultMode);
        CFRelease(session);

        if eject_ok == Some(true) {
            Ok(())
        } else {
            bail!("DADiskEject returned a dissenter for {}", bsd_name)
        }
    }
}
