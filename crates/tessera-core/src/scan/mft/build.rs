//! Collects parsed records and builds the [`Tree`] from them (portable).
//!
//! Memory during the scan, per MFT record slot: 8 B on-disk size + 8 B
//! logical size + 4 B sequence/flags, plus 16 B per name and the name bytes
//! themselves, which go straight into the final tree's [`NamePool`] so they
//! are never copied again. The tree is then built breadth first: every
//! directory's children are pushed together, so siblings are contiguous
//! ids, parents always precede their children, and the directory sums are
//! one reverse pass over the arena.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use rayon::slice::ParallelSliceMut;

use super::parser::*;
use crate::scan::{FrnIndex, ScanProgress};
use crate::tree::{ChunkedVec, NamePool, Node, NodeFlags, NodeId, Tree};

/// One name of a record, waiting to become a node.
#[derive(Clone, Copy, Debug)]
struct Entry {
    parent: u32,
    rec: u32,
    name_off: u32,
    name_len: u16,
    parent_seq: u16,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 16);

#[derive(Clone, Copy, Default, Debug)]
struct Extra {
    wof_size: u64,
    named_size: u64,
    named_logical: u64,
}

/// Per-record facts merged from all parsed batches, in record order.
pub(crate) struct Accum {
    root_name: Vec<u8>,
    names: NamePool,
    /// On-disk bytes of the unnamed stream, per record.
    size: Vec<u64>,
    /// Logical bytes of the unnamed stream, per record.
    logical: Vec<u64>,
    /// Sequence number (low 16 bits) and parser flags (high 16 bits).
    meta: Vec<u32>,
    /// Last write time per record ([`crate::Node::mtime`] units).
    mtime: Vec<u32>,
    entries: Vec<Entry>,
    extras: HashMap<u32, Extra>,
    corrupt: u64,
}

/// The result of an MFT scan.
pub(crate) struct Built {
    pub tree: Tree,
    pub frn: FrnIndex,
    pub files: u64,
    pub dirs: u64,
    /// In-use records with a name that could not be placed in the tree
    /// (unresolvable parent, stale parent reference, cycle) plus corrupt
    /// records.
    pub skipped: u64,
}

impl Accum {
    /// `records`: number of MFT record slots; `expected_names`: a capacity
    /// hint (records in use).
    pub fn new(root_name: &[u8], records: usize, expected_names: usize) -> Accum {
        let mut names = NamePool::with_dedup();
        names.push(root_name);
        Accum {
            root_name: root_name.to_vec(),
            names,
            size: vec![0; records],
            logical: vec![0; records],
            meta: vec![0; records],
            mtime: vec![0; records],
            // Headroom for hard links (WinSxS has many); capacity that is
            // never touched is never resident.
            entries: Vec::with_capacity(expected_names + expected_names / 4 + 16),
            extras: HashMap::new(),
            corrupt: 0,
        }
    }

    /// Merges one parsed batch. Batches may arrive in any order; merging is
    /// commutative except for the order of names, which is kept stable by
    /// merging in record order.
    pub fn merge(&mut self, b: &ParsedBatch, progress: &ScanProgress) {
        let n = self.meta.len();
        let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
        for r in &b.recs {
            let i = r.rec as usize;
            if i >= n {
                self.corrupt += 1;
                continue;
            }
            let mut m = self.meta[i];
            if r.flags & F_IN_USE != 0 {
                m = (m & 0xFFFF_0000) | u32::from(r.seq);
                if r.flags & F_DIR != 0 {
                    dirs += 1;
                } else {
                    files += 1;
                }
            }
            self.meta[i] = m | (u32::from(r.flags) << 16);
            self.size[i] = self.size[i].wrapping_add(r.size);
            self.logical[i] = self.logical[i].wrapping_add(r.logical);
            if r.mtime != 0 {
                self.mtime[i] = r.mtime;
            }
            bytes = bytes.wrapping_add(r.size);
        }
        for nf in &b.names {
            if nf.rec as usize >= n {
                continue;
            }
            let name_off = self.names.push(b.name(nf));
            self.entries.push(Entry {
                parent: nf.parent,
                rec: nf.rec,
                name_off,
                name_len: nf.len,
                parent_seq: nf.parent_seq,
            });
        }
        for x in &b.extras {
            if x.rec as usize >= n {
                continue;
            }
            let e = self.extras.entry(x.rec).or_default();
            e.wof_size = e.wof_size.wrapping_add(x.wof_size);
            e.named_size = e.named_size.wrapping_add(x.named_size);
            e.named_logical = e.named_logical.wrapping_add(x.named_logical);
        }
        self.corrupt += u64::from(b.corrupt);
        progress.files.fetch_add(files, Ordering::Relaxed);
        progress.dirs.fetch_add(dirs, Ordering::Relaxed);
        progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Builds the tree. Consumes the accumulator; its arrays are freed as
    /// soon as they are no longer needed.
    pub fn finish(self) -> Built {
        Tree::build_from_mft(self)
    }
}

#[inline]
fn flags_of(meta: u32) -> u16 {
    (meta >> 16) as u16
}

#[inline]
fn seq_of(meta: u32) -> u16 {
    meta as u16
}

impl Tree {
    /// Breadth-first bulk build from merged MFT facts.
    ///
    /// Record 5 (the root directory) becomes node 0 and keeps the scanned
    /// root's name. Every directory's children are appended in one go, so
    /// sibling lists are runs of consecutive ids. While a node waits in
    /// the queue (the arena itself is the queue), its `mtime` field holds
    /// its record number; the real time replaces it when it is dequeued. A record reached through a second name becomes a
    /// `HARDLINK` node with zero sizes (or is skipped if it is a directory,
    /// which can only happen on a corrupt volume), so every record is
    /// counted once and cycles cannot be entered. Records never reached
    /// from the root are not in the tree.
    fn build_from_mft(acc: Accum) -> Built {
        let Accum {
            root_name,
            names,
            size,
            logical,
            meta,
            mtime,
            mut entries,
            extras,
            corrupt,
        } = acc;
        let records = meta.len();

        // Children of a directory become one contiguous run of entries.
        // The name offset breaks ties so the result is deterministic.
        entries.par_sort_unstable_by_key(|e| (e.parent, e.rec, e.name_off));

        let mut tree = Tree {
            posix_paths: false,
            #[cfg(target_os = "macos")]
            identities: Vec::new(),
            #[cfg(target_os = "macos")]
            hardlinks: Default::default(),
            nodes: ChunkedVec::new(),
            names,
            changed: 0,
            journal: Default::default(),
        };
        tree.nodes.push(Node {
            size: 0,
            logical: 0,
            parent: NodeId::NONE,
            first_child: NodeId::NONE,
            next_sibling: NodeId::NONE,
            name_off: 0,
            name_len: root_name.len() as u16,
            flags: NodeFlags::DIR,
            mtime: RECORD_ROOT,
        });
        let mut frn = vec![u32::MAX; records];
        if (RECORD_ROOT as usize) < records {
            frn[RECORD_ROOT as usize] = 0;
        }

        let (mut files, mut dirs) = (0u64, 0u64);
        let mut i = 0usize;
        while i < tree.nodes.len() {
            let node = *tree.nodes.get(i);
            let rec = node.mtime;
            tree.nodes.get_mut(i).mtime = mtime.get(rec as usize).copied().unwrap_or(0);
            let expandable = node.is_dir()
                && !node.flags.contains(NodeFlags::HARDLINK)
                && (rec as usize) < records;
            if !expandable {
                i += 1;
                continue;
            }
            let parent_seq = seq_of(meta[rec as usize]);
            let parent_is_meta = node.flags.contains(NodeFlags::METAFILE);
            let lo = entries.partition_point(|e| e.parent < rec);
            let hi = lo + entries[lo..].partition_point(|e| e.parent == rec);
            let first = tree.nodes.len();
            for e in &entries[lo..hi] {
                let c = e.rec as usize;
                if e.rec == rec || (e.parent_seq != 0 && e.parent_seq != parent_seq) {
                    continue;
                }
                let m = meta[c];
                let pf = flags_of(m);
                if pf & F_IN_USE == 0 {
                    continue;
                }
                let is_dir = pf & F_DIR != 0;
                let mut flags = NodeFlags(pf & NODE_FLAG_MASK);
                if pf & F_SI_REPARSE != 0 {
                    if pf & F_TAG_SURROGATE != 0 {
                        flags.insert(NodeFlags::REPARSE);
                    }
                    if pf & F_TAG_CLOUD != 0 {
                        flags.insert(NodeFlags::CLOUD);
                    }
                }
                if parent_is_meta || (e.rec < FIRST_NORMAL_RECORD && e.rec != RECORD_ROOT) {
                    flags.insert(NodeFlags::METAFILE);
                }
                let id = tree.nodes.len();
                let (mut s, mut l, stash) = (0u64, 0u64, e.rec);
                if frn[c] == u32::MAX {
                    frn[c] = id as u32;
                    if !is_dir {
                        s = size[c];
                        l = logical[c];
                        if pf & F_HAS_EXTRA != 0 {
                            if let Some(x) = extras.get(&e.rec) {
                                s = s.wrapping_add(x.wof_size);
                                if flags.contains(NodeFlags::METAFILE) {
                                    s = s.wrapping_add(x.named_size);
                                    l = l.wrapping_add(x.named_logical);
                                }
                            }
                        }
                    }
                } else if is_dir {
                    continue;
                } else {
                    flags.insert(NodeFlags::HARDLINK);
                }
                if is_dir {
                    dirs += 1;
                } else {
                    files += 1;
                }
                tree.nodes.push(Node {
                    size: s,
                    logical: l,
                    parent: NodeId(i as u32),
                    first_child: NodeId::NONE,
                    next_sibling: NodeId(id as u32 + 1),
                    name_off: e.name_off,
                    name_len: e.name_len,
                    flags,
                    mtime: stash,
                });
            }
            let last = tree.nodes.len();
            let p = tree.nodes.get_mut(i);
            if last > first {
                p.first_child = NodeId(first as u32);
                tree.nodes.get_mut(last - 1).next_sibling = NodeId::NONE;
            }
            i += 1;
        }
        drop(entries);
        drop(size);
        drop(logical);
        drop(mtime);
        drop(extras);

        // Parents precede children, so one reverse pass sums every directory.
        for id in (1..tree.nodes.len()).rev() {
            let n = tree.nodes.get(id);
            let (s, l, p) = (n.size, n.logical, n.parent.index());
            let parent = tree.nodes.get_mut(p);
            parent.size = parent.size.wrapping_add(s);
            parent.logical = parent.logical.wrapping_add(l);
        }

        let unplaced = meta
            .iter()
            .zip(&frn)
            .filter(|&(&m, &f)| {
                let pf = flags_of(m);
                f == u32::MAX && pf & F_IN_USE != 0 && pf & F_HAS_NAME != 0
            })
            .count() as u64;

        Built {
            tree,
            frn: FrnIndex {
                record_to_node: frn,
            },
            files,
            dirs,
            skipped: unplaced + corrupt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;

    /// Parses `records` and builds a tree rooted at `R:\`.
    fn build(records: Vec<(u32, Vec<u8>)>, slots: usize) -> Built {
        let mut acc = Accum::new(b"R:\\", slots, records.len());
        let progress = ScanProgress::new();
        let mut sorted = records;
        sorted.sort_by_key(|r| r.0);
        for (no, mut raw) in sorted {
            let mut b = ParsedBatch::default();
            parse_record(no, &mut raw, &mut b);
            acc.merge(&b, &progress);
        }
        acc.finish()
    }

    fn root() -> (u32, Vec<u8>) {
        (
            5,
            RecordBuilder::new(1024, 5, 5)
                .dir()
                .attr(si(0x6))
                .attr(file_name(5, 5, ".", 3))
                .build(),
        )
    }

    fn dir(no: u32, parent: u64, pseq: u16, name: &str) -> (u32, Vec<u8>) {
        (
            no,
            RecordBuilder::new(1024, no, 1)
                .dir()
                .attr(si(0))
                .attr(file_name(parent, pseq, name, 1))
                .build(),
        )
    }

    fn file(no: u32, parent: u64, pseq: u16, name: &str, alloc: u64, len: u64) -> (u32, Vec<u8>) {
        (
            no,
            RecordBuilder::new(1024, no, 1)
                .attr(si(0))
                .attr(file_name(parent, pseq, name, 1))
                .attr(data_nonresident(None, 0, alloc, len, None, 0))
                .build(),
        )
    }

    #[test]
    fn builds_a_small_tree() {
        let recs = vec![
            root(),
            dir(20, 5, 5, "Users"),
            dir(21, 20, 1, "me"),
            file(22, 21, 1, "a.bin", 8192, 5000),
            file(23, 21, 1, "b.bin", 4096, 10),
            file(24, 5, 5, "pagefile.sys", 1 << 20, 1 << 20),
            (
                25,
                RecordBuilder::new(1024, 25, 1)
                    .attr(si(0))
                    .attr(file_name(20, 1, "tiny.txt", 3))
                    .attr(data_resident(None, 42))
                    .build(),
            ),
        ];
        let b = build(recs, 64);
        let t = &b.tree;
        t.check_invariants().unwrap();
        assert_eq!(t.name(NodeId::ROOT), b"R:\\");
        assert_eq!(t.len(), 7);
        assert_eq!((b.files, b.dirs, b.skipped), (4, 2, 0));
        let a = t.lookup_relative(b"Users\\me\\a.bin").unwrap();
        assert_eq!((t.node(a).size, t.node(a).logical), (8192, 5000));
        let me = t.lookup_relative(b"users\\ME").unwrap();
        assert_eq!((t.node(me).size, t.node(me).logical), (12288, 5010));
        let users = t.lookup_relative(b"Users").unwrap();
        assert_eq!(t.node(users).logical, 5010 + 42);
        assert_eq!(t.node(NodeId::ROOT).size, 12288 + (1 << 20));
        assert_eq!(b.frn.get(22), Some(a));
        assert_eq!(b.frn.get(5), Some(NodeId::ROOT));
        assert_eq!(b.frn.get(30), None);
        // Parents precede children.
        for id in 1..t.len() {
            assert!(t.node(NodeId(id as u32)).parent.0 < id as u32);
        }
    }

    #[test]
    fn hard_links_count_once() {
        let recs = vec![
            root(),
            dir(20, 5, 5, "a"),
            dir(21, 20, 1, "deeper"),
            (
                30,
                RecordBuilder::new(1024, 30, 1)
                    .attr(si(0))
                    .attr(file_name(21, 1, "link-deep.bin", 1))
                    .attr(file_name(20, 1, "link-shallow.bin", 1))
                    .attr(file_name(20, 1, "LINK-S~1.BIN", 2))
                    .attr(data_nonresident(None, 0, 4096, 4000, None, 0))
                    .build(),
            ),
            // A third link stored in an extension record.
            (
                40,
                RecordBuilder::new(1024, 40, 1)
                    .base(30, 1)
                    .attr(file_name(5, 5, "link-root.bin", 0))
                    .build(),
            ),
        ];
        let b = build(recs, 64);
        let t = &b.tree;
        t.check_invariants().unwrap();
        // Breadth first: the link closest to the root gets the bytes.
        let root_link = t.lookup_relative(b"link-root.bin").unwrap();
        assert_eq!(t.node(root_link).size, 4096);
        assert!(!t.node(root_link).flags.contains(NodeFlags::HARDLINK));
        for p in ["a\\link-shallow.bin", "a\\deeper\\link-deep.bin"] {
            let n = t.lookup_relative(p.as_bytes()).unwrap();
            assert!(t.node(n).flags.contains(NodeFlags::HARDLINK), "{p}");
            assert_eq!((t.node(n).size, t.node(n).logical), (0, 0));
        }
        assert!(t.lookup_relative(b"a\\LINK-S~1.BIN").is_none());
        assert_eq!(t.node(NodeId::ROOT).size, 4096);
        assert_eq!(b.frn.get(30), Some(root_link));
        assert_eq!(b.files, 3);
    }

    #[test]
    fn orphans_cycles_and_stale_parents_are_skipped() {
        let recs = vec![
            root(),
            dir(20, 5, 5, "ok"),
            // Parent record 99 is not in use.
            file(21, 99, 1, "orphan", 4096, 1),
            // Parent reference with a stale sequence number.
            file(22, 20, 9, "stale", 4096, 1),
            // Two directories that are each other's parent.
            dir(23, 24, 1, "cycle-a"),
            dir(24, 23, 1, "cycle-b"),
            // Parent is a file.
            file(25, 21, 1, "under-file", 4096, 1),
            // Parent reference beyond the MFT.
            file(26, 1 << 40, 1, "far", 4096, 1),
            file(27, 20, 1, "kept", 4096, 7),
        ];
        let b = build(recs, 64);
        let t = &b.tree;
        t.check_invariants().unwrap();
        assert_eq!(t.len(), 3, "root, ok, kept");
        assert_eq!(b.skipped, 6);
        assert_eq!(t.node(NodeId::ROOT).logical, 7);
    }

    #[test]
    fn metafiles_reparse_and_extras() {
        let recs = vec![
            root(),
            (
                0,
                RecordBuilder::new(1024, 0, 1)
                    .attr(si(0x6))
                    .attr(file_name(5, 5, "$MFT", 3))
                    .attr(data_nonresident(None, 0, 1 << 20, 1 << 20, None, 0))
                    .build(),
            ),
            (11, {
                RecordBuilder::new(1024, 11, 11)
                    .dir()
                    .attr(si(0x6))
                    .attr(file_name(5, 5, "$Extend", 3))
                    .build()
            }),
            (
                40,
                RecordBuilder::new(1024, 40, 1)
                    .attr(si(0x6 | 0x20))
                    .attr(file_name(11, 11, "$UsnJrnl", 3))
                    .attr(data_resident(Some("$Max"), 32))
                    .attr(data_nonresident(
                        Some("$J"),
                        0,
                        1 << 22,
                        1 << 34,
                        Some(1 << 22),
                        0x8000,
                    ))
                    .build(),
            ),
            (
                41,
                RecordBuilder::new(1024, 41, 1)
                    .attr(si(0))
                    .attr(file_name(5, 5, "user-with-stream", 1))
                    .attr(data_nonresident(None, 0, 4096, 100, None, 0))
                    .attr(data_nonresident(Some("big"), 0, 1 << 20, 1 << 20, None, 0))
                    .build(),
            ),
            (
                42,
                RecordBuilder::new(1024, 42, 1)
                    .dir()
                    .attr(si(0x400))
                    .attr(file_name(5, 5, "junction", 1))
                    .attr(reparse(0xA000_0003))
                    .build(),
            ),
            (
                43,
                RecordBuilder::new(1024, 43, 1)
                    .attr(si(0x400 | 0x0040_0000))
                    .attr(file_name(5, 5, "cloud.docx", 1))
                    .attr(reparse(0x9000_001A))
                    .attr(data_nonresident(None, 0, 0, 1 << 20, Some(0), 0x8000))
                    .build(),
            ),
            (
                44,
                RecordBuilder::new(1024, 44, 1)
                    .attr(si(0x400))
                    .attr(file_name_ex(5, 5, "wof.dll", 1, 0x400, 0x8000_0017))
                    .attr(data_nonresident(None, 0, 0, 30_000, Some(0), 0x8000))
                    .attr(data_nonresident(
                        Some("WofCompressedData"),
                        0,
                        8192,
                        7000,
                        None,
                        0,
                    ))
                    .build(),
            ),
        ];
        let b = build(recs, 64);
        let t = &b.tree;
        t.check_invariants().unwrap();
        let mft = t.lookup_relative(b"$MFT").unwrap();
        assert!(t.node(mft).flags.contains(NodeFlags::METAFILE));
        assert_eq!(t.node(mft).size, 1 << 20);
        let usn = t.lookup_relative(b"$Extend\\$UsnJrnl").unwrap();
        assert!(t.node(usn).flags.contains(NodeFlags::METAFILE));
        assert_eq!(t.node(usn).size, 1 << 22, "metafile named streams count");
        assert_eq!(t.node(usn).logical, 1 << 22, "sparse: logical = on disk");
        let user = t.lookup_relative(b"user-with-stream").unwrap();
        assert_eq!((t.node(user).size, t.node(user).logical), (4096, 100));
        assert!(!t.node(user).flags.contains(NodeFlags::METAFILE));
        let j = t.lookup_relative(b"junction").unwrap();
        assert!(
            t.node(j)
                .flags
                .contains(NodeFlags::REPARSE | NodeFlags::DIR)
        );
        let c = t.lookup_relative(b"cloud.docx").unwrap();
        assert!(t.node(c).flags.contains(NodeFlags::CLOUD));
        assert!(!t.node(c).flags.contains(NodeFlags::REPARSE));
        assert_eq!((t.node(c).size, t.node(c).logical), (0, 1 << 20));
        let w = t.lookup_relative(b"wof.dll").unwrap();
        assert_eq!((t.node(w).size, t.node(w).logical), (8192, 30_000));
        assert!(t.node(w).flags.contains(NodeFlags::COMPRESSED));
    }
}
