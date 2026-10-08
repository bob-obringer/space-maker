//! Parallel directory walker.
//!
//! Each rayon worker appends nodes to its own arena, so a directory's children
//! end up contiguous and nothing is allocated per file. Directory metadata is
//! read with `getattrlistbulk`, which returns a whole batch of entries per
//! syscall instead of one `lstat` per file. When the scan finishes the arenas
//! are stitched into a single `Tree`.

use crate::tree::{Names, Node, Nodes, Tree, DIR, NO_PARENT, REMOVED};
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    pub current: Mutex<String>,
}

struct Ctx<'a> {
    progress: &'a Progress,
    /// Devices we are allowed to descend into (the scan root's, plus the APFS
    /// data volume so that firmlinked folders like /Users are included).
    devices: Vec<u64>,
    /// Hard-linked files we have already counted, keyed by (dev, inode).
    seen_links: Mutex<HashSet<(u64, u64)>>,
}

/// Paths that would double count or wander into other volumes.
const SKIP: &[&str] = &["/System/Volumes/Data", "/Volumes", "/dev", "/private/var/vm"];

#[derive(Default)]
struct Arena {
    nodes: Nodes,
    names: Names,
}

thread_local! {
    static ARENA: RefCell<Arena> = RefCell::new(Arena::default());
    static ATTR_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 128 * 1024]);
}

fn arena_index() -> u8 {
    rayon::current_thread_index().map(|i| i as u8 + 1).unwrap_or(0)
}

/// An open directory, closed on drop.
struct DirFd(libc::c_int);

impl Drop for DirFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

impl DirFd {
    fn open(path: &Path) -> Option<DirFd> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let fd = unsafe { libc::open(c.as_ptr(), DIR_FLAGS) };
        (fd >= 0).then_some(DirFd(fd))
    }

    /// Open a child by name relative to this directory: the kernel resolves
    /// one component instead of re-walking the whole path.
    fn open_child(&self, name: &[u8]) -> Option<DirFd> {
        let c = CString::new(name).ok()?;
        let fd = unsafe { libc::openat(self.0, c.as_ptr(), DIR_FLAGS) };
        (fd >= 0).then_some(DirFd(fd))
    }
}

/// Each directory being scanned holds its descriptor open until its children
/// are done, so allow plenty of them.
fn raise_fd_limit() {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        let want = rl.rlim_max.min(65536);
        if rl.rlim_cur < want {
            rl.rlim_cur = want;
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) };
        }
    }
}

/// A directory entry as read from disk.
pub struct Entry {
    pub name: (usize, usize), // range into the scratch name buffer
    pub dir: bool,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub links: u32,
}

/// Devices a scan of `root` may descend into.
pub fn devices_for(root: &Path) -> Vec<u64> {
    let mut devices = Vec::new();
    for p in [root, Path::new("/System/Volumes/Data")] {
        if let Ok(md) = fs::metadata(p) {
            devices.push(md.dev());
        }
    }
    devices
}

/// Whether a subdirectory should be descended into.
pub fn keep_dir(devices: &[u64], dev: u64, child: &Path) -> bool {
    devices.contains(&dev) && !SKIP.iter().any(|s| child == Path::new(s))
}

/// Read one directory's entries (directories and regular files only).
pub fn read_entries(path: &Path) -> std::io::Result<(Vec<u8>, Vec<Entry>)> {
    let (mut names, mut entries) = (Vec::new(), Vec::new());
    if let Some(fd) = DirFd::open(path) {
        if read_bulk(&fd, &mut names, &mut entries).is_ok() {
            return Ok((names, entries));
        }
        names.clear();
        entries.clear();
    }
    read_std(path, &mut names, &mut entries)?;
    Ok((names, entries))
}

pub fn new_pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(16 * 1024 * 1024)
        .build()
        .expect("thread pool")
}

/// Full scan with a dedicated pool sized for the machine.
pub fn scan(root: &Path, progress: &Progress) -> Tree {
    let threads = std::env::var("SM_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()))
        .clamp(1, 250);
    scan_with(&new_pool(threads), root, progress)
}

/// Scan using an existing pool. Scans must not run concurrently on the same
/// pool: each one gathers every worker's arena when it finishes.
pub fn scan_with(pool: &rayon::ThreadPool, root: &Path, progress: &Progress) -> Tree {
    let ctx = Ctx {
        progress,
        devices: devices_for(root),
        seen_links: Mutex::new(HashSet::new()),
    };
    raise_fd_limit();
    let root_arena = pool.install(|| {
        let me = arena_index();
        let root_idx = ARENA.with_borrow_mut(|a| a.nodes.push(Node { flags: DIR, parent: NO_PARENT, ..Node::default() }));
        assert_eq!(root_idx, 0, "fresh pool starts with empty arenas");
        let r = scan_dir(root, DirFd::open(root), &ctx);
        ARENA.with_borrow_mut(|a| {
            let n = &mut a.nodes[0];
            n.first = r.first;
            n.len = r.len;
            n.first_arena = r.arena;
            n.size = r.size;
            n.count = r.count;
        });
        me
    });

    let mut arenas: Vec<(u8, Arena)> = pool
        .broadcast(|_| (arena_index(), ARENA.take()))
        .into_iter()
        .filter(|(_, a)| a.nodes.len > 0)
        .collect();
    // The root must become node 0.
    arenas.sort_by_key(|(i, _)| (*i != root_arena, *i));
    stitch(arenas, root.to_path_buf())
}

struct DirResult {
    arena: u8,
    first: u32,
    len: u32,
    size: u64,
    count: u32,
}

fn scan_dir(path: &Path, fd: Option<DirFd>, ctx: &Ctx) -> DirResult {
    let p = ctx.progress;
    let empty = DirResult { arena: arena_index(), first: 0, len: 0, size: 0, count: 0 };
    if p.cancel.load(Ordering::Relaxed) {
        return empty;
    }
    p.dirs.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut cur) = p.current.try_lock() {
        cur.clear();
        cur.push_str(&path.to_string_lossy());
    }

    let mut names = Vec::new();
    let mut entries = Vec::new();
    let bulk_ok = fd.as_ref().is_some_and(|fd| read_bulk(fd, &mut names, &mut entries).is_ok());
    if !bulk_ok {
        names.clear();
        entries.clear();
        if read_std(path, &mut names, &mut entries).is_err() {
            p.errors.fetch_add(1, Ordering::Relaxed);
            return empty;
        }
    }

    // Keep what we want, deduping hard links and staying on our volumes.
    let mut files = 0u32;
    let mut bytes = 0u64;
    entries.retain(|e| {
        if e.dir {
            let child = path.join(std::ffi::OsStr::from_bytes(&names[e.name.0..e.name.1]));
            keep_dir(&ctx.devices, e.dev, &child)
        } else {
            if e.links > 1 && !ctx.seen_links.lock().unwrap().insert((e.dev, e.ino)) {
                return false;
            }
            files += 1;
            bytes += e.size;
            true
        }
    });
    p.files.fetch_add(files as u64, Ordering::Relaxed);
    p.bytes.fetch_add(bytes as u64, Ordering::Relaxed);

    // Append the children contiguously to this thread's arena.
    let arena = arena_index();
    let len = entries.len() as u32;
    let first = ARENA.with_borrow_mut(|a| {
        a.nodes.align_for(entries.len());
        let first = a.nodes.len as u32;
        for e in &entries {
            let name = &names[e.name.0..e.name.1.min(e.name.0 + u16::MAX as usize)];
            let name_off = a.names.push_slice(name) as u64;
            a.nodes.push(Node {
                size: if e.dir { 0 } else { e.size },
                name_off,
                name_len: name.len().min(u16::MAX as usize) as u16,
                flags: if e.dir { DIR } else { 0 },
                count: if e.dir { 0 } else { 1 },
                ..Node::default()
            });
        }
        first
    });

    let subdirs: Vec<(u32, PathBuf, std::ops::Range<usize>)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.dir)
        .map(|(i, e)| {
            let name = std::ffi::OsStr::from_bytes(&names[e.name.0..e.name.1]);
            (first + i as u32, path.join(name), e.name.0..e.name.1)
        })
        .collect();
    drop(entries);

    // Recurse. This stack frame stays on its thread, so the arena indices
    // recorded above remain ours to fill in afterwards. Our descriptor stays
    // open until then so children can be opened relative to it.
    let results: Vec<(u32, DirResult)> = subdirs
        .into_par_iter()
        .map(|(idx, child, name)| {
            let child_fd = match &fd {
                Some(fd) => fd.open_child(&names[name]),
                None => DirFd::open(&child),
            };
            (idx, scan_dir(&child, child_fd, ctx))
        })
        .collect();
    drop(fd);
    drop(names);

    ARENA.with_borrow_mut(|a| {
        for (idx, r) in &results {
            let n = &mut a.nodes[*idx as usize];
            n.first = r.first;
            n.len = r.len;
            n.first_arena = r.arena;
            n.size = r.size;
            n.count = r.count;
        }
        // Other directories scanned on this thread while we waited have
        // appended after our children, so only touch our own range.
        let by_size = |x: &Node, y: &Node| y.size.cmp(&x.size);
        let (size, count) = match a.nodes.run_mut(first as usize, len as usize) {
            Some(kids) => {
                kids.sort_unstable_by(by_size);
                (kids.iter().map(|k| k.size).sum(), kids.iter().map(|k| k.count).sum())
            }
            None => {
                // Huge directory spanning blocks: sort a copy and write it back.
                let mut kids: Vec<Node> = (first..first + len).map(|i| a.nodes[i as usize]).collect();
                kids.sort_unstable_by(by_size);
                for (i, k) in kids.iter().enumerate() {
                    a.nodes[first as usize + i] = *k;
                }
                (kids.iter().map(|k| k.size).sum(), kids.iter().map(|k| k.count).sum())
            }
        };
        DirResult { arena, first, len, size, count }
    })
}

/// Join per-thread arenas into one tree by moving their blocks, then turn
/// arena-local references into global ids.
fn stitch(arenas: Vec<(u8, Arena)>, root_path: PathBuf) -> Tree {
    let mut node_base = [0u32; 256];
    let mut name_base = [0u64; 256];
    let mut nodes = Nodes::default();
    let mut names = Names::default();
    let mut owner = Vec::new(); // arena of each node block
    for (i, a) in arenas {
        node_base[i as usize] = nodes.capacity() as u32;
        name_base[i as usize] = names.capacity() as u64;
        // Padding slots default to REMOVED, so they're skipped below.
        let a_nodes = a.nodes;
        owner.extend(std::iter::repeat_n(i, a_nodes.blocks.len()));
        nodes.blocks.extend(a_nodes.blocks);
        names.blocks.extend(a.names.blocks);
    }
    nodes.len = nodes.capacity();
    names.len = names.capacity();
    for (b, block) in nodes.blocks.iter_mut().enumerate() {
        let nb = name_base[owner[b] as usize];
        for n in block.iter_mut() {
            if n.flags & REMOVED == 0 {
                n.name_off += nb;
                n.first += node_base[n.first_arena as usize];
            }
        }
    }
    for id in 0..nodes.capacity() {
        let n = nodes[id];
        if n.flags & REMOVED != 0 {
            continue;
        }
        for k in n.first..n.first + n.len {
            nodes[k as usize].parent = id as u32;
        }
    }
    if nodes.capacity() > 0 {
        nodes[0].parent = NO_PARENT;
    }
    Tree::new(nodes, names, root_path)
}

// ------------------------------------------------------------------ reading

const VREG: u32 = 1;
const VDIR: u32 = 2;

/// Read a directory with `getattrlistbulk`.
fn read_bulk(dir: &DirFd, names: &mut Vec<u8>, out: &mut Vec<Entry>) -> std::io::Result<()> {
    let fd = dir.0;
    let mut attrs: libc::attrlist = unsafe { std::mem::zeroed() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attrs.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_FILEID;
    attrs.fileattr = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;

    let result = ATTR_BUF.with_borrow_mut(|buf| loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd,
                &mut attrs as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::FSOPT_PACK_INVAL_ATTRS as u64,
            )
        };
        if n < 0 {
            break Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            break Ok(());
        }
        let mut p = 0usize;
        for _ in 0..n {
            let rd_u32 = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
            let rd_u64 = |at: usize| u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap());
            let entry_len = rd_u32(p) as usize;
            let mut q = p + 4;
            let returned_file = rd_u32(q + 12); // attribute_set_t.fileattr
            q += std::mem::size_of::<libc::attribute_set_t>();
            let name_ref = q;
            let name_off = i32::from_ne_bytes(buf[q..q + 4].try_into().unwrap());
            let name_len = rd_u32(q + 4) as usize;
            q += 8;
            let dev = rd_u32(q) as i32 as u64;
            q += 4;
            let objtype = rd_u32(q);
            q += 4;
            let ino = rd_u64(q);
            q += 8;
            let start = (name_ref as isize + name_off as isize) as usize;
            // The name includes a trailing NUL.
            let name = &buf[start..start + name_len.saturating_sub(1)];
            match objtype {
                VDIR => {
                    let s = names.len();
                    names.extend_from_slice(name);
                    out.push(Entry { name: (s, names.len()), dir: true, size: 0, dev, ino, links: 1 });
                }
                VREG => {
                    let links = if returned_file & libc::ATTR_FILE_LINKCOUNT != 0 { rd_u32(q) } else { 1 };
                    let size = if returned_file & libc::ATTR_FILE_ALLOCSIZE != 0 { rd_u64(q + 4) } else { 0 };
                    let s = names.len();
                    names.extend_from_slice(name);
                    out.push(Entry { name: (s, names.len()), dir: false, size, dev, ino, links });
                }
                _ => {} // symlinks, devices, sockets: not counted
            }
            p += entry_len;
        }
    });
    result
}

/// Portable fallback for filesystems without `getattrlistbulk`.
fn read_std(path: &Path, names: &mut Vec<u8>, out: &mut Vec<Entry>) -> std::io::Result<()> {
    for entry in fs::read_dir(path)?.flatten() {
        // DirEntry::metadata uses lstat, so symlinks are never followed.
        let Ok(md) = entry.metadata() else { continue };
        let ft = md.file_type();
        if !ft.is_dir() && !ft.is_file() {
            continue;
        }
        let s = names.len();
        names.extend_from_slice(entry.file_name().as_bytes());
        out.push(Entry {
            name: (s, names.len()),
            dir: ft.is_dir(),
            size: if ft.is_file() { md.blocks() * 512 } else { 0 },
            dev: md.dev(),
            ino: md.ino(),
            links: if ft.is_dir() { 1 } else { md.nlink() as u32 },
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: usize) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![7u8; bytes]).unwrap();
        f.sync_all().unwrap();
    }

    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("space-maker-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big/inner")).unwrap();
        fs::create_dir_all(root.join("small")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        write(&root.join("big/movie.mov"), 4_000_000);
        write(&root.join("big/inner/data.db"), 1_000_000);
        write(&root.join("small/note.txt"), 10);
        // A hard link must only be counted once; a symlink must not be followed.
        fs::hard_link(root.join("big/movie.mov"), root.join("big/movie-alias.mov")).unwrap();
        std::os::unix::fs::symlink(root.join("big"), root.join("small/big-link")).unwrap();
        root
    }

    #[test]
    fn scans_sizes_and_structure() {
        let root = fixture("sizes");
        let tree = scan(&root, &Progress::default());
        let top = tree.children(0, 100);
        let names: Vec<_> = top.kids.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["big", "small", "empty"], "children sorted largest first");

        let big = &top.kids[0];
        assert!(big.dir && big.count == 2);
        assert!(big.size >= 5_000_000, "allocated size covers contents: {}", big.size);
        let small = &top.kids[1];
        assert_eq!(small.count, 1, "hard link deduped and symlink skipped");
        assert_eq!(tree.info(0).size, big.size + small.size);
        assert_eq!(top.kids[2].kids, 0);

        // Paths round-trip.
        let inner = tree.children(big.id, 10).kids.into_iter().find(|k| k.name == "inner").unwrap();
        assert_eq!(tree.path(inner.id), root.join("big/inner"));

        // Limit aggregates the remainder.
        let limited = tree.children(0, 1);
        let rest = limited.rest.expect("rest");
        assert_eq!(rest.count, 2);
        assert_eq!(rest.size, small.size);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn summary_and_remove() {
        let root = fixture("summary");
        let mut tree = scan(&root, &Progress::default());
        let s = tree.summary(0, 2);
        assert_eq!(s.largest[0].info.name, "movie.mov");
        assert_eq!(s.largest[0].path, "big/movie.mov");
        assert!(s.categories[0] > 0, "video bucket");
        assert!(s.categories[7] > 0, "data bucket");

        let total = tree.info(0).size;
        let big = tree.children(0, 10).kids[0].clone();
        let movie = tree.children(big.id, 10).kids.into_iter().find(|k| k.name == "movie.mov").unwrap();
        let updates = tree.remove(movie.id);
        assert_eq!(updates.len(), 2, "parent and root updated");
        assert_eq!(tree.info(0).size, total - movie.size);
        assert_eq!(tree.info(big.id).count, 1);
        // `big` shrank below `small`? Order must stay sorted by size.
        let order: Vec<u64> = tree.children(0, 10).kids.iter().map(|k| k.size).collect();
        assert!(order.windows(2).all(|w| w[0] >= w[1]), "{order:?}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bulk_reader_matches_std() {
        let root = fixture("readers");
        for dir in ["big", "small", "empty"] {
            let path = root.join(dir);
            let (mut na, mut a, mut nb, mut b) = (vec![], vec![], vec![], vec![]);
            read_bulk(&DirFd::open(&path).unwrap(), &mut na, &mut a).unwrap();
            read_std(&path, &mut nb, &mut b).unwrap();
            let key = |names: &[u8], e: &Entry| (names[e.name.0..e.name.1].to_vec(), e.dir, e.size, e.ino, e.links);
            let mut ka: Vec<_> = a.iter().map(|e| key(&na, e)).collect();
            let mut kb: Vec<_> = b.iter().map(|e| key(&nb, e)).collect();
            ka.sort();
            kb.sort();
            assert_eq!(ka, kb, "{dir}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn chunk_edges() {
        let mut c = crate::tree::Chunked::<u8, 4>::default();
        assert_eq!(c.run_mut(0, 0).map(|r| r.len()), Some(0));
        assert_eq!(c.push_slice(&[]), 0);
        assert_eq!(c.push_slice(&[1, 2, 3]), 0);
        assert_eq!(c.push_slice(&[4, 5]), 4, "doesn't straddle blocks");
        assert_eq!(c.slice(4, 2), &[4, 5]);
        c.len = 8;
        assert_eq!(c.run_mut(8, 0).map(|r| r.len()), Some(0), "empty run at block boundary");
        assert!(c.run_mut(3, 3).is_none());
    }

    #[test]
    fn cancel_stops_early() {
        let root = fixture("cancel");
        let p = Progress::default();
        p.cancel.store(true, Ordering::Relaxed);
        let tree = scan(&root, &p);
        assert_eq!(tree.info(0).kids, 0);
        let _ = fs::remove_dir_all(&root);
    }

    /// Read-only benchmark against the real home folder:
    /// `cargo test --release bench_home -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_home() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let p = Progress::default();
        let t = std::time::Instant::now();
        let tree = scan(&home, &p);
        let root = tree.info(0);
        eprintln!(
            "scanned {} files / {} dirs ({:.1} GB) in {:?}, {} unreadable, {} nodes",
            root.count,
            p.dirs.load(Ordering::Relaxed),
            root.size as f64 / 1e9,
            t.elapsed(),
            p.errors.load(Ordering::Relaxed),
            tree.nodes.len
        );
        let t = std::time::Instant::now();
        let s = tree.summary(0, 14);
        eprintln!("summary of root in {:?}; largest: {} ({:.2} GB)", t.elapsed(), s.largest[0].path, s.largest[0].info.size as f64 / 1e9);
    }
}
