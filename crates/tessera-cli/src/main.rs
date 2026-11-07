//! `tessera`: command-line front end for the Tessera core.
//!
//! Used to benchmark the scanner against the targets in the architecture
//! doc (non-admin full C: scan < 60 s, < 300 MB, 0 % idle CPU) and to
//! exercise the core without the GUI.
//!
//! ```text
//! tessera scan  <PATH> [--mode auto|walk|mft] [--threads N] [--top N] [--json]
//! tessera bench <PATH> [--mode auto|walk|mft] [--threads N] [--runs N] [--idle SECS] [--watch]
//! tessera watch <PATH> [--mode auto|walk|mft] [--secs N]
//! ```

mod sys;

use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tessera_core::layout::{LayoutEngine, LayoutRequest, RootExtras};
use tessera_core::scan::{self, ScanMode, ScanOptions, ScanProgress, ScanSummary};
use tessera_core::watch::{BatchReport, WatchOptions, Watcher};
use tessera_core::{NodeId, SharedTree, Tree};

const USAGE: &str = "\
usage:
  tessera scan  <PATH> [--mode auto|walk|mft] [--threads N] [--top N] [--json]
  tessera bench <PATH> [--mode auto|walk|mft] [--threads N] [--runs N] [--idle SECS] [--watch]
  tessera watch <PATH> [--mode auto|walk|mft] [--secs N]

scan   scans PATH once and prints totals, memory and the largest items
bench  scans PATH several times, times treemap layouts of the last tree,
       then keeps it and measures idle CPU for SECS seconds (with --watch:
       with live updates running); prints one JSON object
watch  scans PATH, then prints every batch of live updates (for N seconds,
       or until Ctrl+C)
";

struct Args {
    command: String,
    path: PathBuf,
    mode: ScanMode,
    threads: usize,
    top: usize,
    json: bool,
    runs: usize,
    idle_secs: u64,
    watch: bool,
    secs: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args_os().skip(1);
    let command = it
        .next()
        .ok_or("missing command")?
        .into_string()
        .map_err(|_| "bad command")?;
    let mut args = Args {
        command,
        path: PathBuf::new(),
        mode: ScanMode::Auto,
        threads: 0,
        top: 10,
        json: false,
        runs: 3,
        idle_secs: 10,
        watch: false,
        secs: 0,
    };
    let mut path = None;
    while let Some(a) = it.next() {
        let s = a.to_string_lossy().into_owned();
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .map(|v| v.to_string_lossy().into_owned())
                .ok_or(format!("{name} needs a value"))
        };
        match s.as_str() {
            "--mode" => {
                args.mode = match value("--mode")?.as_str() {
                    "auto" => ScanMode::Auto,
                    "walk" => ScanMode::Walk,
                    "mft" => ScanMode::Mft,
                    other => return Err(format!("unknown mode {other}")),
                }
            }
            "--threads" => args.threads = num(&value("--threads")?)?,
            "--top" => args.top = num(&value("--top")?)?,
            "--runs" => args.runs = num::<usize>(&value("--runs")?)?.max(1),
            "--idle" => args.idle_secs = num(&value("--idle")?)?,
            "--secs" => args.secs = num(&value("--secs")?)?,
            "--json" => args.json = true,
            "--watch" => args.watch = true,
            "-h" | "--help" => return Err(String::new()),
            _ if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument {s}")),
        }
    }
    args.path = path.ok_or("missing PATH")?;
    Ok(args)
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}\n");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match args.command.as_str() {
        "scan" => cmd_scan(&args),
        "bench" => cmd_bench(&args),
        "watch" => cmd_watch(&args),
        other => Err(format!("unknown command {other}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs one scan on a worker thread while the main thread prints progress
/// to stderr every 500 ms (only while the scan runs).
fn run_scan(args: &Args, show_progress: bool) -> Result<(SharedTree, ScanSummary), String> {
    let tree = Tree::for_path(&args.path).into_shared();
    let progress = Arc::new(ScanProgress::new());
    let opts = ScanOptions {
        mode: args.mode,
        threads: args.threads,
    };
    if !show_progress {
        return scan::scan(&args.path, &opts, &tree, &progress)
            .map(|summary| (tree, summary))
            .map_err(|e| e.to_string());
    }
    let (done, completion) = std::sync::mpsc::sync_channel(1);
    let worker = {
        let (tree, progress, path) = (tree.clone(), progress.clone(), args.path.clone());
        std::thread::spawn(move || {
            let result = scan::scan(&path, &opts, &tree, &progress);
            let _ = done.send(());
            result
        })
    };
    let start = Instant::now();
    while matches!(
        completion.recv_timeout(Duration::from_millis(500)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ) {
        let p = progress.snapshot();
        eprint!(
            "\r  {:>6.1}s  {:>10} files  {:>9} dirs  {:>10}",
            start.elapsed().as_secs_f64(),
            p.files,
            p.dirs,
            human(p.bytes)
        );
    }
    if show_progress {
        eprint!("\r{:72}\r", "");
    }
    let summary = worker
        .join()
        .map_err(|_| "scanner thread panicked".to_string())?
        .map_err(|e| e.to_string())?;
    Ok((tree, summary))
}

/// Starts live updates on a scanned tree, reusing the MFT scan's journal
/// position when there is one.
fn start_watcher(
    args: &Args,
    tree: &SharedTree,
    summary: &mut ScanSummary,
    on_batch: impl Fn(&BatchReport) + Send + 'static,
) -> Result<Watcher, String> {
    let usn = summary.frn_index.take().zip(summary.usn_checkpoint.take());
    Watcher::start(
        &args.path,
        tree.clone(),
        WatchOptions::default(),
        usn,
        on_batch,
    )
    .map_err(|e| format!("cannot watch: {e}"))
}

fn cmd_scan(args: &Args) -> Result<(), String> {
    let (shared, summary) = run_scan(args, !args.json)?;
    let tree = shared.read().map_err(|_| "tree lock poisoned")?;
    let mem = sys::memory();
    let tmem = tree.memory();
    let root = tree.node(NodeId::ROOT);

    let mut top_dirs: Vec<NodeId> = tree.children(NodeId::ROOT).collect();
    top_dirs.sort_by_key(|&c| std::cmp::Reverse(tree.node(c).size));
    top_dirs.truncate(args.top);
    let top_files = largest_files(&tree, args.top);

    if args.json {
        let mut j = Json::new();
        j.str("path", &args.path.to_string_lossy());
        j.str("mode", &format!("{:?}", summary.mode));
        j.num("elapsed_ms", summary.elapsed.as_millis() as u64);
        j.num("files", summary.files);
        j.num("dirs", summary.dirs);
        j.num("inaccessible", summary.inaccessible);
        j.num("nodes", tree.len() as u64);
        j.num("size_on_disk", root.size);
        j.num("logical_size", root.logical);
        j.num("tree_bytes", tmem.total() as u64);
        j.num("working_set", mem.working_set);
        j.num("peak_working_set", mem.peak_working_set);
        j.num("private_bytes", mem.private_bytes);
        println!("{}", j.finish());
        return Ok(());
    }

    println!("{}", args.path.display());
    println!(
        "  scanner        {:?}{}",
        summary.mode,
        if scan::is_elevated() {
            " (elevated)"
        } else {
            ""
        }
    );
    println!("  elapsed        {:.2} s", summary.elapsed.as_secs_f64());
    println!(
        "  entries        {} files, {} dirs, {} inaccessible",
        summary.files, summary.dirs, summary.inaccessible
    );
    println!(
        "  total          {} on disk, {} logical",
        human(root.size),
        human(root.logical)
    );
    if let Some(v) = &summary.volume {
        println!(
            "  volume         {} {} total, {} free",
            v.file_system,
            human(v.total_bytes),
            human(v.free_bytes)
        );
    }
    println!(
        "  tree memory    {} ({} nodes, {} names)",
        human(tmem.total() as u64),
        human(tmem.nodes as u64),
        human(tmem.names as u64)
    );
    println!(
        "  process memory {} working set, {} peak, {} private",
        human(mem.working_set),
        human(mem.peak_working_set),
        human(mem.private_bytes)
    );
    if !top_dirs.is_empty() {
        println!("\nlargest entries in {}", args.path.display());
        for c in top_dirs {
            let n = tree.node(c);
            println!(
                "  {:>10}  {}{}",
                human(n.size),
                tree.name_lossy(c),
                if n.is_dir() { "\\" } else { "" }
            );
        }
    }
    if !top_files.is_empty() {
        println!("\nlargest files");
        for f in top_files {
            println!("  {:>10}  {}", human(tree.node(f).size), tree.path_lossy(f));
        }
    }
    Ok(())
}

fn cmd_bench(args: &Args) -> Result<(), String> {
    let mut times = Vec::with_capacity(args.runs);
    let mut last: Option<(SharedTree, ScanSummary)> = None;
    for run in 0..args.runs {
        // Drop the previous tree first so runs don't stack up memory.
        drop(last.take());
        let (tree, summary) = run_scan(args, false)?;
        eprintln!(
            "run {}: {:.2} s, {} nodes",
            run + 1,
            summary.elapsed.as_secs_f64(),
            tree.read().map_err(|_| "tree lock poisoned")?.len()
        );
        times.push(summary.elapsed);
        last = Some((tree, summary));
    }
    let (tree, mut summary) = last.expect("at least one run");
    let after_scan = sys::memory();

    // Treemap layout of the whole volume, as the GUI would ask for it. The
    // engine stays alive through the idle phase, like in the app.
    let mut engine = LayoutEngine::new();
    let layout = time_layouts(&tree, &mut engine)?;

    // Idle: keep the tree alive (and, with --watch, live updates running)
    // and do nothing else; any CPU time is overhead.
    let batches = Arc::new(AtomicUsize::new(0));
    let watcher = if args.watch {
        let b = batches.clone();
        Some(start_watcher(args, &tree, &mut summary, move |_| {
            b.fetch_add(1, Ordering::Relaxed);
        })?)
    } else {
        None
    };
    let backend = watcher.as_ref().map(|w| format!("{:?}", w.backend()));
    let cpu_before = sys::cpu_time();
    let idle_start = Instant::now();
    std::thread::sleep(Duration::from_secs(args.idle_secs));
    let idle_wall = idle_start.elapsed();
    let idle_cpu = sys::cpu_time().saturating_sub(cpu_before);
    let idle_mem = sys::memory();
    if let Some(w) = watcher {
        w.stop();
    }

    // The first run reads a cold(er) disk cache; the rest are warm.
    let first = times[0];
    times.sort();
    let median = times[times.len() / 2];
    let t = tree.read().map_err(|_| "tree lock poisoned")?;
    let mut j = Json::new();
    j.str("path", &args.path.to_string_lossy());
    j.str("mode", &format!("{:?}", summary.mode));
    j.bool("elevated", scan::is_elevated());
    j.num("runs", args.runs as u64);
    j.num("first_ms", first.as_millis() as u64);
    j.num("min_ms", times[0].as_millis() as u64);
    j.num("median_ms", median.as_millis() as u64);
    j.num("max_ms", times[times.len() - 1].as_millis() as u64);
    j.num("files", summary.files);
    j.num("dirs", summary.dirs);
    j.num("inaccessible", summary.inaccessible);
    j.num("nodes", t.len() as u64);
    j.num("size_on_disk", t.node(NodeId::ROOT).size);
    if let Some(v) = &summary.volume {
        j.num("volume_used", v.total_bytes.saturating_sub(v.free_bytes));
    }
    j.num("tree_bytes", t.memory().total() as u64);
    j.num("working_set_after_scan", after_scan.working_set);
    j.num("private_bytes_after_scan", after_scan.private_bytes);
    j.num("peak_working_set", after_scan.peak_working_set);
    j.float("layout_first_ms", layout.first_ms);
    j.float("layout_1080p_ms", layout.fhd_ms);
    j.float("layout_4k_ms", layout.uhd_ms);
    j.num("layout_1080p_bytes", layout.fhd_bytes as u64);
    j.num("working_set_idle", idle_mem.working_set);
    j.str("watcher", backend.as_deref().unwrap_or("off"));
    j.num("watch_batches", batches.load(Ordering::Relaxed) as u64);
    j.num("idle_secs", idle_wall.as_secs());
    j.float(
        "idle_cpu_percent_of_one_core",
        100.0 * idle_cpu.as_secs_f64() / idle_wall.as_secs_f64(),
    );
    drop(engine);
    println!("{}", j.finish());
    Ok(())
}

struct LayoutTimes {
    /// First layout of the root with a fresh engine (fills its caches).
    first_ms: f64,
    /// Median of repeated root layouts at 1920x1080 and 3840x2160.
    fhd_ms: f64,
    uhd_ms: f64,
    fhd_bytes: usize,
}

fn time_layouts(tree: &SharedTree, engine: &mut LayoutEngine) -> Result<LayoutTimes, String> {
    const RUNS: usize = 15;
    let t = tree.read().map_err(|_| "tree lock poisoned")?;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mut rev = 0;
    let mut run = |engine: &mut LayoutEngine, w: f32, h: f32| {
        rev += 1;
        let req = LayoutRequest::new(NodeId::ROOT, w, h);
        let start = Instant::now();
        let buf = engine.layout(&t, &req, RootExtras::default(), rev);
        (start.elapsed(), buf.len())
    };
    let (first, fhd_bytes) = run(engine, 1920.0, 1080.0);
    let mut median = |w: f32, h: f32| {
        let mut v: Vec<Duration> = (0..RUNS).map(|_| run(engine, w, h).0).collect();
        v.sort();
        ms(v[RUNS / 2])
    };
    let fhd_ms = median(1920.0, 1080.0);
    let uhd_ms = median(3840.0, 2160.0);
    Ok(LayoutTimes {
        first_ms: ms(first),
        fhd_ms,
        uhd_ms,
        fhd_bytes,
    })
}

fn cmd_watch(args: &Args) -> Result<(), String> {
    let (tree, mut summary) = run_scan(args, true)?;
    {
        let t = tree.read().map_err(|_| "tree lock poisoned")?;
        println!(
            "scanned {} ({:?}) in {:.2} s: {} files, {} dirs, {}",
            args.path.display(),
            summary.mode,
            summary.elapsed.as_secs_f64(),
            summary.files,
            summary.dirs,
            human(t.node(NodeId::ROOT).size)
        );
    }
    let start = Instant::now();
    let shared = tree.clone();
    let watcher = start_watcher(args, &tree, &mut summary, move |r| {
        let total = shared
            .read()
            .map(|t| t.node(NodeId::ROOT).size)
            .unwrap_or(0);
        println!(
            "{:>8.1}s  {} events -> +{} -{} ~{} moved {}{}{}  total {}",
            start.elapsed().as_secs_f64(),
            r.events,
            r.created,
            r.deleted,
            r.modified,
            r.moved,
            if r.rescan_needed.is_empty() {
                String::new()
            } else {
                format!("  rescan needed: {}", r.rescan_needed.len())
            },
            r.error
                .as_ref()
                .map(|e| format!("  error: {e}"))
                .unwrap_or_default(),
            human(total)
        );
    })?;
    println!(
        "watching with {:?}; idle means 0% CPU. Ctrl+C to stop.",
        watcher.backend()
    );
    if args.secs > 0 {
        std::thread::sleep(Duration::from_secs(args.secs));
        watcher.stop();
    } else {
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

/// The `n` largest files (by size on disk) in the whole tree.
fn largest_files(tree: &Tree, n: usize) -> Vec<NodeId> {
    if n == 0 {
        return Vec::new();
    }
    let mut heap = BinaryHeap::with_capacity(n + 1);
    for i in 1..tree.len() as u32 {
        let id = NodeId(i);
        let node = tree.node(id);
        if node.is_dir() || node.size == 0 {
            continue;
        }
        if heap.len() < n {
            heap.push(std::cmp::Reverse((node.size, id)));
        } else if heap.peek().is_some_and(|m| m.0.0 < node.size) {
            heap.pop();
            heap.push(std::cmp::Reverse((node.size, id)));
        }
    }
    let mut v: Vec<(u64, NodeId)> = heap.into_iter().map(|r| r.0).collect();
    v.retain(|&(_, id)| !tree.is_deleted(id));
    v.sort_by(|a, b| b.cmp(a));
    v.into_iter().map(|(_, id)| id).collect()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// A tiny JSON object writer (keeps the CLI dependency-free).
struct Json(String);

impl Json {
    fn new() -> Self {
        Json(String::from("{"))
    }
    fn key(&mut self, k: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push('"');
        self.0.push_str(k);
        self.0.push_str("\":");
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        self.0.push('"');
        for c in v.chars() {
            match c {
                '"' => self.0.push_str("\\\""),
                '\\' => self.0.push_str("\\\\"),
                c if (c as u32) < 0x20 => self.0.push_str(&format!("\\u{:04x}", c as u32)),
                c => self.0.push(c),
            }
        }
        self.0.push('"');
    }
    fn num(&mut self, k: &str, v: u64) {
        self.key(k);
        self.0.push_str(&v.to_string());
    }
    fn float(&mut self, k: &str, v: f64) {
        self.key(k);
        self.0.push_str(&format!("{v:.3}"));
    }
    fn bool(&mut self, k: &str, v: bool) {
        self.key(k);
        self.0.push_str(if v { "true" } else { "false" });
    }
    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn json_escapes() {
        let mut j = Json::new();
        j.str("p", "C:\\a \"b\"\n");
        j.num("n", 3);
        j.bool("b", true);
        assert_eq!(j.finish(), r#"{"p":"C:\\a \"b\"\u000a","n":3,"b":true}"#);
    }

    #[test]
    fn largest_files_picks_biggest() {
        let mut t = Tree::new(b"X:\\");
        let mut b = tessera_core::EntryBatch::new();
        for (i, s) in [5u64, 50, 7, 900, 3].iter().enumerate() {
            b.push_wtf8(format!("f{i}").as_bytes(), *s, *s, Default::default(), 0);
        }
        t.add_children(NodeId::ROOT, &b);
        let top: Vec<String> = largest_files(&t, 2)
            .into_iter()
            .map(|id| t.name_lossy(id))
            .collect();
        assert_eq!(top, ["f3", "f1"]);
    }
}
