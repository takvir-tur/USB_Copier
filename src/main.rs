#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod copier;
mod notifier;
mod usb;

use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use config::AppConfig;
use copier::BackupStats;
use log::{error, info, warn};
use notifier::send_notification;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(target_os = "macos")]
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use usb::{UsbDriveInfo, UsbEvent};

enum AppEvent {
    Menu(MenuEvent),
    Usb(UsbEvent),
    BackupStarted {
        drive: UsbDriveInfo,
    },
    BackupSucceeded {
        drive: UsbDriveInfo,
        stats: BackupStats,
        ejected: Result<(), String>,
    },
    BackupFailed {
        drive: UsbDriveInfo,
        error: String,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!("Starting USBBackup background tray utility...");

    let config = Arc::new(Mutex::new(AppConfig::load()));

    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    // Hide Dock icon on macOS so the app runs purely in the Menu Bar / System Tray
    #[cfg(target_os = "macos")]
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let proxy = event_loop.create_proxy();

    // Forward tray menu clicks into the main event loop
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(AppEvent::Menu(event));
    }));

    // Build system tray menu matching functional requirements:
    // - "Status: Idle / Copying..."
    // - "Set Backup Destination"
    // - "Open Destination Folder"
    // - "Quit"
    let tray_menu = Menu::new();
    let status_item = MenuItem::new("Status: Idle", false, None);
    let set_dest_item = MenuItem::new("Set Backup Destination", true, None);
    let open_dest_item = MenuItem::new("Open Destination Folder", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit_item = MenuItem::new("Quit", true, None);

    tray_menu.append(&status_item)?;
    tray_menu.append(&separator)?;
    tray_menu.append(&set_dest_item)?;
    tray_menu.append(&open_dest_item)?;
    tray_menu.append(&PredefinedMenuItem::separator())?;
    tray_menu.append(&quit_item)?;

    let set_dest_id = set_dest_item.id().clone();
    let open_dest_id = open_dest_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Start OS-level USB volume listener (DiskArbitration on macOS, WM_DEVICECHANGE on Windows)
    let usb_proxy = proxy.clone();
    usb::start_usb_monitor(move |usb_event| {
        let _ = usb_proxy.send_event(AppEvent::Usb(usb_event));
    });

    let mut tray_icon = None;
    let worker_proxy = proxy.clone();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                let icon = create_status_icon(false);
                let initial_dest = config
                    .lock()
                    .map(|c| c.destination_path.display().to_string())
                    .unwrap_or_default();
                let tooltip = format!("USB Backup - Status: Idle\nDestination: {}", initial_dest);

                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_menu(Box::new(tray_menu.clone()))
                        .with_tooltip(&tooltip)
                        .with_icon(icon)
                        .build()
                        .expect("Failed to create system tray icon"),
                );
            }

            Event::UserEvent(AppEvent::Usb(UsbEvent::Connected(drive))) => {
                let dest_path = match config.lock() {
                    Ok(cfg) => cfg.destination_path.clone(),
                    Err(_) => return,
                };

                let bg_proxy = worker_proxy.clone();
                let _ = bg_proxy.send_event(AppEvent::BackupStarted {
                    drive: drive.clone(),
                });

                thread::spawn(move || {
                    match copier::backup_usb_drive(&drive.mount_point, &dest_path) {
                        Ok(stats) => {
                            let eject_res = usb::flush_and_eject_drive(&drive)
                                .map_err(|e| e.to_string());
                            let _ = bg_proxy.send_event(AppEvent::BackupSucceeded {
                                drive,
                                stats,
                                ejected: eject_res,
                            });
                        }
                        Err(err) => {
                            let _ = bg_proxy.send_event(AppEvent::BackupFailed {
                                drive,
                                error: err.to_string(),
                            });
                        }
                    }
                });
            }

            Event::UserEvent(AppEvent::Usb(UsbEvent::Disconnected(mount_point))) => {
                info!("USB drive unmounted/removed: {}", mount_point.display());
            }

            Event::UserEvent(AppEvent::BackupStarted { drive }) => {
                status_item.set_text(format!("Status: Copying ({})...", drive.name));
                if let Some(ref tray) = tray_icon {
                    let _ = tray.set_icon(Some(create_status_icon(true)));
                    let _ = tray.set_tooltip(Some(format!(
                        "USB Backup - Status: Copying {}...",
                        drive.name
                    )));
                }
                send_notification(
                    "USB Backup Started",
                    &format!(
                        "Copying '{}' ({}) to backup folder...",
                        drive.name,
                        drive.mount_point.display()
                    ),
                );
            }

            Event::UserEvent(AppEvent::BackupSucceeded {
                drive,
                stats,
                ejected,
            }) => {
                status_item.set_text("Status: Idle");
                if let Some(ref tray) = tray_icon {
                    let _ = tray.set_icon(Some(create_status_icon(false)));
                    let _ = tray.set_tooltip(Some(format!(
                        "USB Backup - Status: Idle\nLast backup: {} files from {}",
                        stats.files_copied, drive.name
                    )));
                }

                match ejected {
                    Ok(()) => {
                        info!(
                            "Backup of '{}' completed ({} files) and drive safely ejected.",
                            drive.name, stats.files_copied
                        );
                        send_notification(
                            "USB Backup",
                            "Copy complete. USB drive safely ejected.",
                        );
                    }
                    Err(eject_err) => {
                        warn!(
                            "Backup of '{}' succeeded ({} files), but auto-eject failed: {}",
                            drive.name, stats.files_copied, eject_err
                        );
                        send_notification(
                            "USB Backup Complete (Eject Warning)",
                            &format!(
                                "Copied {} files to {}, but drive could not be auto-ejected: {}",
                                stats.files_copied,
                                stats.backup_folder.display(),
                                eject_err
                            ),
                        );
                    }
                }
            }

            Event::UserEvent(AppEvent::BackupFailed { drive, error }) => {
                error!("Backup failed for '{}': {}", drive.name, error);
                status_item.set_text("Status: Idle");
                if let Some(ref tray) = tray_icon {
                    let _ = tray.set_icon(Some(create_status_icon(false)));
                    let _ = tray.set_tooltip(Some(format!(
                        "USB Backup - Status: Idle (Last error: {})",
                        drive.name
                    )));
                }
                send_notification(
                    "USB Backup Failed",
                    &format!("Failed to copy '{}': {}", drive.name, error),
                );
            }

            Event::UserEvent(AppEvent::Menu(menu_event)) => {
                if menu_event.id == set_dest_id {
                    let current_dir = config
                        .lock()
                        .ok()
                        .map(|c| c.destination_path.clone())
                        .unwrap_or_default();

                    let picked = rfd::FileDialog::new()
                        .set_title("Select USB Backup Destination Folder")
                        .set_directory(&current_dir)
                        .pick_folder();

                    if let Some(new_dest) = picked {
                        if let Ok(mut cfg) = config.lock() {
                            cfg.destination_path = new_dest.clone();
                            if let Err(err) = cfg.save() {
                                error!("Failed to save config: {}", err);
                            }
                        }
                        if let Some(ref tray) = tray_icon {
                            let _ = tray.set_tooltip(Some(format!(
                                "USB Backup - Status: Idle\nDestination: {}",
                                new_dest.display()
                            )));
                        }
                        send_notification(
                            "USB Backup Destination Updated",
                            &format!("New backups will be saved to: {}", new_dest.display()),
                        );
                    }
                } else if menu_event.id == open_dest_id {
                    if let Ok(cfg) = config.lock() {
                        let dest = cfg.destination_path.clone();
                        let _ = fs::create_dir_all(&dest);
                        if let Err(err) = open::that(&dest) {
                            error!(
                                "Failed to open destination folder {}: {}",
                                dest.display(),
                                err
                            );
                        }
                    }
                } else if menu_event.id == quit_id {
                    info!("Quit requested from system tray menu.");
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                }
            }

            _ => {}
        }
    });
}

/// Generates a crisp 32x32 RGBA system tray icon.
/// - `is_copying == false`: DodgerBlue USB connector icon (Idle)
/// - `is_copying == true`: EmeraldGreen active sync icon (Copying...)
fn create_status_icon(is_copying: bool) -> Icon {
    let width = 32u32;
    let height = 32u32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    let primary: [u8; 4] = if is_copying {
        [16, 185, 129, 255] // Emerald Green (Copying)
    } else {
        [37, 99, 235, 255] // Royal Blue (Idle)
    };
    let inner: [u8; 4] = [255, 255, 255, 255];
    let metal: [u8; 4] = [203, 213, 225, 255];

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;

            // USB metal plug top (x: 10..22, y: 3..10)
            let in_plug = (10..22).contains(&x) && (3..10).contains(&y);
            let in_plug_pins =
                ((12..15).contains(&x) || (17..20).contains(&x)) && (5..8).contains(&y);

            // USB body (x: 7..25, y: 10..28)
            let in_body = (7..25).contains(&x) && (10..28).contains(&y);
            let in_body_highlight = (11..21).contains(&x) && (14..24).contains(&y);

            if in_plug_pins {
                rgba[idx..idx + 4].copy_from_slice(&primary);
            } else if in_plug {
                rgba[idx..idx + 4].copy_from_slice(&metal);
            } else if in_body_highlight {
                rgba[idx..idx + 4].copy_from_slice(&inner);
            } else if in_body {
                rgba[idx..idx + 4].copy_from_slice(&primary);
            }
        }
    }

    Icon::from_rgba(rgba, width, height).expect("Failed to build RGBA tray icon")
}
