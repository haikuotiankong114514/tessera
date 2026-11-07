//! Deleting: to the recycle bin with undo ([`recycle`], [`restore`]),
//! permanently ([`purge`]), and emptying the recycle bin
//! ([`empty_recycle_bin`]). These back the `recycle`, `restore`, `purge`
//! and `empty_recycle_bin` commands of the IPC contract.
//!
//! The work is done by the shell's `IFileOperation`, so it behaves like
//! Explorer.
//!
//! # Prompts and errors
//!
//! Tessera shows its own confirmation first, so Windows' "Are you sure?"
//! prompt is suppressed (`FOF_NOCONFIRMATION`). Everything else is Windows'
//! own UI, as in Explorer: the progress dialog for long operations, "file
//! in use" with Try again / Skip / Cancel, the administrator-permission
//! prompt (the shell handles elevation), and, when recycling, a warning
//! before an item that cannot be recycled (too large, or no recycle bin on
//! that drive) is destroyed instead (`FOF_WANTNUKEWARNING`). Pass
//! [`DeleteOptions::owner_hwnd`] so those dialogs belong to Tessera's window.
//!
//! [`DeleteOptions::silent`] turns all of that off, for tests and headless
//! use: no progress, error or elevation dialogs (a failing item is skipped),
//! and an item that cannot be recycled is deleted permanently, because
//! Windows has nobody to ask.
//!
//! Per-item results come from an `IFileOperationProgressSink`: the reason
//! an item failed, and for a recycled item the item it became in the
//! Recycle Bin, which [`restore`] moves back with `IFileOperation::MoveItem`.
//! Whether an item is gone is always checked on disk afterwards.
//!
//! # Threading
//!
//! All operations block until the shell is done, which can take minutes or
//! wait on a dialog, so run them on a background thread (e.g. Tauri's
//! `spawn_blocking`). They initialise COM as a single-threaded apartment on
//! the calling thread (`IFileOperation` does not work in the multithreaded
//! apartment); if that thread already belongs to the multithreaded
//! apartment, the operation runs on a short-lived helper thread instead.
//! The tree lock is never held while the shell works.
//!
//! # Protected folders
//!
//! [`lock_reason`] refuses folders whose deletion would break Windows or the
//! user's profile, with a user-facing reason: volume roots (and mounted
//! folders), the Windows, System and SysWOW64 folders, every Program Files
//! folder, ProgramData, the Users folder, the Public profile, the current
//! user's profile, its shell folders (Desktop, Documents, Downloads,
//! Pictures, Music, Videos, OneDrive, AppData\Roaming, AppData\Local,
//! AppData\LocalLow) and the user's Temp folder, plus every folder that
//! contains one of them. The locations come from the known-folder API, never
//! from hard-coded paths, and are resolved once per process. Their contents
//! are not protected.
//!
//! # Tree updates
//!
//! The tree is updated as soon as the shell returns, without waiting for
//! the watcher: what is gone is marked deleted, what [`restore`] brought
//! back is un-deleted ([`Tree::restore_deleted`]). The watcher must
//! therefore tolerate events for nodes that are already up to date.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::{NodeFlags, NodeId, Result, SharedTree, Tree, wtf8};

/// Where deleted items go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeleteMode {
    /// Move to the recycle bin.
    #[default]
    RecycleBin,
    /// Delete permanently.
    Permanent,
}

/// How to run a shell operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeleteOptions {
    /// The window (`HWND` as an integer) that owns Windows' progress and
    /// error dialogs, or 0 for none.
    pub owner_hwnd: isize,
    /// Show no Windows UI at all (see the module docs). For tests and
    /// headless use only.
    pub silent: bool,
}

/// One item that was not removed (or not restored), and why, in English.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub id: NodeId,
    pub reason: String,
}

/// The `DeleteResult` of the IPC contract, minus `rev`, which the caller
/// owns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteResult {
    /// Requested ids that were removed from the tree ([`restore`]: put back).
    pub ok: Vec<NodeId>,
    pub failed: Vec<Failure>,
    /// Bytes on disk ([`crate::Node::size`]) moved or freed (restored).
    pub bytes: u64,
    /// Token for [`restore`] ([`recycle`] only, when something can be undone).
    pub undo: Option<u32>,
}

/// User-facing reasons, in English (the UI translates them).
pub mod reasons {
    pub const DRIVE_ROOT: &str = "This is the root of a drive.";
    pub const SCAN_ROOT: &str = "This is the scanned folder itself.";
    pub const WINDOWS: &str = "Windows needs this folder.";
    pub const PROGRAMS: &str = "Installed programs live in this folder.";
    pub const APP_DATA: &str = "Windows and your apps keep their data in this folder.";
    pub const PROFILES: &str = "This folder holds the user profiles.";
    pub const PROFILE: &str = "This is a user profile folder.";
    pub const PERSONAL: &str =
        "This is one of your personal folders. Delete what is inside it instead.";
    pub const TEMP: &str =
        "Windows and apps keep temporary files here. Delete what is inside it instead.";
    pub const CONTAINS: &str = "This folder contains folders that Windows or your apps need.";
    pub const STILL_THERE: &str = "The item could not be deleted.";
    pub const NOT_RESTORED: &str = "The item could not be restored.";
    pub const NAME_TAKEN: &str = "Another item with this name is already there.";
    pub const CANCELLED: &str = "Cancelled.";
    pub const SKIPPED: &str = "Skipped.";
}

// ---------------------------------------------------------------------------
// Commands

/// Moves `ids` to the recycle bin and removes what is gone from the tree.
/// Returns an undo token for [`restore`] when at least one item landed in
/// the bin. Ids that are invalid or already deleted are ignored; an id below
/// another requested id goes with it.
pub fn recycle(
    tree: &SharedTree,
    ids: &[NodeId],
    opts: &DeleteOptions,
    undo: &UndoStore,
) -> Result<DeleteResult> {
    let (mut res, items) = remove(tree, ids, DeleteMode::RecycleBin, opts)?;
    if !items.is_empty() {
        res.undo = Some(undo.put(items));
    }
    Ok(res)
}

/// Deletes `ids` permanently and removes what is gone from the tree.
pub fn purge(tree: &SharedTree, ids: &[NodeId], opts: &DeleteOptions) -> Result<DeleteResult> {
    Ok(remove(tree, ids, DeleteMode::Permanent, opts)?.0)
}

/// Undoes a [`recycle`]: moves its items out of the Recycle Bin to where
/// they were and puts their nodes back in the tree. Items that could not be
/// restored stay under the token, so it can be tried again.
pub fn restore(
    tree: &SharedTree,
    token: u32,
    opts: &DeleteOptions,
    undo: &UndoStore,
) -> Result<DeleteResult> {
    let Some(items) = undo.take(token) else {
        return Err(crate::Error::Unsupported("unknown undo token"));
    };
    let mut res = DeleteResult::default();
    // Never move an item over one that now has its name.
    let (todo, taken): (Vec<UndoItem>, Vec<UndoItem>) =
        items.into_iter().partition(|i| !exists(&i.path));
    #[cfg(windows)]
    let outcome = win::with_sta(|| win::shell_restore(&todo, opts));
    #[cfg(not(windows))]
    let outcome: Result<Vec<Report>> = {
        let _ = opts;
        Err(crate::Error::Unsupported(
            "the recycle bin is only available on Windows",
        ))
    };
    let reports = match outcome {
        Ok(r) => r,
        Err(e) => {
            undo.put_back(token, todo.into_iter().chain(taken).collect());
            return Err(e);
        }
    };
    let mut keep = Vec::new();
    {
        let mut t = tree.write().unwrap_or_else(PoisonError::into_inner);
        for (item, rep) in todo.into_iter().zip(reports) {
            if exists(&item.path) {
                let n = *t.node(item.node);
                let parent_live = !t.is_deleted(n.parent);
                let dup = parent_live && t.find_child(n.parent, t.name(item.node)).is_some();
                // A watcher event may already have added it again.
                if dup || t.restore_deleted(item.node) {
                    res.bytes = res.bytes.wrapping_add(if dup { 0 } else { n.size });
                    res.ok.push(item.node);
                    continue;
                }
            }
            res.failed.push(Failure {
                id: item.node,
                reason: rep.reason.unwrap_or_else(|| reasons::NOT_RESTORED.into()),
            });
            keep.push(item);
        }
    }
    for item in taken {
        res.failed.push(Failure {
            id: item.node,
            reason: reasons::NAME_TAKEN.into(),
        });
        keep.push(item);
    }
    if !keep.is_empty() {
        undo.put_back(token, keep);
    }
    Ok(res)
}

/// Empties the recycle bin of the drive holding `root` (all of it, as
/// Explorer's "Empty Recycle Bin" does for that drive), then removes what
/// disappeared from the tree's `$Recycle.Bin` folder, if the tree is that
/// drive. `bytes` is the bin's size before; `ok` lists the removed nodes.
#[cfg(windows)]
pub fn empty_recycle_bin(
    tree: &SharedTree,
    root: &Path,
    opts: &DeleteOptions,
) -> Result<DeleteResult> {
    let mut res = DeleteResult {
        bytes: win::empty_bin(root, opts)?,
        ..Default::default()
    };
    let bin = {
        let t = tree.read().unwrap_or_else(PoisonError::into_inner);
        let same_drive = is_root_name(t.name(NodeId::ROOT))
            && resolved_key(&wide(&node_path(&t, NodeId::ROOT))) == resolved_key(&wide(root));
        same_drive
            .then(|| t.find_child(NodeId::ROOT, b"$Recycle.Bin"))
            .flatten()
            .map(|b| (b, node_path(&t, b)))
    };
    if let Some((bin, path)) = bin {
        let mut gone = Vec::new();
        let mut budget = RECONCILE_LIMIT;
        reconcile(tree, bin, path, &mut gone, &mut budget);
        let mut t = tree.write().unwrap_or_else(PoisonError::into_inner);
        for id in gone {
            if t.mark_deleted(id) {
                res.ok.push(id);
            }
        }
    }
    Ok(res)
}

/// On other systems: unsupported.
#[cfg(not(windows))]
pub fn empty_recycle_bin(
    _tree: &SharedTree,
    _root: &Path,
    _opts: &DeleteOptions,
) -> Result<DeleteResult> {
    Err(crate::Error::Unsupported(
        "the recycle bin is only available on Windows",
    ))
}

/// Undo tokens of [`recycle`] and what they restore. Node ids belong to
/// one tree: call [`UndoStore::clear`] when a new scan starts. Keeps the
/// most recent [`UndoStore::CAPACITY`] tokens.
#[derive(Default)]
pub struct UndoStore {
    inner: Mutex<(u32, HashMap<u32, Vec<UndoItem>>)>,
}

/// One recycled item.
#[derive(Clone, Debug)]
struct UndoItem {
    node: NodeId,
    /// Where it was.
    path: PathBuf,
    /// The shell's absolute ID list of the item in the Recycle Bin.
    #[cfg_attr(not(windows), allow(dead_code))]
    item: Vec<u8>,
}

impl UndoStore {
    pub const CAPACITY: usize = 32;

    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets every token.
    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .clear();
    }

    /// True if `token` can still be restored.
    pub fn contains(&self, token: u32) -> bool {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.1.contains_key(&token)
    }

    fn put(&self, items: Vec<UndoItem>) -> u32 {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.0 = g.0.wrapping_add(1).max(1);
        let token = g.0;
        g.1.insert(token, items);
        if g.1.len() > Self::CAPACITY {
            // Tokens grow, so the smallest distance below the newest is the oldest.
            let oldest = *g.1.keys().max_by_key(|&&k| token.wrapping_sub(k)).unwrap();
            g.1.remove(&oldest);
        }
        token
    }

    fn put_back(&self, token: u32, items: Vec<UndoItem>) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.1.insert(token, items);
    }

    fn take(&self, token: u32) -> Option<Vec<UndoItem>> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.1.remove(&token)
    }
}

impl Tree {
    /// Undoes [`Tree::mark_deleted`]: `id` and its subtree count again and
    /// their bytes are added back to every ancestor. Returns false if `id`
    /// is the root, is not itself marked deleted, or its parent is deleted.
    pub fn restore_deleted(&mut self, id: NodeId) -> bool {
        if id == NodeId::ROOT || id.index() >= self.len() {
            return false;
        }
        let n = *self.node(id);
        if !n.is_deleted() || self.is_deleted(n.parent) {
            return false;
        }
        self.node_mut(id).flags.remove(NodeFlags::DELETED);
        self.add_to_ancestors(n.parent, n.size, n.logical);
        true
    }
}

/// Upper bound on the existence checks made inside directories that were
/// only partly deleted.
const RECONCILE_LIMIT: usize = 100_000;

/// The shared part of [`recycle`] and [`purge`].
fn remove(
    tree: &SharedTree,
    ids: &[NodeId],
    mode: DeleteMode,
    opts: &DeleteOptions,
) -> Result<(DeleteResult, Vec<UndoItem>)> {
    let mut res = DeleteResult::default();
    let mut targets: Vec<(NodeId, PathBuf, bool)> = Vec::new();
    let mut covered_ids: Vec<NodeId> = Vec::new();
    {
        let t = tree.read().unwrap_or_else(PoisonError::into_inner);
        let live = |id: NodeId| id != NodeId::ROOT && id.index() < t.len() && !t.is_deleted(id);
        let wanted: HashSet<NodeId> = ids.iter().copied().filter(|&id| live(id)).collect();
        let mut seen = HashSet::with_capacity(ids.len());
        for &id in ids {
            if !seen.insert(id) {
                continue;
            }
            if id == NodeId::ROOT {
                res.failed.push(Failure {
                    id,
                    reason: reasons::SCAN_ROOT.into(),
                });
            } else if !live(id) {
                // Invalid or already gone: nothing to do.
            } else if t.ancestors(id).any(|a| wanted.contains(&a)) {
                covered_ids.push(id); // goes with its requested ancestor
            } else {
                targets.push((id, node_path(&t, id), t.node(id).is_dir()));
            }
        }
    }

    // From here on, no lock until the tree update.
    let mut reason_of: HashMap<NodeId, String> = HashMap::new();
    targets.retain(|(id, path, _)| match lock_reason(path) {
        Some(r) => {
            reason_of.insert(*id, r.into());
            false
        }
        None => true,
    });
    let paths: Vec<PathBuf> = targets.iter().map(|(_, p, _)| p.clone()).collect();
    let reports = if paths.is_empty() {
        Vec::new()
    } else {
        delete_detailed(&paths, mode, opts)?
    };

    let mut to_mark: Vec<NodeId> = Vec::new();
    let mut items = Vec::new();
    let mut budget = RECONCILE_LIMIT;
    for ((id, path, is_dir), rep) in targets.into_iter().zip(reports) {
        if rep.gone {
            to_mark.push(id);
            if let Some(item) = rep.recycled {
                items.push(UndoItem {
                    node: id,
                    path,
                    item,
                });
            }
        } else {
            reason_of.insert(
                id,
                rep.reason.unwrap_or_else(|| reasons::STILL_THERE.into()),
            );
            if is_dir {
                reconcile(tree, id, path, &mut to_mark, &mut budget);
            }
        }
    }

    let mut t = tree.write().unwrap_or_else(PoisonError::into_inner);
    for &id in &to_mark {
        let n = *t.node(id);
        // The watcher may have marked it already; the bytes are freed anyway.
        if t.mark_deleted(id) || n.is_deleted() {
            res.bytes = res.bytes.wrapping_add(n.size);
        }
    }
    // Report every requested id once, in request order.
    let handled: HashSet<NodeId> = to_mark.into_iter().chain(covered_ids).collect();
    let mut seen = HashSet::with_capacity(ids.len());
    for &id in ids {
        if id == NodeId::ROOT || id.index() >= t.len() || !seen.insert(id) {
            continue;
        }
        if let Some(r) = reason_of.remove(&id) {
            res.failed.push(Failure { id, reason: r });
        } else if handled.contains(&id) {
            if t.is_deleted(id) {
                res.ok.push(id);
            } else {
                res.failed.push(Failure {
                    id,
                    reason: reasons::STILL_THERE.into(),
                });
            }
        }
    }
    items.retain(|i| t.is_deleted(i.node));
    Ok((res, items))
}

/// Walks the tree below `dir`, which still exists on disk after a delete,
/// and collects the descendants that no longer exist. Holds the read lock
/// only while listing one directory, never while touching the disk.
/// Returns false if the check budget ran out.
fn reconcile(
    tree: &SharedTree,
    dir: NodeId,
    dir_path: PathBuf,
    gone: &mut Vec<NodeId>,
    budget: &mut usize,
) -> bool {
    let mut stack = vec![(dir, dir_path)];
    let mut kids: Vec<(NodeId, OsString, bool)> = Vec::new();
    while let Some((d, path)) = stack.pop() {
        kids.clear();
        {
            let t = tree.read().unwrap_or_else(PoisonError::into_inner);
            if t.is_deleted(d) {
                continue;
            }
            kids.extend(
                t.children(d)
                    .map(|c| (c, name_os(t.name(c)), t.node(c).is_dir())),
            );
        }
        if kids.len() > *budget {
            return false;
        }
        *budget -= kids.len();
        for (c, name, is_dir) in kids.drain(..) {
            let p = path.join(name);
            if !exists(&p) {
                gone.push(c);
            } else if is_dir {
                stack.push((c, p));
            }
        }
    }
    true
}

/// True unless the OS positively says the path does not exist.
fn exists(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

fn name_os(name: &[u8]) -> OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&wtf8::to_utf16(name))
    }
    #[cfg(not(windows))]
    {
        OsString::from(wtf8::to_string_lossy(name))
    }
}

/// The node's path for file-system calls.
fn node_path(t: &Tree, id: NodeId) -> PathBuf {
    #[cfg(windows)]
    {
        t.path(id)
    }
    #[cfg(not(windows))]
    {
        // `Tree::path` joins with `\`; build a native path instead.
        let mut chain: Vec<NodeId> = std::iter::once(id).chain(t.ancestors(id)).collect();
        chain.reverse();
        let mut p = PathBuf::from(name_os(t.name(chain[0])));
        for &n in &chain[1..] {
            p.push(name_os(t.name(n)));
        }
        p
    }
}

// ---------------------------------------------------------------------------
// Deleting paths

/// What happened to one path.
#[derive(Clone, Debug, Default)]
struct Report {
    /// The path no longer exists.
    gone: bool,
    /// Why not, if the shell said.
    reason: Option<String>,
    /// The item in the Recycle Bin (absolute ID list), if recycled.
    recycled: Option<Vec<u8>>,
}

/// Deletes `paths` with one shell operation (one progress dialog) and
/// returns, for each path, whether it is gone afterwards. A path that did
/// not exist counts as gone, and a path below another one in the list is
/// left to the deletion of its ancestor. Blocking. Does not check
/// [`lock_reason`].
///
/// Fails if the shell operation could not be set up, or if it failed with
/// nothing deleted for any reason other than the user cancelling (for
/// example under wine, whose shell does not implement it). Otherwise
/// partial failures show up as `false` entries.
pub fn delete_paths(
    paths: &[PathBuf],
    mode: DeleteMode,
    opts: &DeleteOptions,
) -> Result<Vec<bool>> {
    Ok(delete_detailed(paths, mode, opts)?
        .into_iter()
        .map(|r| r.gone)
        .collect())
}

#[cfg(windows)]
fn delete_detailed(
    paths: &[PathBuf],
    mode: DeleteMode,
    opts: &DeleteOptions,
) -> Result<Vec<Report>> {
    let keys: Vec<Vec<u16>> = paths.iter().map(|p| resolved_key(&wide(p))).collect();
    let skip = covered(&keys);
    let queue: Vec<(usize, &PathBuf)> = paths
        .iter()
        .enumerate()
        .filter(|&(i, p)| !skip[i] && exists(p))
        .collect();
    let mut reports = vec![Report::default(); paths.len()];
    if queue.is_empty() {
        for (r, p) in reports.iter_mut().zip(paths) {
            r.gone = !exists(p);
        }
        return Ok(reports);
    }
    let qpaths: Vec<&Path> = queue.iter().map(|(_, p)| p.as_path()).collect();
    let (shell, err) = win::with_sta(|| win::shell_delete(&qpaths, mode, opts))?;
    for ((i, _), rep) in queue.iter().zip(shell) {
        reports[*i] = rep;
    }
    for (r, p) in reports.iter_mut().zip(paths) {
        r.gone = !exists(p);
        if r.gone {
            r.reason = None;
        }
    }
    match err {
        Some(e) if !queue.iter().any(|(i, _)| reports[*i].gone) => Err(e),
        _ => Ok(reports),
    }
}

/// On other systems (development only): permanent deletion with
/// `std::fs`; the recycle bin is not supported.
#[cfg(not(windows))]
fn delete_detailed(
    paths: &[PathBuf],
    mode: DeleteMode,
    _opts: &DeleteOptions,
) -> Result<Vec<Report>> {
    if mode == DeleteMode::RecycleBin {
        return Err(crate::Error::Unsupported(
            "the recycle bin is only available on Windows",
        ));
    }
    let keys: Vec<Vec<u16>> = paths.iter().map(|p| resolved_key(&wide(p))).collect();
    let mut reports = vec![Report::default(); paths.len()];
    for ((p, skip), r) in paths.iter().zip(covered(&keys)).zip(reports.iter_mut()) {
        if skip {
            continue;
        }
        let res = match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
            Ok(_) => std::fs::remove_file(p),
            Err(_) => Ok(()),
        };
        r.reason = res.err().map(|e| e.to_string());
    }
    for (r, p) in reports.iter_mut().zip(paths) {
        r.gone = !exists(p);
    }
    Ok(reports)
}

// ---------------------------------------------------------------------------
// Protection

/// Why `path` may not be deleted from the treemap, or `None` if it may:
/// a volume root or mounted folder, a protected folder (see the module
/// docs), or a folder containing one. Paths that cannot be resolved count
/// as protected. Comparison is case-insensitive and sees through 8.3 short
/// names, `\\?\` prefixes, `/` separators, `.` and `..`.
pub fn lock_reason(path: &Path) -> Option<&'static str> {
    let raw = wide(path);
    // A bare `C:` means the drive here. Windows would resolve it to the
    // current directory on that drive, which could be anything.
    if raw.is_empty() || is_drive(&raw) {
        return Some(reasons::DRIVE_ROOT);
    }
    let Some(full) = resolve(&raw) else {
        return Some(reasons::DRIVE_ROOT);
    };
    let key = path_key(&full);
    if is_root_key(&key) {
        return Some(reasons::DRIVE_ROOT);
    }
    #[cfg(windows)]
    if let Some(vol) = win::volume_path(&full) {
        if path_key(&vol) == key {
            return Some(reasons::DRIVE_ROOT); // a mounted volume
        }
    }
    let list = protected_list();
    if let Some((_, r)) = list.iter().find(|(p, _)| *p == key) {
        return Some(r);
    }
    list.iter()
        .any(|(p, _)| is_ancestor_or_self(&key, p))
        .then_some(reasons::CONTAINS)
}

/// True if [`lock_reason`] refuses `path`.
pub fn is_protected(path: &Path) -> bool {
    lock_reason(path).is_some()
}

/// The ids of the nodes [`lock_reason`] refuses, sorted: the protected
/// folders inside the tree, their ancestors, and the root if it is a volume
/// root. Costs one lookup per protected folder; used by the layout for the
/// PROTECTED flag.
pub(crate) fn protected_nodes(tree: &Tree) -> Vec<u32> {
    #[cfg(target_os = "macos")]
    if tree.posix_paths {
        return crate::scan::darwin::protected_nodes(tree);
    }
    let root_path = node_path(tree, NodeId::ROOT);
    let root_key = resolved_key(&wide(&root_path));
    let mut out = Vec::new();
    if is_root_key(&root_key)
        || protected_list()
            .iter()
            .any(|(p, _)| is_ancestor_or_self(&root_key, p))
    {
        out.push(NodeId::ROOT.0);
    }
    for (p, _) in protected_list() {
        if p.len() <= root_key.len() || !is_ancestor_or_self(&root_key, p) {
            continue;
        }
        let rel = wtf8::from_utf16(&p[root_key.len()..]);
        if let Some(id) = tree.lookup_relative(&rel) {
            out.push(id.0);
            out.extend(tree.ancestors(id).map(|a| a.0));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// True if a tree root's name (a WTF-8 path) is a volume root such as `C:\`.
pub(crate) fn is_root_name(name: &[u8]) -> bool {
    is_root_key(&path_key(&wtf8::to_utf16(name)))
}

/// Absolute form of a path (long names on Windows).
fn resolve(path: &[u16]) -> Option<Vec<u16>> {
    #[cfg(windows)]
    {
        win::resolve(path)
    }
    #[cfg(not(windows))]
    {
        let p = PathBuf::from(String::from_utf16_lossy(path));
        std::path::absolute(p).ok().map(|a| wide(&a))
    }
}

fn resolved_key(path: &[u16]) -> Vec<u16> {
    path_key(&resolve(path).unwrap_or_else(|| path.to_vec()))
}

/// Keys of the protected folders with their reasons, resolved once.
fn protected_list() -> &'static [(Vec<u16>, &'static str)] {
    static LIST: OnceLock<Vec<(Vec<u16>, &'static str)>> = OnceLock::new();
    LIST.get_or_init(|| {
        #[cfg(windows)]
        let raw = win::protected_folders();
        #[cfg(not(windows))]
        let raw: Vec<(Vec<u16>, &'static str)> = std::env::var_os("HOME")
            .map(|h| (wide(Path::new(&h)), reasons::PROFILE))
            .into_iter()
            .collect();
        let mut keys: Vec<(Vec<u16>, &'static str)> = raw
            .into_iter()
            .map(|(p, r)| (resolved_key(&p), r))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        keys.sort();
        keys.dedup_by(|a, b| a.0 == b.0);
        keys
    })
}

// ---------------------------------------------------------------------------
// Path keys (portable, pure)

const BS: u16 = b'\\' as u16;

/// A normalised comparison key for a Windows path: `\\?\`, `\??\` and
/// `\\.\` prefixes removed, `/` turned into `\`, empty and `.` components
/// dropped, `..` resolved lexically, no trailing separator, upper-cased.
/// `C:\Windows\` gives `C:\WINDOWS`, `C:\` gives `C:`, and
/// `\\?\UNC\srv\share\x` gives `\\SRV\SHARE\X`.
fn path_key(path: &[u16]) -> Vec<u16> {
    let p: Vec<u16> = path
        .iter()
        .map(|&c| if c == b'/' as u16 { BS } else { c })
        .collect();
    let (body, unc) = if has_prefix(&p, r"\\?\UNC\") || has_prefix(&p, r"\??\UNC\") {
        (&p[8..], true)
    } else if has_prefix(&p, r"\\?\") || has_prefix(&p, r"\??\") || has_prefix(&p, r"\\.\") {
        (&p[4..], false)
    } else if has_prefix(&p, r"\\") {
        (&p[2..], true)
    } else {
        (&p[..], false)
    };
    let mut comps: Vec<&[u16]> = Vec::new();
    let mut fixed = 0; // leading components `..` cannot remove
    for c in body.split(|&u| u == BS) {
        match c {
            [] | [0x2E] => {}
            [0x2E, 0x2E] => {
                if comps.len() > fixed {
                    comps.pop();
                }
            }
            _ => {
                if comps.is_empty() {
                    fixed = if unc {
                        2
                    } else if is_drive(c) {
                        1
                    } else {
                        0
                    };
                }
                comps.push(c);
            }
        }
    }
    let mut out = Vec::with_capacity(p.len());
    if unc {
        out.extend_from_slice(&[BS, BS]);
    }
    for (i, c) in comps.iter().enumerate() {
        if i > 0 || !(unc || is_drive(c)) {
            out.push(BS);
        }
        out.extend(c.iter().map(|&u| upcase(u)));
    }
    out
}

fn has_prefix(p: &[u16], prefix: &str) -> bool {
    p.len() >= prefix.len()
        && p.iter()
            .zip(prefix.encode_utf16())
            .all(|(&a, b)| upcase(a) == upcase(b))
}

fn is_drive(c: &[u16]) -> bool {
    c.len() == 2 && c[1] == b':' as u16 && c[0] < 0x80 && (c[0] as u8).is_ascii_alphabetic()
}

/// Simple (one-to-one) upper-casing of a UTF-16 unit; surrogates and
/// characters whose upper case is not a single BMP character stay as they
/// are. Close to what NTFS does with its upcase table.
fn upcase(u: u16) -> u16 {
    if u < 0x80 {
        return (u as u8).to_ascii_uppercase() as u16;
    }
    let Some(c) = char::from_u32(u as u32) else {
        return u; // a surrogate
    };
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(x), None) if (x as u32) < 0x1_0000 => x as u32 as u16,
        _ => u,
    }
}

/// True if `anc` is `key` or one of its ancestors (both from [`path_key`]).
fn is_ancestor_or_self(anc: &[u16], key: &[u16]) -> bool {
    key.starts_with(anc) && (key.len() == anc.len() || key[anc.len()] == BS)
}

/// True for a key that names the root of a drive or share (or no path).
fn is_root_key(key: &[u16]) -> bool {
    if key.is_empty() || is_drive(key) {
        return true;
    }
    if key.starts_with(&[BS, BS]) {
        // `\\server\share` and shorter
        return key[2..].iter().filter(|&&u| u == BS).count() < 2;
    }
    false
}

/// For each key, whether it duplicates an earlier key or lies below another
/// key in the list (so deleting the others deletes it too).
fn covered(keys: &[Vec<u16>]) -> Vec<bool> {
    let all: HashSet<&[u16]> = keys.iter().map(|k| k.as_slice()).collect();
    let mut seen: HashSet<&[u16]> = HashSet::with_capacity(keys.len());
    keys.iter()
        .map(|k| {
            let dup = !seen.insert(k.as_slice());
            dup || k
                .iter()
                .enumerate()
                .any(|(i, &u)| u == BS && i > 0 && all.contains(&k[..i]))
        })
        .collect()
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().collect()
}

#[cfg(not(windows))]
fn wide(path: &Path) -> Vec<u16> {
    path.to_string_lossy().encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// Windows

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND, RPC_E_CHANGED_MODE};
    use windows::Win32::Storage::FileSystem::{
        GetFullPathNameW, GetLongPathNameW, GetTempPathW, GetVolumePathNameW,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::*;
    use windows::core::{GUID, HRESULT, PCWSTR, Ref, implement};

    use super::{DeleteMode, DeleteOptions, Report, UndoItem, reasons, wide};

    /// Known folders that may not be deleted, with their reasons.
    const PROTECTED: &[(GUID, &str)] = &[
        (FOLDERID_Windows, reasons::WINDOWS),
        (FOLDERID_System, reasons::WINDOWS),
        (FOLDERID_SystemX86, reasons::WINDOWS),
        (FOLDERID_ProgramFiles, reasons::PROGRAMS),
        (FOLDERID_ProgramFilesX86, reasons::PROGRAMS),
        (FOLDERID_ProgramFilesX64, reasons::PROGRAMS),
        (FOLDERID_ProgramFilesCommon, reasons::PROGRAMS),
        (FOLDERID_ProgramFilesCommonX86, reasons::PROGRAMS),
        (FOLDERID_ProgramFilesCommonX64, reasons::PROGRAMS),
        (FOLDERID_ProgramData, reasons::APP_DATA),
        (FOLDERID_UserProfiles, reasons::PROFILES),
        (FOLDERID_Public, reasons::PROFILE),
        (FOLDERID_Profile, reasons::PROFILE),
        (FOLDERID_Desktop, reasons::PERSONAL),
        (FOLDERID_Documents, reasons::PERSONAL),
        (FOLDERID_Downloads, reasons::PERSONAL),
        (FOLDERID_Pictures, reasons::PERSONAL),
        (FOLDERID_Music, reasons::PERSONAL),
        (FOLDERID_Videos, reasons::PERSONAL),
        (FOLDERID_SkyDrive, reasons::PERSONAL),
        (FOLDERID_RoamingAppData, reasons::APP_DATA),
        (FOLDERID_LocalAppData, reasons::APP_DATA),
        (FOLDERID_LocalAppDataLow, reasons::APP_DATA),
    ];

    /// The protected folders' paths (unresolved) and reasons.
    pub(super) fn protected_folders() -> Vec<(Vec<u16>, &'static str)> {
        PROTECTED
            .iter()
            .filter_map(|(id, r)| known_folder(id).map(|p| (p, *r)))
            .chain(fill(|buf| unsafe { GetTempPathW(Some(buf)) }).map(|p| (p, reasons::TEMP)))
            .collect()
    }

    fn known_folder(id: &GUID) -> Option<Vec<u16>> {
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None).ok()?;
            let v = p.as_wide().to_vec();
            CoTaskMemFree(Some(p.0 as *const _));
            (!v.is_empty()).then_some(v)
        }
    }

    /// Calls a Win32 function that fills a UTF-16 buffer and returns the
    /// length written, or the size needed (including the NUL) when the
    /// buffer is too small, or 0 on failure.
    fn fill(mut f: impl FnMut(&mut [u16]) -> u32) -> Option<Vec<u16>> {
        let mut buf = vec![0u16; 300];
        loop {
            let n = f(&mut buf) as usize;
            if n == 0 {
                return None;
            }
            if n < buf.len() {
                buf.truncate(n);
                return Some(buf);
            }
            buf.resize(n + 1, 0);
        }
    }

    fn nul(units: &[u16]) -> Vec<u16> {
        let mut v = Vec::with_capacity(units.len() + 1);
        v.extend_from_slice(units);
        v.push(0);
        v
    }

    /// Absolute path with long (not 8.3) names where the path exists.
    pub(super) fn resolve(path: &[u16]) -> Option<Vec<u16>> {
        let p = nul(path);
        let full = fill(|buf| unsafe { GetFullPathNameW(PCWSTR(p.as_ptr()), Some(buf), None) })?;
        let f = nul(&full);
        Some(fill(|buf| unsafe { GetLongPathNameW(PCWSTR(f.as_ptr()), Some(buf)) }).unwrap_or(full))
    }

    /// The root of the volume holding `path` (with a trailing `\`).
    pub(super) fn volume_path(path: &[u16]) -> Option<Vec<u16>> {
        let p = nul(path);
        let mut buf = vec![0u16; path.len().max(260) + 2];
        unsafe { GetVolumePathNameW(PCWSTR(p.as_ptr()), &mut buf).ok()? };
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        buf.truncate(n);
        Some(buf)
    }

    /// Calls `CoUninitialize` when dropped.
    struct ComGuard;

    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    /// Joins the single-threaded apartment, or returns `None` if this
    /// thread already belongs to the multithreaded one.
    fn enter_sta() -> windows::core::Result<Option<ComGuard>> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(None);
        }
        hr.ok()?; // S_OK, or S_FALSE if already initialised; both need CoUninitialize
        Ok(Some(ComGuard))
    }

    /// Runs `f` in a single-threaded COM apartment: on this thread if
    /// possible, else on a helper thread.
    pub(super) fn with_sta<R: Send>(
        f: impl FnOnce() -> crate::Result<R> + Send,
    ) -> crate::Result<R> {
        if let Some(_guard) = enter_sta()? {
            return f();
        }
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let _guard = enter_sta()?;
                f()
            });
            worker
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
        })
    }

    /// A path the shell can parse: no `\\?\` prefix, `\` separators,
    /// NUL-terminated.
    fn shell_path(path: &Path) -> Vec<u16> {
        let w = wide(path);
        let s = |p: &str| -> Vec<u16> { p.encode_utf16().collect() };
        let mut out: Vec<u16> = if w.starts_with(&s(r"\\?\UNC\")) {
            let mut v = s(r"\\");
            v.extend_from_slice(&w[8..]);
            v
        } else if w.starts_with(&s(r"\\?\")) && w.get(5) == Some(&(b':' as u16)) {
            w[4..].to_vec()
        } else {
            w.iter()
                .map(|&c| if c == b'/' as u16 { b'\\' as u16 } else { c })
                .collect()
        };
        out.push(0);
        out
    }

    fn reason(hr: HRESULT) -> Option<String> {
        if hr == COPYENGINE_E_USER_CANCELLED || hr == HRESULT::from_win32(ERROR_CANCELLED.0) {
            return Some(reasons::CANCELLED.into());
        }
        if hr == COPYENGINE_S_USER_IGNORED || hr == COPYENGINE_S_DONT_PROCESS_CHILDREN {
            return Some(reasons::SKIPPED.into());
        }
        if hr.is_err() {
            let msg = hr.message();
            let msg = msg.trim();
            return Some(if msg.is_empty() {
                format!("Error 0x{:08X}.", hr.0 as u32)
            } else {
                msg.to_string()
            });
        }
        None
    }

    /// The absolute ID list of a shell item, as bytes.
    fn id_list(item: &IShellItem) -> Option<Vec<u8>> {
        unsafe {
            let p = SHGetIDListFromObject(item).ok()?;
            let n = ILGetSize(Some(p)) as usize;
            let v = (n > 0).then(|| std::slice::from_raw_parts(p as *const u8, n).to_vec());
            ILFree(Some(p));
            v
        }
    }

    /// What the shell reported for one queued item.
    #[derive(Clone, Default)]
    struct SinkResult {
        hr: Option<HRESULT>,
        created: Option<Vec<u8>>,
        matched: bool,
    }

    /// Per-item progress sink: records the item's result and, for a
    /// recycle, the item it became in the Recycle Bin.
    #[implement(IFileOperationProgressSink)]
    struct Sink {
        item: IShellItem,
        out: Rc<RefCell<Vec<SinkResult>>>,
        index: usize,
    }

    impl Sink {
        fn record(&self, item: Ref<IShellItem>, hr: HRESULT, created: Ref<IShellItem>) {
            let same = item.ok().is_ok_and(|i| {
                matches!(
                    unsafe { self.item.Compare(i, SICHINT_CANONICAL.0 as u32) },
                    Ok(0)
                )
            });
            let mut out = self.out.borrow_mut();
            let slot = &mut out[self.index];
            // The first report counts unless a later one is surely ours.
            if slot.matched || (slot.hr.is_some() && !same) {
                return;
            }
            *slot = SinkResult {
                hr: Some(hr),
                created: created.ok().ok().and_then(id_list),
                matched: same,
            };
        }
    }

    #[allow(non_snake_case)]
    impl IFileOperationProgressSink_Impl for Sink_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            item: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            hr: HRESULT,
            created: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            self.record(item, hr, created);
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(&self, _: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            item: Ref<IShellItem>,
            hr: HRESULT,
            created: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            self.record(item, hr, created);
            Ok(())
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    fn new_operation(
        flags: FILEOPERATION_FLAGS,
        opts: &DeleteOptions,
    ) -> windows::core::Result<IFileOperation> {
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(flags)?;
            if opts.owner_hwnd != 0 {
                op.SetOwnerWindow(HWND(opts.owner_hwnd as *mut _))?;
            }
            Ok(op)
        }
    }

    fn reports(results: &RefCell<Vec<SinkResult>>, mode_recycle: bool) -> Vec<Report> {
        results
            .borrow()
            .iter()
            .map(|r| Report {
                gone: false, // checked on disk by the caller
                reason: r.hr.and_then(reason),
                recycled: if mode_recycle {
                    r.created.clone()
                } else {
                    None
                },
            })
            .collect()
    }

    /// Queues one delete per path and performs them as one operation.
    /// Paths the shell cannot parse are skipped (they stay on disk).
    /// Returns the per-item reports and, separately, the error of the
    /// operation as a whole (not for a cancel), if any.
    pub(super) fn shell_delete(
        paths: &[&Path],
        mode: DeleteMode,
        opts: &DeleteOptions,
    ) -> crate::Result<(Vec<Report>, Option<crate::Error>)> {
        let mut flags = FOF_NOCONFIRMATION;
        if mode == DeleteMode::RecycleBin {
            flags |= FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE;
            if !opts.silent {
                flags |= FOF_WANTNUKEWARNING;
            }
        }
        if opts.silent {
            flags |= FOF_SILENT | FOF_NOERRORUI;
        }
        let op = new_operation(flags, opts)?;
        let results = Rc::new(RefCell::new(vec![SinkResult::default(); paths.len()]));
        let mut queued = 0usize;
        let mut first_err = None;
        for (index, p) in paths.iter().enumerate() {
            let sp = shell_path(p);
            let item: IShellItem =
                match unsafe { SHCreateItemFromParsingName(PCWSTR(sp.as_ptr()), None) } {
                    Ok(i) => i,
                    Err(e) => {
                        results.borrow_mut()[index].hr = Some(e.code());
                        first_err.get_or_insert(e);
                        continue;
                    }
                };
            let sink: IFileOperationProgressSink = Sink {
                item: item.clone(),
                out: results.clone(),
                index,
            }
            .into();
            unsafe { op.DeleteItem(&item, &sink)? };
            queued += 1;
        }
        if queued == 0 {
            return Err(first_err.unwrap_or_else(windows::core::Error::empty).into());
        }
        let done = unsafe { op.PerformOperations() };
        drop(op);
        let out = reports(&results, mode == DeleteMode::RecycleBin);
        Ok((out, done.err().filter(|e| !is_cancel(e)).map(Into::into)))
    }

    fn is_cancel(e: &windows::core::Error) -> bool {
        e.code() == COPYENGINE_E_USER_CANCELLED
            || e.code() == HRESULT::from_win32(ERROR_CANCELLED.0)
    }

    /// Moves recycled items back to their original folders.
    pub(super) fn shell_restore(
        items: &[UndoItem],
        opts: &DeleteOptions,
    ) -> crate::Result<Vec<Report>> {
        let mut out = vec![Report::default(); items.len()];
        if items.is_empty() {
            return Ok(out);
        }
        let flags = if opts.silent {
            FOF_SILENT | FOF_NOERRORUI | FOF_NOCONFIRMATION | FOF_NOCONFIRMMKDIR
        } else {
            FILEOPERATION_FLAGS(0)
        };
        let op = new_operation(flags, opts)?;
        let results = Rc::new(RefCell::new(vec![SinkResult::default(); items.len()]));
        let mut queued = 0usize;
        for (index, it) in items.iter().enumerate() {
            let (Some(parent), Some(name)) = (it.path.parent(), it.path.file_name()) else {
                continue;
            };
            let dest = shell_path(parent);
            let name: Vec<u16> = wide(Path::new(name)).into_iter().chain(Some(0)).collect();
            let queued_ok = unsafe {
                let item: windows::core::Result<IShellItem> =
                    SHCreateItemFromIDList(it.item.as_ptr() as *const ITEMIDLIST);
                let folder: windows::core::Result<IShellItem> =
                    SHCreateItemFromParsingName(PCWSTR(dest.as_ptr()), None);
                match (item, folder) {
                    (Ok(item), Ok(folder)) => {
                        let sink: IFileOperationProgressSink = Sink {
                            item: item.clone(),
                            out: results.clone(),
                            index,
                        }
                        .into();
                        op.MoveItem(&item, &folder, PCWSTR(name.as_ptr()), &sink)
                            .is_ok()
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        out[index].reason = reason(e.code());
                        false
                    }
                }
            };
            queued += usize::from(queued_ok);
        }
        if queued > 0 {
            let done = unsafe { op.PerformOperations() };
            drop(op);
            // Items without a reason of their own get the operation's; the
            // caller only looks at the reason if the item is not back.
            let overall = done.err().and_then(|e| reason(e.code()));
            for (o, r) in out.iter_mut().zip(reports(&results, false)) {
                if r.reason.is_some() {
                    o.reason = r.reason;
                }
                if o.reason.is_none() {
                    o.reason.clone_from(&overall);
                }
            }
        }
        Ok(out)
    }

    /// Empties the recycle bin of the drive holding `root`; returns the
    /// bin's size before.
    pub(super) fn empty_bin(root: &Path, opts: &DeleteOptions) -> crate::Result<u64> {
        let p = shell_path(root);
        let mut info = SHQUERYRBINFO {
            cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
            ..Default::default()
        };
        unsafe { SHQueryRecycleBinW(PCWSTR(p.as_ptr()), &mut info)? };
        if info.i64NumItems == 0 {
            return Ok(0);
        }
        let mut flags = SHERB_NOCONFIRMATION;
        if opts.silent {
            flags |= SHERB_NOPROGRESSUI | SHERB_NOSOUND;
        }
        let owner = opts.owner_hwnd;
        with_sta(|| {
            let hwnd = (owner != 0).then_some(HWND(owner as *mut _));
            unsafe { SHEmptyRecycleBinW(hwnd, PCWSTR(p.as_ptr()), flags)? };
            Ok(())
        })?;
        Ok(info.i64Size.max(0) as u64)
    }
}

#[cfg(test)]
mod tests;
