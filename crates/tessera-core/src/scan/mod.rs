//! Filling a [`Tree`](crate::Tree) from disk.
//!
//! Two paths share one output: the tree.
//!
//! * [`ScanMode::Walk`]: parallel directory enumeration with a work-stealing
//!   pool. Works without admin rights and on any file system.
//! * [`ScanMode::Mft`]: reads the NTFS master file table straight from the
//!   volume. Needs admin rights and an NTFS volume root.
//!
//! [`ScanMode::Auto`] picks MFT when it can and falls back to the walk.
//!
//! Progress is published through atomics in [`ScanProgress`], which the UI
//! may sample (e.g. every 100 ms) while the scan runs. Nothing polls after
//! the scan ends.

#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[cfg(not(target_os = "macos"))]
use crate::{Result, SharedTree};

#[cfg(target_os = "macos")]
pub mod darwin;
pub mod mft;
#[cfg(windows)]
pub mod walk;

/// Which scanner to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScanMode {
    /// MFT if elevated and the root is an NTFS volume root, else walk.
    #[default]
    Auto,
    /// Parallel directory walk.
    Walk,
    /// Direct MFT read (fails with [`crate::Error::Unsupported`] if impossible).
    Mft,
}

/// Scan settings.
#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    pub mode: ScanMode,
    /// Worker threads for the walk (0 = automatic).
    pub threads: usize,
}

/// Live counters, shared with the UI while a scan runs.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// Bytes on disk found so far.
    pub bytes: AtomicU64,
    /// Directories that could not be read.
    pub inaccessible: AtomicU64,
    cancel: AtomicBool,
}

/// A copy of [`ScanProgress`] at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub inaccessible: u64,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the running scan to stop; it returns [`crate::Error::Cancelled`].
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            inaccessible: self.inaccessible.load(Ordering::Relaxed),
        }
    }
}

/// Capacity of the volume holding the scanned root.
#[derive(Clone, Debug, Default)]
pub struct VolumeInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// File system name, e.g. `NTFS`, `exFAT`.
    pub file_system: String,
}

/// Where the USN change journal stood when an MFT scan started, so that
/// the watcher can pick up every change made after it.
#[derive(Clone, Copy, Debug)]
pub struct UsnCheckpoint {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// Maps NTFS file record numbers to tree nodes. Produced by the MFT scan
/// and used by the USN watcher. `u32::MAX` means "no node".
#[derive(Clone, Debug, Default)]
pub struct FrnIndex {
    pub(crate) record_to_node: Vec<u32>,
}

impl FrnIndex {
    /// The node for MFT record `record` (the low 48 bits of a file reference).
    pub fn get(&self, record: u64) -> Option<crate::NodeId> {
        match self.record_to_node.get(record as usize) {
            Some(&n) if n != u32::MAX => Some(crate::NodeId(n)),
            _ => None,
        }
    }

    pub fn heap_bytes(&self) -> usize {
        self.record_to_node.capacity() * 4
    }
}

/// What a finished scan reports.
#[derive(Debug, Default)]
pub struct ScanSummary {
    /// The scanner that actually ran (never `Auto`).
    pub mode: ScanMode,
    pub elapsed: Duration,
    pub files: u64,
    pub dirs: u64,
    pub inaccessible: u64,
    pub volume: Option<VolumeInfo>,
    /// Set by the MFT scanner, for the USN watcher.
    pub frn_index: Option<FrnIndex>,
    /// Set by the MFT scanner, for the USN watcher.
    pub usn_checkpoint: Option<UsnCheckpoint>,
}

/// Scans `root` into `tree`, which must contain only its root node
/// (create it with [`Tree::for_path`](crate::Tree::for_path)).
///
/// Blocks until done. The UI may read the tree (and `progress`) while this
/// runs on another thread; the walk inserts directories as it reads them,
/// the MFT scan swaps the finished tree in at the end.
#[cfg(windows)]
pub fn scan(
    root: &Path,
    opts: &ScanOptions,
    tree: &SharedTree,
    progress: &ScanProgress,
) -> Result<ScanSummary> {
    let start = std::time::Instant::now();
    let mut summary = match opts.mode {
        ScanMode::Walk => walk::scan(root, opts, tree, progress)?,
        ScanMode::Mft => mft::scan(root, opts, tree, progress)?,
        ScanMode::Auto => {
            if mft::can_scan(root) {
                match mft::scan(root, opts, tree, progress) {
                    Ok(s) => s,
                    Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
                    Err(_) => walk::scan(root, opts, tree, progress)?,
                }
            } else {
                walk::scan(root, opts, tree, progress)?
            }
        }
    };
    if let Ok(mut t) = tree.write() {
        t.end_scan();
    }
    summary.elapsed = start.elapsed();
    if summary.volume.is_none() {
        summary.volume = volume_info(root).ok();
    }
    Ok(summary)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn scan(
    _root: &Path,
    _opts: &ScanOptions,
    _tree: &SharedTree,
    _progress: &ScanProgress,
) -> Result<ScanSummary> {
    Err(crate::Error::Unsupported(
        "scanning is only implemented on Windows",
    ))
}

/// True if the process runs elevated (admin token).
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// Size, free space and file system of the volume holding `path`.
#[cfg(windows)]
pub fn volume_info(path: &Path) -> Result<VolumeInfo> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNameW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut vol = [0u16; 512];
    let mut fs = [0u16; 64];
    let (mut free, mut total) = (0u64, 0u64);
    unsafe {
        GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut vol)?;
        let vol_ptr = PCWSTR(vol.as_ptr());
        GetVolumeInformationW(vol_ptr, None, None, None, None, Some(&mut fs))?;
        GetDiskFreeSpaceExW(vol_ptr, None, Some(&mut total), Some(&mut free))?;
    }
    let fs_len = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    Ok(VolumeInfo {
        total_bytes: total,
        free_bytes: free,
        file_system: String::from_utf16_lossy(&fs[..fs_len]),
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn volume_info(_path: &Path) -> Result<VolumeInfo> {
    Err(crate::Error::Unsupported(
        "volume info is only implemented on Windows",
    ))
}

#[cfg(target_os = "macos")]
pub use darwin::{scan, volume_info};
