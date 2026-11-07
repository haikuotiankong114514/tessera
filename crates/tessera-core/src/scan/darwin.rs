//! SSD-oriented Darwin walker: bulk metadata, work stealing, worker-local 64 KiB
//! buffers, bounded directory handles, and one tree lock per batch. Never opens
//! file contents. NoMaterialize also covers intermediate dataless directories.
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, atomic::Ordering::Relaxed};
use std::time::Instant;

use super::{ScanMode, ScanOptions, ScanProgress, ScanSummary, VolumeInfo};
use crate::{EntryBatch, Error, NodeFlags, NodeId, Result, SharedTree};

unsafe extern "C" {
    fn getiopolicy_np(kind: i32, scope: i32) -> i32;
    fn setiopolicy_np(kind: i32, scope: i32, policy: i32) -> i32;
}

/// Apple TN3150: thread-scoped and restored when leaving a borrowed thread.
pub struct NoMaterialize(i32, std::marker::PhantomData<std::rc::Rc<()>>);
impl NoMaterialize {
    pub fn new() -> io::Result<Self> {
        let old = unsafe { getiopolicy_np(3, 1) };
        if old < 0 || unsafe { setiopolicy_np(3, 1, 1) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(old, std::marker::PhantomData))
    }
}
impl Drop for NoMaterialize {
    fn drop(&mut self) {
        unsafe {
            setiopolicy_np(3, 1, self.0);
        }
    }
}

struct Scratch {
    buffer: Vec<u64>,
    batch: EntryBatch,
    dirs: Vec<usize>,
}
thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch { buffer: vec![0;8192], batch: EntryBatch::new(), dirs: Vec::new() });
    static WORKER_POLICY: std::result::Result<NoMaterialize, io::Error> = NoMaterialize::new();
}
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const SF_DATALESS: u32 = 0x40000000;
const SHARDS: usize = 32;
type Identity = (i32, u64);
struct Context<'a> {
    tree: &'a SharedTree,
    progress: &'a ScanProgress,
    identities: [Mutex<HashSet<Identity>>; SHARDS],
    devices: Vec<i32>,
}
struct Job {
    node: NodeId,
    path: PathBuf,
}

impl Context<'_> {
    fn first(&self, id: Identity) -> bool {
        let shard = ((id.1 ^ (id.1 >> 32)) as usize) & (SHARDS - 1);
        self.identities[shard]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id)
    }

    fn inaccessible(&self, node: NodeId) {
        self.progress.inaccessible.fetch_add(1, Relaxed);
        let mut t = self.tree.write().unwrap_or_else(|e| e.into_inner());
        let flags = t.node(node).flags | NodeFlags::INACCESSIBLE;
        t.set_flags(node, flags);
    }

    fn process(&self, job: Job, emit: &mut dyn FnMut(Job)) -> io::Result<()> {
        if self.progress.is_cancelled() {
            return Ok(());
        }
        let path = cpath(&job.path)?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            self.inaccessible(job.node);
            return Ok(());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut attrs = libc::attrlist {
            bitmapcount: 5,
            reserved: 0,
            commonattr: libc::ATTR_CMN_NAME
                | libc::ATTR_CMN_DEVID
                | libc::ATTR_CMN_OBJTYPE
                | libc::ATTR_CMN_MODTIME
                | libc::ATTR_CMN_FLAGS
                | libc::ATTR_CMN_FILEID
                | libc::ATTR_CMN_RETURNED_ATTRS,
            volattr: 0,
            dirattr: 0,
            fileattr: libc::ATTR_FILE_LINKCOUNT
                | libc::ATTR_FILE_TOTALSIZE
                | libc::ATTR_FILE_ALLOCSIZE,
            forkattr: 0,
        };
        let mut first = true;
        SCRATCH.with(|scratch| -> io::Result<()> {
            let Scratch {
                buffer,
                batch,
                dirs,
            } = &mut *scratch.borrow_mut();
            loop {
                if self.progress.is_cancelled() {
                    return Ok(());
                }
                let n = unsafe {
                    libc::getattrlistbulk(
                        fd.as_raw_fd(),
                        (&mut attrs as *mut libc::attrlist).cast(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len() * 8,
                        0,
                    )
                };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    if first
                        && matches!(
                            err.raw_os_error(),
                            Some(libc::ENOTSUP | libc::EINVAL | libc::ENOSYS)
                        )
                    {
                        return self.fallback(&job, fd.as_raw_fd(), emit);
                    }
                    self.inaccessible(job.node);
                    return Ok(());
                }
                first = false;
                if n == 0 {
                    return Ok(());
                }
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8)
                };
                let mut offset = 0;
                batch.clear();
                dirs.clear();
                for _ in 0..n {
                    let length = read::<u32>(bytes, &mut offset)? as usize;
                    let start = offset - 4;
                    let end = start
                        .checked_add(length)
                        .filter(|&v| v <= bytes.len() && length >= 24)
                        .ok_or_else(|| io::Error::other("invalid Darwin attribute record"))?;
                    let record = &bytes[start..end];
                    match decode(record, fd.as_raw_fd()) {
                        Ok(e) => self.add(e, batch, dirs),
                        Err(e)
                            if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {}
                        Err(e)
                            if matches!(
                                e.raw_os_error(),
                                Some(libc::EPERM | libc::EACCES | libc::EDEADLK)
                            ) =>
                        {
                            self.progress.inaccessible.fetch_add(1, Relaxed);
                        }
                        Err(e) => return Err(e),
                    }
                    offset = end;
                }
                self.flush(&job, batch, dirs, emit);
            }
        })
    }

    fn add(&self, e: Entry<'_>, batch: &mut EntryBatch, dirs: &mut Vec<usize>) {
        if e.name == b"." || e.name == b".." {
            return;
        }
        let mut flags = NodeFlags::NONE;
        let dir = e.kind == VDIR;
        if dir {
            flags |= NodeFlags::DIR;
        }
        if e.kind == VLNK {
            flags |= NodeFlags::REPARSE;
        }
        if e.flags & SF_DATALESS != 0 {
            flags |= NodeFlags::CLOUD;
        }
        if e.name.starts_with(b".") || e.flags & libc::UF_HIDDEN != 0 {
            flags |= NodeFlags::HIDDEN;
        }
        let mut disk = if dir { 0 } else { e.disk };
        let mut logical = if dir { 0 } else { e.logical };
        if dir && (!self.devices.contains(&e.device) || !self.first((e.device, e.inode))) {
            flags |= NodeFlags::REPARSE;
        } else if !dir && e.links > 1 && !self.first((e.device, e.inode)) {
            flags |= NodeFlags::HARDLINK;
            disk = 0;
            logical = 0;
        }
        if !dir && disk < logical {
            flags |= NodeFlags::COMPRESSED;
        }
        if dir && !flags.intersects(NodeFlags::REPARSE | NodeFlags::CLOUD) {
            dirs.push(batch.len());
        }
        let minutes = (e.seconds / 60 + 194_074_560).clamp(1, u32::MAX as i64) as u32;
        batch.push_wtf8(e.name, disk, logical, flags, minutes);
        batch.set_last_identity((e.device, e.inode), e.links);
    }

    fn flush(&self, job: &Job, batch: &EntryBatch, dirs: &[usize], emit: &mut dyn FnMut(Job)) {
        if batch.is_empty() {
            return;
        }
        let (files, disk) = batch.entries().iter().fold((0, 0u64), |(count, size), e| {
            (
                count + u64::from(!e.flags.contains(NodeFlags::DIR)),
                size.saturating_add(e.size),
            )
        });
        self.progress.files.fetch_add(files, Relaxed);
        self.progress
            .dirs
            .fetch_add(batch.len() as u64 - files, Relaxed);
        self.progress.bytes.fetch_add(disk, Relaxed);
        let ids = self
            .tree
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .add_children(job.node, batch);
        for &i in dirs {
            let e = &batch.entries()[i];
            // Startup scan's physical Data mount is an alias of firmlink paths.
            let path = job.path.join(OsStr::from_bytes(batch.name(e)));
            if job.path == Path::new("/System/Volumes") && batch.name(e) == b"Data" {
                continue;
            }
            emit(Job {
                node: NodeId(ids.start + i as u32),
                path,
            });
        }
    }

    fn fallback(&self, job: &Job, fd: i32, emit: &mut dyn FnMut(Job)) -> io::Result<()> {
        let dup = unsafe { libc::dup(fd) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            unsafe {
                libc::close(dup);
            }
            return Err(io::Error::last_os_error());
        }
        struct Dir(*mut libc::DIR);
        impl Drop for Dir {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let dir = Dir(dir);
        let mut batch = EntryBatch::new();
        let mut dirs = Vec::new();
        loop {
            if self.progress.is_cancelled() {
                break;
            }
            unsafe {
                *libc::__error() = 0;
            }
            let de = unsafe { libc::readdir(dir.0) };
            if de.is_null() {
                if io::Error::last_os_error().raw_os_error() != Some(0) {
                    self.inaccessible(job.node);
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*de).d_name.as_ptr()) };
            if let Ok(e) = stat_entry(fd, name) {
                self.add(e, &mut batch, &mut dirs);
            }
            if batch.len() >= 4096 {
                self.flush(job, &batch, &dirs, emit);
                batch.clear();
                dirs.clear();
            }
        }
        self.flush(job, &batch, &dirs, emit);
        Ok(())
    }
}

pub fn default_threads() -> usize {
    // The local SSD matrix favors the performance-core count over oversubscribing
    // all efficiency cores. Intel falls back to physical cores; --threads remains
    // explicit for other disks and reproducible measurements.
    for key in [c"hw.perflevel0.physicalcpu", c"hw.physicalcpu"] {
        let mut cores = 0u32;
        let mut len = std::mem::size_of_val(&cores);
        if unsafe {
            libc::sysctlbyname(
                key.as_ptr(),
                (&mut cores as *mut u32).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } == 0
            && cores > 0
        {
            return (cores as usize).clamp(2, 16);
        }
    }
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 16)
}

pub fn scan(
    root: &Path,
    opts: &ScanOptions,
    tree: &SharedTree,
    progress: &ScanProgress,
) -> Result<ScanSummary> {
    if opts.mode == ScanMode::Mft {
        return Err(Error::Unsupported("MFT is Windows/NTFS-only"));
    }
    let start = Instant::now();
    let _policy = NoMaterialize::new()?;
    let root = std::fs::canonicalize(root)?;
    let meta = std::fs::metadata(&root)?;
    use std::os::unix::fs::MetadataExt;
    if !meta.is_dir() {
        return Err(
            io::Error::new(io::ErrorKind::NotADirectory, "scan root is not a directory").into(),
        );
    }
    let mut devices = vec![meta.dev() as i32];
    if root == Path::new("/") {
        if let Ok(data) = std::fs::metadata("/System/Volumes/Data") {
            devices.push(data.dev() as i32);
        }
    }
    if let Some(id) = tree
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .identities
        .get_mut(0)
    {
        *id = (meta.dev() as i32, meta.ino());
    }
    let ctx = Context {
        tree,
        progress,
        devices,
        identities: std::array::from_fn(|_| Mutex::new(HashSet::new())),
    };
    ctx.first((meta.dev() as i32, meta.ino()));
    let count = if opts.threads == 0 {
        default_threads()
    } else {
        opts.threads.clamp(1, 64)
    };
    let mut pending = Vec::new();
    ctx.process(
        Job {
            node: NodeId::ROOT,
            path: root.clone(),
        },
        &mut |j| pending.push(j),
    )?;
    let pool = if count > 1 && !pending.is_empty() {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(count)
                .thread_name(|i| format!("tessera-darwin-{i}"))
                .build()
                .map_err(io::Error::other)?,
        )
    } else {
        None
    };
    for phase in 0..2 {
        if phase == 1 {
            if root != Path::new("/") || progress.is_cancelled() {
                break;
            }
            let data = {
                tree.read()
                    .unwrap_or_else(|e| e.into_inner())
                    .lookup_relative(b"System/Volumes/Data")
            };
            // The normal namespace gets first claim on the shared directory IDs.
            // The second pass covers Data-only entries such as hidden metadata.
            if let Some(node) = data {
                ctx.process(
                    Job {
                        node,
                        path: PathBuf::from("/System/Volumes/Data"),
                    },
                    &mut |j| pending.push(j),
                )?;
            }
        }
        if let Some(pool) = &pool {
            pool.scope(|scope| {
                for job in pending.drain(..) {
                    spawn(scope, &ctx, job);
                }
            });
        } else {
            while let Some(job) = pending.pop() {
                ctx.process(job, &mut |j| pending.push(j))?;
            }
        }
    }
    tree.write().unwrap_or_else(|e| e.into_inner()).end_scan();
    if progress.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let snap = progress.snapshot();
    Ok(ScanSummary {
        mode: ScanMode::Walk,
        elapsed: start.elapsed(),
        files: snap.files,
        dirs: snap.dirs,
        inaccessible: snap.inaccessible,
        volume: volume_info(&root).ok(),
        ..Default::default()
    })
}

fn spawn<'s>(scope: &rayon::Scope<'s>, ctx: &'s Context<'s>, job: Job) {
    scope.spawn(move |s| {
        // Dedicated workers must not inherit the process's materialization policy.
        WORKER_POLICY.with(|policy| match policy {
            Ok(_policy) => {
                if ctx.process(job, &mut |j| spawn(s, ctx, j)).is_err() {
                    ctx.progress.inaccessible.fetch_add(1, Relaxed);
                }
            }
            Err(_) => ctx.inaccessible(job.node),
        });
    });
}

pub fn volume_info(path: &Path) -> Result<VolumeInfo> {
    let path = cpath(path)?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(VolumeInfo {
        total_bytes: stat.f_blocks.saturating_mul(stat.f_bsize as u64),
        free_bytes: stat.f_bavail.saturating_mul(stat.f_bsize as u64),
        file_system: unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    })
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

struct Entry<'a> {
    name: &'a [u8],
    device: i32,
    inode: u64,
    kind: u32,
    flags: u32,
    links: u32,
    disk: u64,
    logical: u64,
    seconds: i64,
}
fn read<T: Copy>(buf: &[u8], offset: &mut usize) -> io::Result<T> {
    let end = offset
        .checked_add(std::mem::size_of::<T>())
        .filter(|&e| e <= buf.len())
        .ok_or_else(|| io::Error::other("truncated Darwin attributes"))?;
    let value = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(*offset).cast::<T>()) };
    *offset = end;
    Ok(value)
}

fn decode(record: &[u8], fd: i32) -> io::Result<Entry<'_>> {
    let mut p = 4;
    let attrs: [u32; 5] = read(record, &mut p)?;
    let reference_at = p;
    let reference: libc::attrreference_t = read(record, &mut p)?;
    let begin = (reference_at as i64 + reference.attr_dataoffset as i64) as usize;
    let name = record
        .get(begin..begin.saturating_add(reference.attr_length as usize))
        .and_then(|v| CStr::from_bytes_with_nul(v).ok())
        .ok_or_else(|| io::Error::other("invalid Darwin name reference"))?;
    let common = attrs[0];
    let file = attrs[3];
    let device = if common & libc::ATTR_CMN_DEVID != 0 {
        read::<i32>(record, &mut p)?
    } else {
        0
    };
    let kind = if common & libc::ATTR_CMN_OBJTYPE != 0 {
        read::<u32>(record, &mut p)?
    } else {
        0
    };
    let mtime = if common & libc::ATTR_CMN_MODTIME != 0 {
        read::<libc::timespec>(record, &mut p)?.tv_sec
    } else {
        0
    };
    let flags = if common & libc::ATTR_CMN_FLAGS != 0 {
        read::<u32>(record, &mut p)?
    } else {
        0
    };
    let inode = if common & libc::ATTR_CMN_FILEID != 0 {
        read::<u64>(record, &mut p)?
    } else {
        0
    };
    let links = if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        read::<u32>(record, &mut p)?
    } else {
        1
    };
    let logical = if file & libc::ATTR_FILE_TOTALSIZE != 0 {
        read::<i64>(record, &mut p)?.max(0) as u64
    } else {
        0
    };
    let disk = if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        read::<i64>(record, &mut p)?.max(0) as u64
    } else {
        0
    };
    if device == 0
        || inode == 0
        || kind == 0
        || (kind != VDIR && file & libc::ATTR_FILE_ALLOCSIZE == 0)
    {
        return stat_entry(fd, name);
    }
    Ok(Entry {
        name: name.to_bytes(),
        device,
        inode,
        kind,
        flags,
        links,
        disk,
        logical,
        seconds: mtime,
    })
}

fn stat_entry<'a>(fd: i32, name: &'a CStr) -> io::Result<Entry<'a>> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(fd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFDIR => VDIR,
        libc::S_IFLNK => VLNK,
        _ => VREG,
    };
    Ok(Entry {
        name: name.to_bytes(),
        device: stat.st_dev,
        inode: stat.st_ino,
        kind,
        flags: stat.st_flags,
        links: stat.st_nlink as u32,
        disk: (stat.st_blocks.max(0) as u64).saturating_mul(512),
        logical: stat.st_size.max(0) as u64,
        seconds: stat.st_mtime,
    })
}

/// A shallow directory snapshot used by the FSEvents reconciler. It uses the
/// exact same bulk decoder as the scanner, and never walks unchanged subtrees.
fn shallow(path: &Path, scan_root: &Path) -> Result<crate::Tree> {
    use std::os::unix::fs::MetadataExt;
    let _policy = NoMaterialize::new()?;
    let meta = std::fs::metadata(path)?;
    let tree = crate::Tree::for_path(path).into_shared();
    let progress = ScanProgress::new();
    let mut devices = vec![meta.dev() as i32];
    if scan_root == Path::new("/") {
        if let Ok(data) = std::fs::metadata("/System/Volumes/Data") {
            devices.push(data.dev() as i32);
        }
    }
    let ctx = Context {
        tree: &tree,
        progress: &progress,
        devices,
        identities: std::array::from_fn(|_| Mutex::new(HashSet::new())),
    };
    ctx.process(
        Job {
            node: NodeId::ROOT,
            path: path.to_path_buf(),
        },
        &mut |_| {},
    )?;
    drop(ctx);
    let t = std::sync::Arc::try_unwrap(tree)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    if t.node(NodeId::ROOT).flags.contains(NodeFlags::INACCESSIBLE) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory snapshot unavailable",
        )
        .into());
    }
    Ok(t)
}

/// Reconciles one dirty directory, preserving unchanged node ids. Newly created
/// directories are enumerated iteratively; metadata I/O never holds the tree lock.
/// Returns false when the snapshot is unchanged (no layout/event needed).
pub fn refresh(tree: &SharedTree, directory: &Path) -> Result<bool> {
    use std::collections::HashMap;
    let _policy = NoMaterialize::new()?;
    let root = tree
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .path(NodeId::ROOT);
    let mut directory = directory.to_path_buf();
    if root == Path::new("/") {
        if let Ok(relative) = directory.strip_prefix("/System/Volumes/Data") {
            let t = tree.read().unwrap_or_else(|e| e.into_inner());
            if !relative.as_os_str().is_empty()
                && t.lookup_relative(relative.as_os_str().as_bytes())
                    .is_some_and(|id| !t.is_deleted(id) && t.node(id).is_dir())
            {
                directory = root.join(relative);
            }
        }
    }
    let node = loop {
        let rel = directory
            .strip_prefix(&root)
            .map_err(|_| io::Error::other("change outside scan root"))?;
        let t = tree.read().unwrap_or_else(|e| e.into_inner());
        let id = t.lookup_relative(rel.as_os_str().as_bytes());
        if let Some(id) = id.filter(|&id| {
            !t.is_deleted(id)
                && t.node(id).is_dir()
                && !t
                    .node(id)
                    .flags
                    .intersects(NodeFlags::REPARSE | NodeFlags::CLOUD)
        }) {
            break id;
        }
        drop(t);
        if !directory.pop() {
            return Ok(false);
        }
    };
    let canonical_root = std::fs::canonicalize(&root)?;
    let mut pending = vec![(node, directory)];
    let mut changed = false;
    let mut link_groups = HashSet::new();
    while let Some((parent, path)) = pending.pop() {
        let actual = canonical_root.join(path.strip_prefix(&root).unwrap_or(Path::new("")));
        let snapshot = shallow(&actual, &root)?;
        let mut t = tree.write().unwrap_or_else(|e| e.into_inner());
        let old: Vec<_> = t.children(parent).collect();
        let mut names: HashMap<Vec<u8>, NodeId> =
            old.iter().map(|&id| (t.name(id).to_vec(), id)).collect();
        let identities: HashMap<_, _> = old
            .iter()
            .filter_map(|&id| t.native_identity(id).map(|key| (key, id)))
            .collect();
        let mut seen = HashSet::new();
        for source in snapshot.children(NodeId::ROOT) {
            let name = snapshot.name(source);
            let n = snapshot.node(source);
            let identity = snapshot.native_identity(source).unwrap_or((0, 0));
            let named = names
                .remove(name)
                .filter(|&id| t.native_identity(id) == Some(identity));
            let shared_identity =
                snapshot.hardlinks.contains_key(&identity) || t.hardlinks.contains_key(&identity);
            let existing = named.or_else(|| {
                if shared_identity {
                    return None;
                }
                identities
                    .get(&identity)
                    .copied()
                    .filter(|id| !seen.contains(id))
            });
            let id = if let Some(id) = existing {
                if t.name(id) != name {
                    t.move_node(id, parent, name);
                    changed = true;
                }
                let old = *t.node(id);
                if !n.is_dir() && (old.size != n.size || old.logical != n.logical) {
                    t.set_file_size(id, n.size, n.logical);
                    changed = true;
                }
                if old.mtime != n.mtime {
                    t.set_mtime(id, n.mtime);
                    changed = true;
                }
                let flags = if old.flags.contains(NodeFlags::REPARSE) {
                    n.flags | NodeFlags::REPARSE
                } else {
                    n.flags
                };
                if old.flags != flags {
                    t.set_flags(id, flags);
                    changed = true;
                }
                id
            } else {
                let id = t.add_child(parent, name, n.size, n.logical, n.flags, n.mtime);
                t.identities[id.index()] = identity;
                if n.is_dir() && !n.flags.intersects(NodeFlags::REPARSE | NodeFlags::CLOUD) {
                    pending.push((id, path.join(OsStr::from_bytes(name))));
                }
                changed = true;
                id
            };
            seen.insert(id);
            if shared_identity {
                let group = t.hardlinks.entry(identity).or_default();
                if !group.contains(&id) {
                    group.push(id);
                }
                link_groups.insert(identity);
            }
        }
        let mut removed_directory = false;
        for id in old {
            if !seen.contains(&id) {
                removed_directory |= t.node(id).is_dir();
                if let Some(identity) = t.native_identity(id) {
                    if t.hardlinks.contains_key(&identity) {
                        link_groups.insert(identity);
                    }
                }
                changed |= t.mark_deleted(id);
            }
        }
        if removed_directory {
            link_groups.extend(
                t.hardlinks
                    .iter()
                    .filter(|(_, ids)| ids.iter().any(|&id| t.is_deleted(id)))
                    .map(|(&key, _)| key),
            );
        }
    }
    // A hardlink's counted name can disappear; transfer its bytes to a survivor.
    // Only affected link groups are probed, never every file in the tree.
    for identity in link_groups {
        let paths = {
            let t = tree.read().unwrap_or_else(|e| e.into_inner());
            t.hardlinks
                .get(&identity)
                .into_iter()
                .flatten()
                .filter(|&&id| !t.is_deleted(id))
                .map(|&id| (id, t.path(id)))
                .collect::<Vec<_>>()
        };
        {
            let mut t = tree.write().unwrap_or_else(|e| e.into_inner());
            if paths.is_empty() {
                t.hardlinks.remove(&identity);
            } else {
                t.hardlinks
                    .insert(identity, paths.iter().map(|(id, _)| *id).collect());
            }
        }
        let mut owner = false;
        for (id, path) in paths {
            let meta = std::fs::symlink_metadata(&path);
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = meta {
                if (meta.dev() as i32, meta.ino()) != identity {
                    continue;
                }
                let mut t = tree.write().unwrap_or_else(|e| e.into_inner());
                let (disk, logical) = if !owner {
                    ((meta.blocks() * 512), meta.size())
                } else {
                    (0, 0)
                };
                let was = *t.node(id);
                if was.size != disk || was.logical != logical {
                    t.set_file_size(id, disk, logical);
                    changed = true;
                }
                let mut flags = was.flags;
                if owner {
                    flags.insert(NodeFlags::HARDLINK);
                } else {
                    flags.remove(NodeFlags::HARDLINK);
                }
                if flags != was.flags {
                    t.set_flags(id, flags);
                    changed = true;
                }
                owner = true;
            }
        }
    }
    Ok(changed)
}

/// Platform protection rules, shared by presentation and execution.
pub fn lock_reason(path: &Path) -> Option<&'static str> {
    if path.parent().is_none() {
        return Some("volumeRoot");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.as_deref() == Some(path) {
        return Some("homeRoot");
    }
    let protected = [
        "/System",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/private/etc",
        "/private/var/db",
        "/dev",
    ];
    for p in protected {
        let p = Path::new(p);
        if path.starts_with(p) || p.starts_with(path) {
            return Some("systemProtected");
        }
    }
    let mounts = [
        "/Volumes",
        "/Applications",
        "/Library",
        "/Users",
        "/private",
        "/usr",
    ];
    if mounts.iter().any(|p| Path::new(p) == path) {
        return Some("systemRoot");
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::MetadataExt;
        if meta.permissions().readonly() && meta.is_dir() {
            return Some("readOnly");
        }
        if let Some(parent) = path.parent().and_then(|p| std::fs::metadata(p).ok()) {
            if meta.dev() != parent.dev() {
                return Some("volumeRoot");
            }
        }
    }
    None
}

pub(crate) fn protected_nodes(tree: &crate::Tree) -> Vec<u32> {
    let root = tree.path(NodeId::ROOT);
    let mut ids = Vec::new();
    if lock_reason(&root).is_some() {
        ids.push(0);
    }
    for path in [
        "/System",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/private/etc",
        "/private/var/db",
        "/dev",
        "/Applications",
        "/Library",
        "/Users",
        "/Volumes",
    ] {
        if let Ok(rel) = Path::new(path).strip_prefix(&root) {
            if let Some(id) = tree.lookup_relative(rel.as_os_str().as_bytes()) {
                ids.push(id.0);
                ids.extend(tree.ancestors(id).map(|id| id.0));
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}
