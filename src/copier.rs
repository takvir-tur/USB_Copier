use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Local;
use log::{info, warn};

/// Summary statistics for a completed USB backup operation.
#[derive(Debug, Default, Clone)]
pub struct BackupStats {
    pub backup_folder: PathBuf,
    pub files_copied: u64,
    pub dirs_created: u64,
    pub symlinks_copied: u64,
    pub bytes_copied: u64,
    pub skipped_entries: u64,
}

/// System/OS internal volume metadata directories that should not be copied from the drive root.
const IGNORED_ROOT_ENTRIES: &[&str] = &[
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    "System Volume Information",
    "$RECYCLE.BIN",
];

/// Verifies that `mount_point` is a valid removable mount and NOT the main system disk.
pub fn is_system_disk(mount_point: &Path) -> bool {
    let canonical = mount_point
        .canonicalize()
        .unwrap_or_else(|_| mount_point.to_path_buf());

    #[cfg(target_os = "macos")]
    {
        let s = canonical.to_string_lossy();
        if s == "/"
            || s == "/System"
            || s.starts_with("/System/Volumes/")
            || s == "/Volumes/Macintosh HD"
            || s == "/Volumes/Recovery"
            || s == "/private"
        {
            return true;
        }
    }

    #[cfg(target_os = "windows")]
    {
        let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let sys_prefix = sys_drive.trim_end_matches('\\').to_uppercase();
        let mount_str = canonical
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_uppercase();
        if mount_str.starts_with(&sys_prefix) {
            return true;
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if canonical == Path::new("/") || canonical == Path::new("/boot") {
            return true;
        }
    }

    false
}

/// Copies the entire contents of `usb_mount` into `[destination_root]/USB_Backup_YYYY-MM-DD_HH-mm-ss/`.
pub fn backup_usb_drive(usb_mount: &Path, destination_root: &Path) -> Result<BackupStats> {
    if !usb_mount.exists() {
        bail!("USB mount point does not exist: {}", usb_mount.display());
    }

    if is_system_disk(usb_mount) {
        bail!(
            "Refusing to back up system disk at {}",
            usb_mount.display()
        );
    }

    // Ensure destination_root is not inside the USB drive itself
    if let (Ok(canon_src), Ok(canon_dst)) = (usb_mount.canonicalize(), destination_root.canonicalize())
        && canon_dst.starts_with(&canon_src)
    {
        bail!(
            "Backup destination ({}) cannot be inside the source USB drive ({})",
            destination_root.display(),
            usb_mount.display()
        );
    }

    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let target_dir = destination_root.join(format!("USB_Backup_{}", timestamp));

    fs::create_dir_all(&target_dir).with_context(|| {
        format!(
            "Failed to create target backup folder: {}",
            target_dir.display()
        )
    })?;

    info!(
        "Starting backup from {} to {}",
        usb_mount.display(),
        target_dir.display()
    );

    let mut stats = BackupStats {
        backup_folder: target_dir.clone(),
        ..Default::default()
    };

    copy_dir_recursive(usb_mount, &target_dir, true, &mut stats)?;

    info!(
        "Backup finished: {} files ({} bytes), {} dirs, {} symlinks, {} skipped",
        stats.files_copied,
        stats.bytes_copied,
        stats.dirs_created,
        stats.symlinks_copied,
        stats.skipped_entries
    );

    Ok(stats)
}

fn copy_dir_recursive(
    src_dir: &Path,
    dst_dir: &Path,
    is_root: bool,
    stats: &mut BackupStats,
) -> Result<()> {
    let entries = match fs::read_dir(src_dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            warn!("Cannot read directory {}: {}", src_dir.display(), err);
            stats.skipped_entries += 1;
            return Ok(());
        }
    };

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                warn!("Error reading directory entry in {}: {}", src_dir.display(), err);
                stats.skipped_entries += 1;
                continue;
            }
        };

        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        if is_root && IGNORED_ROOT_ENTRIES.contains(&file_name_str.as_ref()) {
            continue;
        }

        let src_path = entry.path();
        let dst_path = dst_dir.join(&file_name);

        let file_type = match fs::symlink_metadata(&src_path) {
            Ok(meta) => meta.file_type(),
            Err(err) => {
                warn!("Cannot read metadata for {}: {}", src_path.display(), err);
                stats.skipped_entries += 1;
                continue;
            }
        };

        if file_type.is_symlink() {
            if let Err(err) = copy_symlink(&src_path, &dst_path) {
                warn!(
                    "Failed to copy symlink {} -> {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    err
                );
                stats.skipped_entries += 1;
            } else {
                stats.symlinks_copied += 1;
            }
        } else if file_type.is_dir() {
            if let Err(err) = fs::create_dir_all(&dst_path) {
                warn!("Failed to create directory {}: {}", dst_path.display(), err);
                stats.skipped_entries += 1;
                continue;
            }
            stats.dirs_created += 1;
            copy_dir_recursive(&src_path, &dst_path, false, stats)?;
        } else if file_type.is_file() {
            match copy_single_file(&src_path, &dst_path) {
                Ok(bytes) => {
                    stats.files_copied += 1;
                    stats.bytes_copied += bytes;
                }
                Err(err) => {
                    warn!(
                        "Failed to copy file {} -> {}: {}",
                        src_path.display(),
                        dst_path.display(),
                        err
                    );
                    stats.skipped_entries += 1;
                }
            }
        }
    }

    Ok(())
}

/// Copies a single file and ensures its contents and metadata are flushed to disk.
fn copy_single_file(src: &Path, dst: &Path) -> io::Result<u64> {
    let mut reader = File::open(src)?;
    let mut writer = File::create(dst)?;
    let bytes = io::copy(&mut reader, &mut writer)?;
    writer.sync_all()?;

    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dst, meta.permissions());
    }

    Ok(bytes)
}

/// Recreates a symbolic link at `dst` pointing to the same target as `src`.
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst)?;
    }

    #[cfg(windows)]
    {
        let is_dir = src.metadata().map(|m| m.is_dir()).unwrap_or(false);
        let res = if is_dir {
            std::os::windows::fs::symlink_dir(&target, dst)
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
        };
        // If unprivileged Windows account cannot create symlinks, copy target file if regular file
        if res.is_err() && src.is_file() {
            let _ = copy_single_file(src, dst)?;
        } else {
            res?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_disk_detection() {
        #[cfg(target_os = "macos")]
        {
            assert!(is_system_disk(Path::new("/")));
            assert!(is_system_disk(Path::new("/System")));
            assert!(!is_system_disk(Path::new("/Volumes/MY_USB_FLASH")));
        }

        #[cfg(target_os = "windows")]
        {
            assert!(is_system_disk(Path::new(r"C:\")));
            assert!(!is_system_disk(Path::new(r"E:\")));
        }
    }

    #[test]
    fn test_backup_usb_drive_copies_files_hidden_and_symlinks() -> Result<()> {
        let temp_base = std::env::temp_dir().join(format!("usb_backup_test_{}", std::process::id()));
        let fake_usb = temp_base.join("FakeUSB");
        let dest_root = temp_base.join("Backups");

        let _ = fs::remove_dir_all(&temp_base);
        fs::create_dir_all(fake_usb.join("subdir"))?;
        fs::create_dir_all(fake_usb.join(".Spotlight-V100"))?;
        fs::create_dir_all(&dest_root)?;

        // Normal file, hidden file, nested file, and ignored system file
        fs::write(fake_usb.join("readme.txt"), b"hello usb")?;
        fs::write(fake_usb.join(".hidden_config"), b"secret=123")?;
        fs::write(fake_usb.join("subdir").join("nested.dat"), b"nested payload")?;
        fs::write(fake_usb.join(".Spotlight-V100").join("store.db"), b"ignore me")?;

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            fake_usb.join("readme.txt"),
            fake_usb.join("readme_link.txt"),
        )?;

        let stats = backup_usb_drive(&fake_usb, &dest_root)?;

        assert_eq!(stats.files_copied, 3);
        assert!(stats.backup_folder.exists());
        assert!(
            stats
                .backup_folder
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("USB_Backup_")
        );
        assert_eq!(
            fs::read_to_string(stats.backup_folder.join("readme.txt"))?,
            "hello usb"
        );
        assert_eq!(
            fs::read_to_string(stats.backup_folder.join(".hidden_config"))?,
            "secret=123"
        );
        assert_eq!(
            fs::read_to_string(stats.backup_folder.join("subdir").join("nested.dat"))?,
            "nested payload"
        );
        assert!(!stats.backup_folder.join(".Spotlight-V100").exists());

        #[cfg(unix)]
        {
            assert_eq!(stats.symlinks_copied, 1);
            assert!(
                fs::symlink_metadata(stats.backup_folder.join("readme_link.txt"))?
                    .file_type()
                    .is_symlink()
            );
        }

        let _ = fs::remove_dir_all(&temp_base);
        Ok(())
    }
}

