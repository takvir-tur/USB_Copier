use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use log::{info, warn};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    PNP_VetoTypeUnknown, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT,
    WPARAM,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DRIVE_REMOVABLE, FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers,
    GetDriveTypeW, GetVolumeInformationW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_EJECT_MEDIA,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    STORAGE_DEVICE_NUMBER,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_VOLUME,
    DEV_BROADCAST_HDR, DEV_BROADCAST_VOLUME, DefWindowProcW, DispatchMessageW, GetMessageW,
    HWND_MESSAGE, MSG, RegisterClassW, TranslateMessage, WM_DEVICECHANGE, WNDCLASSW,
};

use crate::copier::is_system_disk;
use crate::usb::{UsbDriveInfo, UsbEvent};

static USB_CALLBACK: OnceLock<Box<dyn Fn(UsbEvent) + Send + Sync + 'static>> = OnceLock::new();

pub fn start_monitor<F>(callback: F)
where
    F: Fn(UsbEvent) + Send + Sync + 'static,
{
    let _ = USB_CALLBACK.set(Box::new(callback));

    thread::Builder::new()
        .name("windows-wm-devicechange".into())
        .spawn(move || unsafe {
            let class_name: Vec<u16> = "USBBackupDeviceChangeListener\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(std::ptr::null());

            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(device_window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };

            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );

            if hwnd.is_null() {
                warn!("Failed to create message-only window for WM_DEVICECHANGE");
                return;
            }

            info!("Windows WM_DEVICECHANGE USB monitor listening on message-only window.");

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .expect("Failed to spawn Windows WM_DEVICECHANGE thread");
}

unsafe extern "system" fn device_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_DEVICECHANGE && lparam != 0 {
            let event_type = wparam as u32;
            let hdr = &*(lparam as *const DEV_BROADCAST_HDR);

            if hdr.dbch_devicetype == DBT_DEVTYP_VOLUME {
                let vol = &*(lparam as *const DEV_BROADCAST_VOLUME);
                let unit_mask = vol.dbcv_unitmask;

                for bit in 0..26u32 {
                    if (unit_mask & (1 << bit)) != 0 {
                        let drive_letter = (b'A' + bit as u8) as char;
                        let root_path_str = format!("{}:\\", drive_letter);
                        let mount_point = PathBuf::from(&root_path_str);

                        if event_type == DBT_DEVICEARRIVAL {
                            if let Some(drive_info) =
                                inspect_windows_drive(drive_letter, &mount_point)
                                && let Some(cb) = USB_CALLBACK.get()
                            {
                                cb(UsbEvent::Connected(drive_info));
                            }
                        } else if event_type == DBT_DEVICEREMOVECOMPLETE
                            && let Some(cb) = USB_CALLBACK.get()
                        {
                            cb(UsbEvent::Disconnected(mount_point));
                        }
                    }
                }
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn inspect_windows_drive(drive_letter: char, mount_point: &PathBuf) -> Option<UsbDriveInfo> {
    unsafe {
        let root_wide: Vec<u16> = format!("{}:\\\0", drive_letter).encode_utf16().collect();
        let drive_type = GetDriveTypeW(root_wide.as_ptr());

        if drive_type != DRIVE_REMOVABLE || is_system_disk(mount_point) {
            return None;
        }

        let mut vol_name_buf = [0u16; 261];
        let ok = GetVolumeInformationW(
            root_wide.as_ptr(),
            vol_name_buf.as_mut_ptr(),
            vol_name_buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        );

        let name = if ok != 0 {
            let len = vol_name_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(vol_name_buf.len());
            let label = std::ffi::OsString::from_wide(&vol_name_buf[..len])
                .to_string_lossy()
                .to_string();
            if label.trim().is_empty() {
                format!("USB Drive ({}:)", drive_letter)
            } else {
                label
            }
        } else {
            format!("USB Drive ({}:)", drive_letter)
        };

        Some(UsbDriveInfo {
            name,
            mount_point: mount_point.clone(),
            device_id: format!("{}:", drive_letter),
        })
    }
}

/// Flushes volume buffers and safely ejects the USB volume on Windows using
/// `FSCTL_LOCK_VOLUME` -> `FSCTL_DISMOUNT_VOLUME` -> `IOCTL_STORAGE_EJECT_MEDIA`
/// and `CM_Request_Device_EjectW`.
pub fn flush_and_eject(drive: &UsbDriveInfo) -> Result<()> {
    unsafe {
        let device_path = format!("\\\\.\\{}", drive.device_id.trim_end_matches('\\'));
        let wide_path: Vec<u16> = std::ffi::OsStr::new(&device_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let handle: HANDLE = CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        if handle == INVALID_HANDLE_VALUE {
            bail!("Failed to open volume handle {}", device_path);
        }

        // Flush pending writes on the volume
        let _ = FlushFileBuffers(handle);

        let mut bytes_returned: u32 = 0;

        // Query physical device number for CM_Request_Device_EjectW
        let mut dev_num: STORAGE_DEVICE_NUMBER = std::mem::zeroed();
        let has_dev_num = DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            std::ptr::null(),
            0,
            &mut dev_num as *mut _ as *mut _,
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        ) != 0;

        // 1. Lock the volume (retry up to 5 times in case Explorer/Indexer briefly holds a handle)
        let mut locked = false;
        for _ in 0..5 {
            let res = DeviceIoControl(
                handle,
                FSCTL_LOCK_VOLUME,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut bytes_returned,
                std::ptr::null_mut(),
            );
            if res != 0 {
                locked = true;
                break;
            }
            thread::sleep(Duration::from_millis(400));
        }

        if !locked {
            warn!(
                "Could not immediately lock volume {}, attempting dismount anyway",
                device_path
            );
        }

        // 2. Dismount the volume
        let dismount_res = DeviceIoControl(
            handle,
            FSCTL_DISMOUNT_VOLUME,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        );

        // 3. Allow media removal
        let pmr = PREVENT_MEDIA_REMOVAL {
            PreventMediaRemoval: 0,
        };
        let _ = DeviceIoControl(
            handle,
            IOCTL_STORAGE_MEDIA_REMOVAL,
            &pmr as *const _ as *const _,
            std::mem::size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        );

        // 4. Eject media via IOCTL_STORAGE_EJECT_MEDIA
        let eject_res = DeviceIoControl(
            handle,
            IOCTL_STORAGE_EJECT_MEDIA,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        );

        CloseHandle(handle);

        // 5. Also attempt CM_Request_Device_EjectW on the parent USB device node if available
        if has_dev_num && try_cm_eject_by_device_number(dev_num.DeviceNumber) {
            info!(
                "Successfully ejected USB device via CM_Request_Device_EjectW ({})",
                device_path
            );
            return Ok(());
        }

        if dismount_res == 0 || eject_res == 0 {
            bail!(
                "IOCTL_STORAGE_EJECT_MEDIA / dismount failed for {}",
                device_path
            );
        }

        info!(
            "Successfully ejected Windows USB volume {} via IOCTL_STORAGE_EJECT_MEDIA",
            device_path
        );
        Ok(())
    }
}

unsafe fn try_cm_eject_by_device_number(target_device_number: u32) -> bool {
    unsafe {
        let h_dev_info = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if h_dev_info == INVALID_HANDLE_VALUE {
            return false;
        }

        let mut index = 0;
        let mut ejected = false;

        loop {
            let mut iface_data: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface_data.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

            if SetupDiEnumDeviceInterfaces(
                h_dev_info,
                std::ptr::null(),
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut iface_data,
            ) == 0
            {
                break;
            }
            index += 1;

            let mut required_size: u32 = 0;
            SetupDiGetDeviceInterfaceDetailW(
                h_dev_info,
                &iface_data,
                std::ptr::null_mut(),
                0,
                &mut required_size,
                std::ptr::null_mut(),
            );
            if required_size == 0 {
                continue;
            }

            let mut detail_buf = vec![0u8; required_size as usize];
            let detail_ptr = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail_ptr).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            let mut dev_info_data: SP_DEVINFO_DATA = std::mem::zeroed();
            dev_info_data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

            if SetupDiGetDeviceInterfaceDetailW(
                h_dev_info,
                &iface_data,
                detail_ptr,
                required_size,
                std::ptr::null_mut(),
                &mut dev_info_data,
            ) == 0
            {
                continue;
            }

            let dev_path_ptr = (*detail_ptr).DevicePath.as_ptr();
            let h_disk = CreateFileW(
                dev_path_ptr,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if h_disk == INVALID_HANDLE_VALUE {
                continue;
            }

            let mut dev_num: STORAGE_DEVICE_NUMBER = std::mem::zeroed();
            let mut bytes_ret = 0;
            let ok = DeviceIoControl(
                h_disk,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null(),
                0,
                &mut dev_num as *mut _ as *mut _,
                std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                &mut bytes_ret,
                std::ptr::null_mut(),
            );
            CloseHandle(h_disk);

            if ok != 0 && dev_num.DeviceNumber == target_device_number {
                let mut parent_inst: u32 = 0;
                if CM_Get_Parent(&mut parent_inst, dev_info_data.DevInst, 0) == CR_SUCCESS {
                    let mut veto_type = PNP_VetoTypeUnknown;
                    let mut veto_name = [0u16; 260];
                    if CM_Request_Device_EjectW(
                        parent_inst,
                        &mut veto_type,
                        veto_name.as_mut_ptr(),
                        veto_name.len() as u32,
                        0,
                    ) == CR_SUCCESS
                    {
                        ejected = true;
                    }
                }
                break;
            }
        }

        SetupDiDestroyDeviceInfoList(h_dev_info);
        ejected
    }
}
