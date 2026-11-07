//! The drives the picker offers (`list_volumes`).

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Volume {
    /// `C:\`
    pub root: String,
    pub label: String,
    pub fs: String,
    pub capacity: u64,
    pub free: u64,
}

/// Fixed and removable drives that have a file system, in drive-letter order.
pub fn list() -> Vec<Volume> {
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec!["/".to_string()];
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            roots.extend(
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned()),
            );
        }
        let mut volumes: Vec<_> = roots
            .into_iter()
            .filter_map(|root| describe(&root))
            .collect();
        if let Ok(root) = std::env::var("TESSERA_BENCH_ROOT") {
            let path = std::path::Path::new(&root);
            if path.is_dir() && !volumes.iter().any(|v| v.root == root) {
                if let Ok(info) = tessera_core::scan::volume_info(path) {
                    let label = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned();
                    volumes.push(Volume {
                        root,
                        label,
                        fs: info.file_system,
                        capacity: info.total_bytes,
                        free: info.free_bytes,
                    });
                }
            }
        }
        return volumes;
    }
    #[cfg(not(target_os = "macos"))]
    imp::letters()
        .into_iter()
        .filter_map(|c| describe(&format!("{c}:\\")))
        .collect()
}

/// One drive by its root (`C:\`), or `None` if it is not a fixed or removable
/// drive with a readable file system.
pub fn describe(root: &str) -> Option<Volume> {
    imp::describe(root)
}

#[cfg(windows)]
mod imp {
    use super::Volume;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::core::PCWSTR;

    // GetDriveTypeW results
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    pub fn letters() -> Vec<char> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| (b'A' + i) as char)
            .collect()
    }

    pub fn describe(root: &str) -> Option<Volume> {
        let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
        let p = PCWSTR(wide.as_ptr());
        let kind = unsafe { GetDriveTypeW(p) };
        if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
            return None;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 64];
        unsafe { GetVolumeInformationW(p, Some(&mut label), None, None, None, Some(&mut fs)) }
            .ok()?;
        let (mut total, mut free) = (0u64, 0u64);
        unsafe { GetDiskFreeSpaceExW(p, None, Some(&mut total), Some(&mut free)) }.ok()?;
        let text = |b: &[u16]| {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf16_lossy(&b[..n])
        };
        let mut label = text(&label);
        if label.is_empty() {
            label = if kind == DRIVE_FIXED {
                crate::i18n::text::LOCAL_DISK
            } else {
                crate::i18n::text::REMOVABLE_DISK
            }
            .get()
            .into();
        }
        Some(Volume {
            root: root.to_string(),
            label,
            fs: text(&fs),
            capacity: total,
            free,
        })
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::Volume;

    pub fn letters() -> Vec<char> {
        Vec::new()
    }

    pub fn describe(_root: &str) -> Option<Volume> {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Volume;
    pub fn describe(root: &str) -> Option<Volume> {
        use std::os::unix::fs::MetadataExt;
        let path = std::path::Path::new(root);
        if root != "/" && path.parent() != Some(std::path::Path::new("/Volumes")) {
            return None;
        }
        if root != "/"
            && std::fs::metadata(path).ok()?.dev() == std::fs::metadata("/Volumes").ok()?.dev()
        {
            return None;
        }
        let info = tessera_core::scan::volume_info(path).ok()?;
        Some(Volume {
            root: root.into(),
            label: if root == "/" {
                "Macintosh HD".into()
            } else {
                path.file_name()?.to_string_lossy().into_owned()
            },
            fs: info.file_system,
            capacity: info.total_bytes,
            free: info.free_bytes,
        })
    }
}
