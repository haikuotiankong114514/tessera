//! Live, batched incremental updates of a scanned tree.
//!
//! "Collect, coalesce, throttle, animate", and nothing wakes up when
//! nothing changes:
//!
//! 1. **Collect.** A collector thread blocks on the file system with no
//!    timeout: `ReadDirectoryChangesExW` (or `ReadDirectoryChangesW`),
//!    recursive on the scan root, without admin rights; or, after an MFT
//!    scan with admin rights, `FSCTL_READ_USN_JOURNAL` from the scan's
//!    checkpoint (`BytesToWaitFor = 1`). Both use overlapped I/O and also
//!    wait on a stop event, so [`Watcher::stop`] wakes them at once.
//! 2. **Coalesce.** Changes are queued; any number of changes to one entry
//!    collapse into its final state. Only changed entries are re-read, and
//!    only their attributes (the extended notifications carry them, so
//!    usually nothing is re-read); file contents are never opened and
//!    cloud placeholders are never hydrated.
//! 3. **Throttle.** The first change after a quiet period opens a window of
//!    [`WatchOptions::throttle`]; at its end the whole batch is applied
//!    under one `tree.write()`. So there is at most one batch per window,
//!    and an idle watcher costs no CPU at all.
//! 4. **Animate.** After each batch that changed something, the callback
//!    receives a [`BatchReport`] so the UI can relayout and animate once.
//!
//! When changes were lost (notification buffer overflow, USN journal gap)
//! the report says which directory needs a rescan (the root if unknown);
//! the tree never drifts silently. [`Watcher::rescan`] performs a rescan on
//! the watcher's own thread, serialized with the batches.
//!
//! Changes outside the tree, and below directories flagged
//! [`INACCESSIBLE`](crate::NodeFlags::INACCESSIBLE) or
//! [`REPARSE`](crate::NodeFlags::REPARSE), are ignored.
//!
//! Start the watcher right after the scan. With the USN journal nothing is
//! missed in between (it reads from the scan's checkpoint); with directory
//! notifications, changes made during a walk scan may be missed.

use std::path::Path;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::scan::{FrnIndex, ScanProgress, UsnCheckpoint};
use crate::{NodeId, Result, SharedTree};

mod apply;
mod coalesce;
#[cfg(target_os = "macos")]
pub mod darwin;
mod rdcw;
#[cfg(windows)]
mod sys;
mod usn;

use coalesce::{Batch, Coalescer, Event};

#[cfg(windows)]
pub use usn::query_usn_checkpoint;

/// Where changes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WatchBackend {
    /// The USN journal if an index and checkpoint are given and the journal
    /// can be read (admin rights), else directory change notifications.
    #[default]
    Auto,
    /// `ReadDirectoryChangesExW` / `ReadDirectoryChangesW` on the root.
    DirectoryChanges,
    /// macOS FSEvents, with dirty directory reconciliation.
    #[cfg(target_os = "macos")]
    FsEvents,
    /// The NTFS USN change journal (needs admin rights, an index of the
    /// tree's file reference numbers and a checkpoint).
    UsnJournal,
}

/// Watcher settings.
#[derive(Clone, Debug)]
pub struct WatchOptions {
    pub backend: WatchBackend,
    /// Length of the window that gathers changes into one batch, i.e. the
    /// shortest time between two batches. Default 500 ms. Change it while
    /// running with [`Watcher::set_throttle`].
    pub throttle: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            backend: WatchBackend::Auto,
            throttle: Duration::from_millis(500),
        }
    }
}

/// What one applied batch changed. Passed to the watcher's callback, which
/// is only called when there is something to report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchReport {
    /// Raw notifications (or journal records) gathered, before coalescing.
    pub events: usize,
    /// Changes made to the tree: `created + deleted + modified + moved`.
    pub applied: usize,
    /// Entries added (a new directory counts once, whatever it contains).
    pub created: usize,
    /// Entries marked deleted (a directory counts once).
    pub deleted: usize,
    /// Entries whose size or attributes changed.
    pub modified: usize,
    /// Entries renamed or moved (the node keeps its id).
    pub moved: usize,
    /// Live directories whose children changed (directly), sorted. Their
    /// ancestors' totals changed too.
    pub dirs: Vec<NodeId>,
    /// Directories whose contents may be out of date because changes were
    /// lost (buffer overflow, journal gap) or a new directory could not be
    /// scanned. [`NodeId::ROOT`] when the place is unknown. Rescan them with
    /// [`Watcher::rescan`].
    pub rescan_needed: Vec<NodeId>,
    /// Set when the watcher stopped working (e.g. the watched directory was
    /// deleted or the journal disabled); no more batches will come.
    pub error: Option<String>,
}

impl BatchReport {
    /// True if the report carries no news.
    pub fn is_empty(&self) -> bool {
        self.applied == 0
            && self.dirs.is_empty()
            && self.rescan_needed.is_empty()
            && self.error.is_none()
    }
}

/// The batch window, shared by the watcher and its applier thread so it
/// can change while the watcher runs.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Throttle(AtomicU64);

#[cfg_attr(not(windows), allow(dead_code))]
impl Throttle {
    pub(crate) fn new(d: Duration) -> Throttle {
        Throttle(AtomicU64::new(Self::nanos(d)))
    }

    fn nanos(d: Duration) -> u64 {
        u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
    }

    pub(crate) fn get(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::Relaxed))
    }

    pub(crate) fn set(&self, d: Duration) {
        self.0.store(Self::nanos(d), Ordering::Relaxed);
    }
}

/// The applier loop: wait (indefinitely) for the first event, gather for
/// one throttle window, apply, report; repeat until the channel closes or
/// `stop` is set. The window is measured from its first event with the
/// throttle as it is now, so shortening the throttle and sending an empty
/// event list ends a long window early.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn run_applier(
    rx: Receiver<Vec<Event>>,
    throttle: &Throttle,
    stop: &AtomicBool,
    mut apply: impl FnMut(Batch) -> Result<BatchReport>,
    on_batch: &dyn Fn(&BatchReport),
) {
    let mut co = Coalescer::default();
    loop {
        match rx.recv() {
            // an empty list only wakes the loop (see Watcher::set_throttle)
            Ok(events) if events.is_empty() => continue,
            Ok(events) => co.extend(events),
            Err(_) => return,
        }
        let opened = Instant::now();
        let mut closed = false;
        loop {
            let left = (opened + throttle.get()).saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(events) => co.extend(events),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        match apply(co.take()) {
            Ok(report) => {
                if !report.is_empty() && !stop.load(Ordering::Acquire) {
                    on_batch(&report);
                }
            }
            Err(_) => return,
        }
        if closed {
            return;
        }
    }
}

/// A running watcher. Dropping it stops it, like [`Watcher::stop`].
pub struct Watcher {
    #[cfg(target_os = "macos")]
    darwin_stream: Option<darwin::Stream>,
    #[cfg(target_os = "macos")]
    darwin_requests: Option<Sender<Option<std::path::PathBuf>>>,
    backend: WatchBackend,
    stop: Arc<AtomicBool>,
    progress: Arc<ScanProgress>,
    throttle: Arc<Throttle>,
    requests: Option<Sender<Vec<Event>>>,
    #[cfg(windows)]
    wake: Arc<sys::OwnedHandle>,
    collector: Option<JoinHandle<()>>,
    applier: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching `root`, the directory `tree` was scanned from (its
    /// root node), and keeps `tree` up to date.
    ///
    /// `usn` is the [`FrnIndex`] and [`UsnCheckpoint`] of an MFT scan of
    /// the same tree (see [`ScanSummary`](crate::scan::ScanSummary)); it
    /// enables the USN journal backend. `on_batch` runs on the watcher's
    /// thread after each batch that changed something; it should return
    /// quickly and must not call [`Watcher::stop`] or wait for a thread
    /// that does.
    ///
    /// Fails if `root` cannot be watched (e.g. it does not exist), or with
    /// [`Error::Unsupported`](crate::Error::Unsupported) if the USN backend
    /// is requested without `usn`.
    #[cfg(windows)]
    pub fn start(
        root: &Path,
        tree: SharedTree,
        opts: WatchOptions,
        usn: Option<(FrnIndex, UsnCheckpoint)>,
        on_batch: impl Fn(&BatchReport) + Send + 'static,
    ) -> Result<Watcher> {
        use std::sync::mpsc;

        use crate::scan::{ScanMode, ScanOptions};

        if opts.backend == WatchBackend::UsnJournal && usn.is_none() {
            return Err(crate::Error::Unsupported(
                "the USN journal backend needs the file index and checkpoint of an MFT scan",
            ));
        }
        let want_usn = opts.backend != WatchBackend::DirectoryChanges && usn.is_some();
        let strict_usn = opts.backend == WatchBackend::UsnJournal;
        let checkpoint = usn.as_ref().map(|(_, cp)| *cp);

        let wake = Arc::new(sys::OwnedHandle::event()?);
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(ScanProgress::new());
        let (tx, rx) = mpsc::channel::<Vec<Event>>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<WatchBackend>>(1);

        let io_root = root.to_path_buf();
        let io_wake = wake.clone();
        let io_tx = tx.clone();
        let collector = std::thread::Builder::new()
            .name("tessera-watch-io".into())
            .spawn(move || {
                let ready = |backend: WatchBackend| -> Box<dyn FnOnce(Result<()>) + Send> {
                    let tx = ready_tx.clone();
                    Box::new(move |r: Result<()>| {
                        let _ = tx.send(r.map(|()| backend));
                    })
                };
                if let Some(cp) = checkpoint.filter(|_| want_usn) {
                    match usn::UsnReader::open(&io_root, cp) {
                        Ok((mut reader, first)) => {
                            if !first.is_empty() {
                                let _ = io_tx.send(first);
                            }
                            let started = ready(WatchBackend::UsnJournal);
                            sys::run_reader(&mut reader, &io_wake, &io_tx, Some(started));
                            return;
                        }
                        Err(e) if strict_usn => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                        Err(_) => {} // not elevated, no journal...: fall back
                    }
                }
                match rdcw::DirReader::open(&io_root) {
                    Ok(mut reader) => {
                        let started = ready(WatchBackend::DirectoryChanges);
                        sys::run_reader(&mut reader, &io_wake, &io_tx, Some(started));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;

        let throttle = Arc::new(Throttle::new(opts.throttle));
        let mut w = Watcher {
            backend: WatchBackend::Auto,
            stop: stop.clone(),
            progress: progress.clone(),
            throttle: throttle.clone(),
            requests: Some(tx),
            wake,
            collector: Some(collector),
            applier: None,
        };
        w.backend = match ready_rx.recv() {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(crate::Error::Unsupported(
                    "the watcher thread ended before it started",
                ));
            }
        };
        let mut usn_state = match w.backend {
            WatchBackend::UsnJournal => usn.map(|(index, _)| apply::UsnState::new(index)),
            _ => None,
        };
        let mut probe = sys::WinProbe::new(root, usn_state.is_some())?;
        let applier = std::thread::Builder::new()
            .name("tessera-watch".into())
            .spawn(move || {
                let base = probe.win32_root().to_vec();
                let scan_opts = ScanOptions {
                    mode: ScanMode::Walk,
                    ..ScanOptions::default()
                };
                let mut scan = |t: &SharedTree, dir: NodeId, rel: &[u8]| -> Result<()> {
                    let path = sys::join_path(&base, rel);
                    crate::scan::walk::scan_into(t, dir, &path, &scan_opts, &progress).map(|_| ())
                };
                run_applier(
                    rx,
                    &throttle,
                    &stop,
                    |batch| {
                        apply::apply_batch(&tree, batch, &mut probe, usn_state.as_mut(), &mut scan)
                    },
                    &on_batch,
                );
            })?;
        w.applier = Some(applier);
        Ok(w)
    }

    /// Live updates are only implemented on Windows.
    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn start(
        _root: &Path,
        _tree: SharedTree,
        _opts: WatchOptions,
        _usn: Option<(FrnIndex, UsnCheckpoint)>,
        _on_batch: impl Fn(&BatchReport) + Send + 'static,
    ) -> Result<Watcher> {
        Err(crate::Error::Unsupported(
            "live updates are only implemented on Windows",
        ))
    }

    #[cfg(target_os = "macos")]
    pub fn start(
        root: &Path,
        tree: SharedTree,
        opts: WatchOptions,
        _usn: Option<(FrnIndex, UsnCheckpoint)>,
        on_batch: impl Fn(&BatchReport) + Send + 'static,
    ) -> Result<Watcher> {
        if opts.backend == WatchBackend::UsnJournal {
            return Err(crate::Error::Unsupported("USN is Windows-only"));
        }
        let logical_root = tree
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .path(NodeId::ROOT);
        let root = std::fs::canonicalize(root)?;
        let (tx, rx) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        let sender = tx.clone();
        let base = root.clone();
        let mapped_root = logical_root.clone();
        let stream = darwin::Stream::new(&root, move |path, flags| {
            let path = if flags & (1 | 2 | 4 | 8 | 32 | 128) != 0 {
                None
            } else {
                let mut path = path.to_path_buf();
                if let Ok(rel) = path.strip_prefix(&base) {
                    path = mapped_root.join(rel);
                }
                Some(path.parent().unwrap_or(&path).to_path_buf())
            };
            let _ = sender.send(path);
        })?;
        let root = logical_root;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let throttle = Arc::new(Throttle::new(opts.throttle));
        let pace = throttle.clone();
        let progress = Arc::new(ScanProgress::new());
        let collector = std::thread::Builder::new()
            .name("tessera-fsevents-apply".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let until = Instant::now() + pace.get();
                    let mut dirty = std::collections::HashSet::new();
                    let mut full = first.is_none();
                    if let Some(path) = first {
                        dirty.insert(path);
                    }
                    while let Some(left) = until.checked_duration_since(Instant::now()) {
                        match rx.recv_timeout(left) {
                            Ok(Some(path)) => {
                                dirty.insert(path);
                            }
                            Ok(None) => full = true,
                            Err(_) => break,
                        }
                    }
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut report = BatchReport {
                        events: dirty.len(),
                        ..Default::default()
                    };
                    if full {
                        let replacement = crate::Tree::for_path(&root).into_shared();
                        match crate::scan::scan(
                            &root,
                            &crate::scan::ScanOptions::default(),
                            &replacement,
                            &ScanProgress::new(),
                        ) {
                            Ok(_) => {
                                let replacement = Arc::try_unwrap(replacement)
                                    .ok()
                                    .unwrap()
                                    .into_inner()
                                    .unwrap_or_else(PoisonError::into_inner);
                                *tree.write().unwrap_or_else(PoisonError::into_inner) = replacement;
                                report.applied = 1;
                            }
                            Err(e) => report.error = Some(e.to_string()),
                        }
                    } else {
                        for path in dirty {
                            if !path.starts_with(&root) {
                                continue;
                            }
                            match crate::scan::darwin::refresh(&tree, &path) {
                                Ok(true) => report.applied += 1,
                                Ok(false) => {}
                                Err(e) => report.error = Some(e.to_string()),
                            }
                        }
                    }
                    if !report.is_empty() {
                        on_batch(&report);
                    }
                }
            })?;
        Ok(Watcher {
            backend: WatchBackend::FsEvents,
            stop,
            progress,
            throttle,
            requests: None,
            collector: Some(collector),
            applier: None,
            darwin_stream: Some(stream),
            darwin_requests: Some(tx),
        })
    }

    /// The backend actually in use (never `Auto`).
    pub fn backend(&self) -> WatchBackend {
        self.backend
    }

    /// Changes the batch window (see [`WatchOptions::throttle`]), e.g. a
    /// longer one while the UI is in the background, so busy disks wake the
    /// watcher less often and repeated changes to one file are read once.
    /// A window already open closes as soon as it has lasted the new length.
    pub fn set_throttle(&self, throttle: Duration) {
        if self.throttle.get() == throttle {
            return;
        }
        self.throttle.set(throttle);
        if let Some(tx) = &self.requests {
            let _ = tx.send(Vec::new());
        }
    }

    /// The current batch window.
    pub fn throttle(&self) -> Duration {
        self.throttle.get()
    }

    /// Rescans directory `dir` on the watcher's thread: its children are
    /// marked deleted and it is scanned again. The result arrives in a batch
    /// report like any change. Use it for [`BatchReport::rescan_needed`].
    pub fn rescan(&self, dir: NodeId) {
        #[cfg(target_os = "macos")]
        if let Some(tx) = &self.darwin_requests {
            let _ = dir;
            let _ = tx.send(None);
        }
        if let Some(tx) = &self.requests {
            let _ = tx.send(vec![Event::Rescan(dir)]);
        }
    }

    /// Stops watching: wakes the blocked threads and waits for them. Changes
    /// gathered but not applied yet are dropped.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.progress.cancel();
        #[cfg(target_os = "macos")]
        {
            self.darwin_stream.take();
            self.darwin_requests.take();
        }
        #[cfg(windows)]
        self.wake.set();
        if let Some(h) = self.collector.take() {
            let _ = h.join();
        }
        // The applier's channel closes once the collector's sender and ours
        // are gone, which ends its wait.
        self.requests = None;
        if let Some(h) = self.applier.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::apply::tests::{FakeFs, dir, file, listing, tree_of};
    use super::coalesce::Info;
    use super::*;

    /// Runs the applier loop on a thread over a fake file system.
    struct Harness {
        tx: Option<Sender<Vec<Event>>>,
        stop: Arc<AtomicBool>,
        throttle: Arc<Throttle>,
        reports: Arc<Mutex<Vec<(Instant, BatchReport)>>>,
        thread: Option<JoinHandle<()>>,
        shared: SharedTree,
        fs: Arc<Mutex<FakeFs>>,
    }

    impl Harness {
        fn new(throttle: Duration) -> Harness {
            let mut fs = FakeFs::default();
            fs.put("d", dir());
            fs.put("d\\f.log", file(10));
            let shared = tree_of(&fs);
            let fs = Arc::new(Mutex::new(fs));
            let (tx, rx) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let throttle = Arc::new(Throttle::new(throttle));
            let reports = Arc::new(Mutex::new(Vec::new()));
            let (t_shared, t_fs, t_stop, t_reports, t_throttle) = (
                shared.clone(),
                fs.clone(),
                stop.clone(),
                reports.clone(),
                throttle.clone(),
            );
            let thread = std::thread::spawn(move || {
                let on_batch = move |r: &BatchReport| {
                    t_reports.lock().unwrap().push((Instant::now(), r.clone()));
                };
                run_applier(
                    rx,
                    &t_throttle,
                    &t_stop,
                    |batch| {
                        let mut fs = t_fs.lock().unwrap().clone();
                        let snapshot = fs.clone();
                        let mut scan =
                            |t: &SharedTree, d: NodeId, rel: &[u8]| snapshot.scan(t, d, rel);
                        apply::apply_batch(&t_shared, batch, &mut fs, None, &mut scan)
                    },
                    &on_batch,
                );
            });
            Harness {
                tx: Some(tx),
                stop,
                throttle,
                reports,
                thread: Some(thread),
                shared,
                fs,
            }
        }

        fn send(&self, events: Vec<Event>) {
            self.tx.as_ref().unwrap().send(events).unwrap();
        }

        fn reports(&self) -> Vec<(Instant, BatchReport)> {
            self.reports.lock().unwrap().clone()
        }

        fn wait_for(&self, n: usize, timeout: Duration) -> bool {
            let end = Instant::now() + timeout;
            while Instant::now() < end {
                if self.reports().len() >= n {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        }

        fn finish(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.tx = None;
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn grow(size: u64) -> Event {
        Event::Changed {
            path: b"d\\f.log".to_vec(),
            info: Info::Present(file(size)),
        }
    }

    #[test]
    fn a_burst_becomes_one_batch_per_window() {
        let throttle = Duration::from_millis(100);
        let mut h = Harness::new(throttle);
        let start = Instant::now();
        // 1000 writes spread over ~250 ms.
        for i in 1..=1000u64 {
            h.send(vec![grow(i * 64)]);
            if i % 100 == 0 {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let burst = start.elapsed();
        assert!(h.wait_for(1, Duration::from_secs(5)));
        std::thread::sleep(throttle * 3);
        let reports = h.reports();
        let max = (burst.as_millis() / throttle.as_millis()) as usize + 2;
        assert!(
            reports.len() <= max,
            "{} batches for a {burst:?} burst",
            reports.len()
        );
        // At most one batch per window.
        for w in reports.windows(2) {
            assert!(w[1].0 - w[0].0 >= throttle - Duration::from_millis(5));
        }
        let events: usize = reports.iter().map(|(_, r)| r.events).sum();
        assert_eq!(events, 1000);
        let t = h.shared.read().unwrap();
        let f = t.lookup_relative(b"d\\f.log").unwrap();
        assert_eq!(t.node(f).logical, 64_000);
        drop(t);
        h.finish();
    }

    #[test]
    fn a_shorter_throttle_ends_a_long_window() {
        let mut h = Harness::new(Duration::from_secs(30));
        let start = Instant::now();
        h.send(vec![grow(99)]);
        std::thread::sleep(Duration::from_millis(50));
        assert!(h.reports().is_empty(), "the 30 s window is still open");
        // What Watcher::set_throttle does: store, then wake the loop.
        h.throttle.set(Duration::from_millis(20));
        h.send(Vec::new());
        assert!(h.wait_for(1, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(h.reports()[0].1.modified, 1);
        // A wake-up alone opens no window and applies nothing.
        h.send(Vec::new());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(h.reports().len(), 1);
        // Later windows use the new length.
        h.send(vec![grow(123)]);
        assert!(h.wait_for(2, Duration::from_secs(5)));
        h.finish();
    }

    #[test]
    fn idle_watcher_reports_nothing_and_stops_promptly() {
        let mut h = Harness::new(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        assert!(h.reports().is_empty());
        // A no-op change (same size) is applied but not reported.
        h.send(vec![grow(10)]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(h.reports().is_empty());
        // Stop in the middle of a window: nothing more is applied.
        let mut h2 = Harness::new(Duration::from_secs(10));
        h2.send(vec![grow(99)]);
        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        h2.finish();
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(h2.reports().is_empty());
        let t = Instant::now();
        h.finish();
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn failure_is_reported_once_the_collector_ends() {
        let mut h = Harness::new(Duration::from_millis(10));
        h.fs.lock().unwrap().put("d\\new", file(5));
        h.send(vec![
            Event::Changed {
                path: b"d\\new".to_vec(),
                info: Info::Unknown,
            },
            Event::Failed("gone".into()),
        ]);
        // The collector's end closes the channel; the batch still applies.
        h.tx = None;
        h.thread.take().unwrap().join().unwrap();
        let reports = h.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].1.created, 1);
        assert_eq!(reports[0].1.error.as_deref(), Some("gone"));
        assert!(listing(&h.shared).contains(&"d\\new 4096/5".to_string()));
        h.stop.store(true, Ordering::Release);
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn start_is_unsupported_off_windows() {
        let tree = crate::Tree::new(b"/").into_shared();
        let r = Watcher::start(Path::new("/"), tree, WatchOptions::default(), None, |_| {});
        assert!(matches!(r, Err(crate::Error::Unsupported(_))));
    }
}
