#![cfg(target_os = "macos")]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, symlink},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tessera_core::{
    NodeFlags, NodeId, Tree,
    scan::{self, ScanMode, ScanOptions, ScanProgress},
};
static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "tessera-darwin-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                + UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(path: &Path, threads: usize) -> tessera_core::SharedTree {
    let t = Tree::for_path(path).into_shared();
    scan::scan(
        path,
        &ScanOptions {
            threads,
            ..Default::default()
        },
        &t,
        &ScanProgress::new(),
    )
    .unwrap();
    t.read().unwrap().check_invariants().unwrap();
    t
}
#[test]
fn bulk_scan_preserves_identity_paths_and_hardlinks() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("目录")).unwrap();
    fs::write(f.0.join("目录/file"), vec![1; 9001]).unwrap();
    fs::hard_link(f.0.join("目录/file"), f.0.join("second")).unwrap();
    fs::write(f.0.join("a\\b"), b"literal backslash").unwrap();
    fs::write(f.0.join("Case"), b"upper").unwrap();
    symlink(&f.0, f.0.join("cycle")).unwrap();
    for threads in [1, 2, 8] {
        let t = run(&f.0, threads);
        let t = t.read().unwrap();
        assert_eq!(t.len(), 7);
        let link = t.lookup_relative(b"cycle").unwrap();
        assert!(t.node(link).flags.contains(NodeFlags::REPARSE));
        let id = t.lookup_relative(b"a\\b").unwrap();
        assert_eq!(t.path(id), f.0.join("a\\b"));
        assert!(t.lookup_relative(b"case").is_none());
        let file = t.lookup_relative("目录/file".as_bytes()).unwrap();
        let second = t.lookup_relative(b"second").unwrap();
        assert_eq!(t.node(file).logical + t.node(second).logical, 9001);
        assert_eq!(t.native_identity(file), t.native_identity(second));
        assert_eq!(
            t.native_identity(id).unwrap().1,
            fs::symlink_metadata(t.path(id)).unwrap().ino()
        );
        assert!(t.node(NodeId::ROOT).size > 0);
    }
}
#[test]
fn filenames_roundtrip_raw_bytes() {
    let f = Fixture::new();
    let name = std::ffi::OsStr::from_bytes(b"invalid-\xff");
    let written = fs::write(f.0.join(name), b"data");
    if let Err(e) = written {
        assert_eq!(e.raw_os_error(), Some(libc::EILSEQ)); // APFS rejects invalid UTF-8 at creation.
        let mut t = Tree::for_path(&f.0);
        let id = t.add_child(NodeId::ROOT, name.as_bytes(), 4, 4, NodeFlags::NONE, 0);
        assert_eq!(t.path(id), f.0.join(name));
        return;
    }
    let t = run(&f.0, 1);
    let t = t.read().unwrap();
    let id = t.lookup_relative(name.as_bytes()).unwrap();
    assert_eq!(t.path(id), f.0.join(name));
}
#[test]
fn cancellation_and_mft_are_explicit() {
    let f = Fixture::new();
    let t = Tree::for_path(&f.0).into_shared();
    let p = ScanProgress::new();
    p.cancel();
    assert!(matches!(
        scan::scan(&f.0, &ScanOptions::default(), &t, &p),
        Err(tessera_core::Error::Cancelled)
    ));
    assert!(matches!(
        scan::scan(
            &f.0,
            &ScanOptions {
                mode: ScanMode::Mft,
                threads: 1
            },
            &t,
            &ScanProgress::new()
        ),
        Err(tessera_core::Error::Unsupported(_))
    ));
    let v = scan::volume_info(&f.0).unwrap();
    assert!(v.total_bytes > v.free_bytes);
    assert_eq!(v.file_system, "apfs");
}

#[test]
fn refresh_updates_only_dirty_directories_and_preserves_ids() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("folder")).unwrap();
    fs::write(f.0.join("stable"), b"stable").unwrap();
    fs::write(f.0.join("folder/file"), b"before").unwrap();
    let tree = run(&f.0, 4);
    let stable = tree.read().unwrap().lookup_relative(b"stable").unwrap();
    let file = tree
        .read()
        .unwrap()
        .lookup_relative(b"folder/file")
        .unwrap();
    assert!(!scan::darwin::refresh(&tree, &f.0).unwrap());
    fs::write(f.0.join("folder/file"), vec![3; 16385]).unwrap();
    assert!(scan::darwin::refresh(&tree, &f.0.join("folder")).unwrap());
    {
        let t = tree.read().unwrap();
        assert_eq!(t.lookup_relative(b"stable"), Some(stable));
        assert_eq!(t.lookup_relative(b"folder/file"), Some(file));
        assert_eq!(t.node(file).logical, 16385);
        t.check_invariants().unwrap();
    }
    fs::rename(f.0.join("folder/file"), f.0.join("folder/renamed")).unwrap();
    scan::darwin::refresh(&tree, &f.0.join("folder")).unwrap();
    assert_eq!(
        tree.read().unwrap().lookup_relative(b"folder/renamed"),
        Some(file)
    );
    fs::create_dir_all(f.0.join("new/sub")).unwrap();
    fs::write(f.0.join("new/sub/data"), b"new").unwrap();
    scan::darwin::refresh(&tree, &f.0).unwrap();
    assert!(
        tree.read()
            .unwrap()
            .lookup_relative(b"new/sub/data")
            .is_some()
    );
    fs::remove_dir_all(f.0.join("folder")).unwrap();
    scan::darwin::refresh(&tree, &f.0).unwrap();
    tree.read().unwrap().check_invariants().unwrap();
}

#[test]
fn hardlink_owner_is_transferred_when_counted_name_disappears() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("one")).unwrap();
    fs::create_dir(f.0.join("two")).unwrap();
    fs::write(f.0.join("one/file"), vec![1; 12001]).unwrap();
    fs::hard_link(f.0.join("one/file"), f.0.join("two/link")).unwrap();
    let tree = run(&f.0, 1);
    let (owner, other) = {
        let t = tree.read().unwrap();
        let one = t.lookup_relative(b"one/file").unwrap();
        let two = t.lookup_relative(b"two/link").unwrap();
        if t.node(one).logical > 0 {
            (t.path(one), two)
        } else {
            (t.path(two), one)
        }
    };
    fs::remove_file(&owner).unwrap();
    scan::darwin::refresh(&tree, owner.parent().unwrap()).unwrap();
    let t = tree.read().unwrap();
    assert_eq!(t.node(NodeId::ROOT).logical, 12001);
    assert_eq!(t.node(other).logical, 12001);
    t.check_invariants().unwrap();
}

#[test]
fn replacing_same_name_changes_identity_without_overwriting_old_node() {
    let f = Fixture::new();
    fs::write(f.0.join("file"), b"old").unwrap();
    let tree = run(&f.0, 1);
    let old = tree.read().unwrap().lookup_relative(b"file").unwrap();
    fs::write(f.0.join("replacement"), b"new").unwrap();
    fs::rename(f.0.join("replacement"), f.0.join("file")).unwrap();
    scan::darwin::refresh(&tree, &f.0).unwrap();
    let t = tree.read().unwrap();
    let new = t.lookup_relative(b"file").unwrap();
    assert_ne!(new, old);
    assert!(t.is_deleted(old));
    t.check_invariants().unwrap();
}

#[test]
fn fsevents_refreshes_and_stops_without_polling() {
    use std::{sync::mpsc, time::Duration};
    use tessera_core::watch::{WatchOptions, Watcher};
    let f = Fixture::new();
    fs::write(f.0.join("file"), b"old").unwrap();
    let tree = run(&f.0, 1);
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(
        &f.0,
        tree.clone(),
        WatchOptions {
            throttle: Duration::from_millis(50),
            ..Default::default()
        },
        None,
        move |report| {
            let _ = tx.send(report.clone());
        },
    )
    .unwrap();
    fs::write(f.0.join("new"), b"new file").unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        rx.recv_timeout(left).expect("FSEvents update");
        if tree.read().unwrap().lookup_relative(b"new").is_some() {
            break;
        }
    }
    tree.read().unwrap().check_invariants().unwrap();
    let start = std::time::Instant::now();
    watcher.stop();
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_new_hardlink_keeps_both_names_after_refresh() {
    let f = Fixture::new();
    fs::write(f.0.join("original"), b"original").unwrap();
    let tree = run(&f.0, 1);
    let original = tree.read().unwrap().lookup_relative(b"original").unwrap();
    fs::hard_link(f.0.join("original"), f.0.join("extra")).unwrap();
    scan::darwin::refresh(&tree, &f.0).unwrap();
    let t = tree.read().unwrap();
    assert_eq!(t.lookup_relative(b"original"), Some(original));
    assert!(t.lookup_relative(b"extra").is_some());
    assert_eq!(t.node(NodeId::ROOT).logical, 8);
    t.check_invariants().unwrap();
}

#[test]
fn deleting_a_hardlink_owners_parent_transfers_its_bytes() {
    let f = Fixture::new();
    for name in ["one", "two"] {
        fs::create_dir(f.0.join(name)).unwrap();
    }
    fs::write(f.0.join("one/file"), b"some bytes").unwrap();
    fs::hard_link(f.0.join("one/file"), f.0.join("two/file")).unwrap();
    let tree = run(&f.0, 1);
    let parent = {
        let t = tree.read().unwrap();
        if t.node(t.lookup_relative(b"one/file").unwrap()).logical > 0 {
            "one"
        } else {
            "two"
        }
    };
    fs::remove_dir_all(f.0.join(parent)).unwrap();
    scan::darwin::refresh(&tree, &f.0).unwrap();
    let t = tree.read().unwrap();
    assert_eq!(t.node(NodeId::ROOT).logical, 10);
    t.check_invariants().unwrap();
}
