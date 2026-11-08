//! C ABI for the native shell. The full tree and layout caches stay in Rust.
//! All calls except cancel/release run on the shell's serial worker queue.
#![cfg(target_os = "macos")]
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    time::Instant,
};
use tessera_core::{
    NodeId, SharedTree, Tree,
    layout::{self, LayoutEngine, LayoutRequest, RootExtras, SizeMode},
    scan::{self, ScanOptions, ScanProgress},
    totals::TotalsCache,
};

struct Session {
    tree: SharedTree,
    progress: ScanProgress,
    done: AtomicBool,
    rev: AtomicU32,
    elapsed: AtomicU64,
    error: Mutex<Option<String>>,
    previous: Mutex<Vec<u8>>,
    engine: Mutex<LayoutEngine>,
    totals: Mutex<TotalsCache>,
    started: Instant,
}
struct Handle(Arc<Session>);
#[repr(C)]
pub struct Buffer {
    pub ptr: *mut u8,
    pub len: usize,
}
impl Buffer {
    fn new(bytes: Vec<u8>) -> Self {
        let boxed = bytes.into_boxed_slice();
        let len = boxed.len();
        Self {
            ptr: Box::into_raw(boxed).cast(),
            len,
        }
    }
    fn json(value: Value) -> Self {
        Self::new(serde_json::to_vec(&value).unwrap_or_default())
    }
}
unsafe fn session<'a>(handle: *const Handle) -> Option<&'a Session> {
    unsafe { handle.as_ref().map(|h| h.0.as_ref()) }
}
unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}
fn mode(logical: bool) -> SizeMode {
    if logical {
        SizeMode::Logical
    } else {
        SizeMode::Disk
    }
}

/// # Safety
/// `path` points to `len` readable bytes for the duration of this call. The returned handle has one owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_create(
    path: *const u8,
    len: usize,
    threads: u32,
) -> *mut std::ffi::c_void {
    let path = Path::new(OsStr::from_bytes(unsafe { bytes(path, len) })).to_path_buf();
    let s = Arc::new(Session {
        tree: Tree::for_path(&path).into_shared(),
        progress: ScanProgress::new(),
        done: AtomicBool::new(false),
        rev: AtomicU32::new(0),
        elapsed: AtomicU64::new(0),
        previous: Mutex::new(Vec::new()),
        error: Mutex::new(None),
        engine: Mutex::new(LayoutEngine::new()),
        totals: Mutex::new(TotalsCache::new()),
        started: Instant::now(),
    });
    let worker = s.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("tessera-scan".into())
        .spawn(move || {
            if let Err(e) = scan::scan(
                &path,
                &ScanOptions {
                    threads: threads as usize,
                    ..Default::default()
                },
                &worker.tree,
                &worker.progress,
            ) {
                *worker.error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(if matches!(e, tessera_core::Error::Cancelled) {
                        "cancelled".into()
                    } else {
                        e.to_string()
                    });
            }
            worker
                .elapsed
                .store(worker.started.elapsed().as_millis() as u64, Relaxed);
            worker.rev.fetch_add(1, Relaxed);
            worker
                .done
                .store(true, std::sync::atomic::Ordering::Release);
        })
    {
        *s.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
        s.done.store(true, Relaxed);
    }
    Box::into_raw(Box::new(Handle(s))).cast()
}

/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_cancel(h: *const std::ffi::c_void) {
    if let Some(s) = unsafe { session(h.cast()) } {
        s.progress.cancel();
    }
}
/// # Safety
/// `h` is null or an owned handle from `ts_create`; no call may use it after release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_release(h: *mut std::ffi::c_void) {
    if !h.is_null() {
        let h = unsafe { Box::from_raw(h.cast::<Handle>()) };
        h.0.progress.cancel();
    }
}
/// # Safety
/// `buf` was returned by this ABI and has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_free(buf: Buffer) {
    if !buf.ptr.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buf.ptr, buf.len)) });
    }
}

/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_status(h: *const std::ffi::c_void) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(json!({"error":"closed"}));
    };
    let _policy = scan::darwin::NoMaterialize::new().ok();
    let p = s.progress.snapshot();
    let path = s
        .tree
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .path(NodeId::ROOT);
    let volume = if _policy.is_some() {
        scan::volume_info(&path).ok()
    } else {
        None
    };
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    let n = t.node(NodeId::ROOT);
    let done = s.done.load(std::sync::atomic::Ordering::Acquire);
    let totals = if done {
        Some(
            s.totals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&t, NodeId::ROOT),
        )
    } else {
        None
    };
    Buffer::json(
        json!({ "files":totals.map_or(p.files,|v|v.files), "dirs":totals.map_or(p.dirs,|v|v.dirs), "scanned":n.size, "logical":n.logical,
        "errors":p.inaccessible, "nodes":t.len(), "treeBytes":if done { t.memory().total() } else { 0 }, "elapsedMs":if done { s.elapsed.load(Relaxed) } else { s.started.elapsed().as_millis() as u64 },
        "done":s.done.load(std::sync::atomic::Ordering::Acquire), "rev":s.rev.load(Relaxed),
        "error":*s.error.lock().unwrap_or_else(|e| e.into_inner()),
        "capacity":volume.as_ref().map_or(0,|v|v.total_bytes), "free":volume.as_ref().map_or(0,|v|v.free_bytes) }),
    )
}
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_layout(
    h: *const std::ffi::c_void,
    view: u32,
    width: f32,
    height: f32,
    logical: bool,
    force: bool,
) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::new(vec![]);
    };
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.
        || height <= 0.
        || width > 16384.
        || height > 16384.
    {
        return Buffer::new(vec![]);
    }
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    let mut req = LayoutRequest::new(NodeId(view), width, height);
    req.size_mode = mode(logical);
    let data = s.engine.lock().unwrap_or_else(|e| e.into_inner()).layout(
        &t,
        &req,
        RootExtras::default(),
        s.rev.load(Relaxed),
    );
    let mut previous = s.previous.lock().unwrap_or_else(|e| e.into_inner());
    if !force && layout::same_picture(&previous, &data, 0.5) {
        return Buffer::new(vec![]);
    }
    previous.clear();
    previous.extend_from_slice(&data);
    Buffer::new(data)
}
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_tile_name(h: *const std::ffi::c_void, id: u32) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::new(Vec::new());
    };
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    let id = NodeId(id);
    if id.index() >= t.len() || t.is_deleted(id) {
        return Buffer::new(Vec::new());
    }
    Buffer::new(t.name_lossy(id).into_bytes())
}

fn details(s: &Session, id: NodeId) -> Value {
    let Ok(_policy) = scan::darwin::NoMaterialize::new() else {
        return Value::Null;
    };
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    if id.index() >= t.len() || t.is_deleted(id) {
        return Value::Null;
    }
    let n = *t.node(id);
    let path = t.path(id);
    let name = t.name_lossy(id);
    let display = t.path_lossy(id);
    let identity = t.native_identity(id);
    let totals = if n.is_dir() {
        Some(
            s.totals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&t, id),
        )
    } else {
        None
    };
    drop(t);
    let protection = scan::darwin::lock_reason(&path);
    json!({ "id":id.0, "name":name, "path":display, "pathBytes":path.as_os_str().as_bytes(),
        "disk":n.size, "logical":n.logical, "dir":n.is_dir(), "flags":n.flags.0,
        "modified":tessera_core::tree::mtime_to_unix_ms(n.mtime), "lockReason":protection,
        "files":totals.map_or(1,|v|v.files), "dirs":totals.map_or(0,|v|v.dirs),
        "parent":if id == NodeId::ROOT { None } else { Some(n.parent.0) }, "identity":identity.map(|(dev,ino)|json!({"device":dev,"inode":ino})) })
}
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_details(h: *const std::ffi::c_void, id: u32) -> Buffer {
    Buffer::json(unsafe { session(h.cast()) }.map_or(Value::Null, |s| details(s, NodeId(id))))
}
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_nav(h: *const std::ffi::c_void, id: u32, logical: bool) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(Value::Null);
    };
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    let nav = layout::nav(&t, NodeId(id), mode(logical));
    Buffer::json(
        json!({"up":nav.up.map(|id|id.0), "crumbs":nav.crumbs.iter().map(|(id,name)| json!({"id":id.0,"name":name})).collect::<Vec<_>>() }),
    )
}
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_children(
    h: *const std::ffi::c_void,
    id: u32,
    offset: u32,
    logical: bool,
) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(json!([]));
    };
    let ids = {
        let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
        if id as usize >= t.len() {
            return Buffer::json(json!([]));
        }
        let mut ids = t.children(NodeId(id)).collect::<Vec<_>>();
        ids.sort_unstable_by_key(|&id| std::cmp::Reverse(mode(logical).of(t.node(id))));
        ids.into_iter()
            .skip(offset as usize)
            .take(100)
            .collect::<Vec<_>>()
    };
    Buffer::json(Value::Array(
        ids.into_iter().map(|id| details(s, id)).collect(),
    ))
}
/// # Safety
/// `h` is a live session and `path` points to `len` readable filesystem bytes during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_refresh(
    h: *const std::ffi::c_void,
    path: *const u8,
    len: usize,
) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(json!({"error":"closed"}));
    };
    if !s.done.load(std::sync::atomic::Ordering::Acquire) {
        return Buffer::json(json!({"error":"scanning"}));
    }
    let path = Path::new(OsStr::from_bytes(unsafe { bytes(path, len) }));
    match scan::darwin::refresh(&s.tree, path) {
        Ok(changed) => {
            if changed {
                s.rev.fetch_add(1, Relaxed);
            }
            Buffer::json(json!({"changed":changed,"rev":s.rev.load(Relaxed)}))
        }
        Err(e) => Buffer::json(json!({"error":e.to_string()})),
    }
}
/// Revalidate the identity recorded during enumeration, outside the tree lock.
/// # Safety
/// `h` is null or a live handle returned by `ts_create`, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_validate(h: *const std::ffi::c_void, id: u32) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(json!({"error":"closed"}));
    };
    if !s.done.load(std::sync::atomic::Ordering::Acquire) {
        return Buffer::json(json!({"error":"scanning"}));
    }
    let value = details(s, NodeId(id));
    let Some(bytes) = value["pathBytes"].as_array() else {
        return Buffer::json(json!({"error":"staleTarget"}));
    };
    if !value["lockReason"].is_null() {
        return Buffer::json(json!({"error":value["lockReason"]}));
    }
    let path_bytes = bytes
        .iter()
        .filter_map(|v| v.as_u64().map(|x| x as u8))
        .collect::<Vec<_>>();
    let path = Path::new(OsStr::from_bytes(&path_bytes));
    let check = std::fs::symlink_metadata(path);
    match check {
        Ok(meta)
            if value["identity"]["device"].as_i64() == Some(meta.dev() as i32 as i64)
                && value["identity"]["inode"].as_u64() == Some(meta.ino()) =>
        {
            Buffer::json(value)
        }
        _ => Buffer::json(json!({"error":"staleTarget"})),
    }
}

/// # Safety
/// `path` has `len` readable bytes. `callback` and its context must remain valid until `ts_watch_release` completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_watch_create(
    path: *const u8,
    len: usize,
    callback: unsafe extern "C" fn(*const u8, usize, u32, *mut std::ffi::c_void),
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    let logical_root = Path::new(OsStr::from_bytes(unsafe { bytes(path, len) })).to_path_buf();
    let Ok(root) = std::fs::canonicalize(&logical_root) else {
        return std::ptr::null_mut();
    };
    let physical_root = root.clone();
    let context = context as usize;
    match tessera_core::watch::darwin::Stream::new(&root, move |path, flags| {
        let logical_path;
        let path = if physical_root != logical_root {
            match path.strip_prefix(&physical_root) {
                Ok(relative) => {
                    logical_path = logical_root.join(relative);
                    &logical_path
                }
                Err(_) => path,
            }
        } else {
            path
        };
        let bytes = path.as_os_str().as_bytes();
        unsafe {
            callback(
                bytes.as_ptr(),
                bytes.len(),
                flags,
                context as *mut std::ffi::c_void,
            )
        }
    }) {
        Ok(stream) => Box::into_raw(Box::new(stream)).cast(),
        Err(_) => std::ptr::null_mut(),
    }
}
/// # Safety
/// `stream` is null or a live owned stream. Do not call this from its event callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_watch_release(stream: *mut std::ffi::c_void) {
    if !stream.is_null() {
        drop(unsafe { Box::from_raw(stream.cast::<tessera_core::watch::darwin::Stream>()) })
    }
}

/// # Safety
/// `h` is null or a live session handle, not concurrently released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_categories(
    h: *const std::ffi::c_void,
    id: u32,
    logical: bool,
) -> Buffer {
    let Some(s) = (unsafe { session(h.cast()) }) else {
        return Buffer::json(json!([]));
    };
    let t = s.tree.read().unwrap_or_else(|e| e.into_inner());
    let id = NodeId(id);
    if id.index() >= t.len() || t.is_deleted(id) {
        return Buffer::json(json!([]));
    }
    let total = s
        .totals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&t, id);
    let mut categories = total
        .bytes(mode(logical))
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, bytes)| bytes > 0)
        .collect::<Vec<_>>();
    categories.sort_unstable_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
    Buffer::json(Value::Array(
        categories
            .into_iter()
            .map(|(id, bytes)| json!({"id":id,"bytes":bytes}))
            .collect(),
    ))
}
