//! Saved scans for instant launch.
//!
//! A cache file is a small plain header followed by a zstd stream of node
//! records written breadth-first. Writing breadth-first compacts the tree on
//! the fly (removed nodes vanish, children become contiguous again) without
//! building a second copy in memory.

use crate::tree::{Names, Node, Nodes, Tree, DIR, NO_PARENT};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SMCACHE\x01";

#[derive(Serialize, Clone)]
pub struct Header {
    pub root: String,
    /// FSEvents id that the saved tree is current through.
    pub event_id: u64,
    pub volume: [u8; 16],
    pub scanned_at: u64,
    pub elapsed_ms: u64,
    pub size: u64,
    pub files: u64,
}

#[derive(Serialize)]
pub struct CachedScan {
    #[serde(flatten)]
    pub header: Header,
    /// Size of the cache file itself.
    pub bytes: u64,
}

pub fn dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Caches/com.bobringer.spacemaker/scans")
}

pub fn file_for(root: &Path) -> PathBuf {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.as_os_str().as_encoded_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    dir().join(format!("{h:016x}.smc"))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn put_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn put_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn get_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn get_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn write_header(w: &mut impl Write, h: &Header) -> io::Result<()> {
    w.write_all(MAGIC)?;
    put_u32(w, h.root.len() as u32)?;
    w.write_all(h.root.as_bytes())?;
    put_u64(w, h.event_id)?;
    w.write_all(&h.volume)?;
    for v in [h.scanned_at, h.elapsed_ms, h.size, h.files] {
        put_u64(w, v)?;
    }
    Ok(())
}

fn read_header(r: &mut impl Read) -> io::Result<Header> {
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a scan cache"));
    }
    let n = get_u32(r)? as usize;
    let mut root = vec![0; n];
    r.read_exact(&mut root)?;
    let event_id = get_u64(r)?;
    let mut volume = [0; 16];
    r.read_exact(&mut volume)?;
    Ok(Header {
        root: String::from_utf8_lossy(&root).into_owned(),
        event_id,
        volume,
        scanned_at: get_u64(r)?,
        elapsed_ms: get_u64(r)?,
        size: get_u64(r)?,
        files: get_u64(r)?,
    })
}

/// Write `tree` to its cache file (atomically, via a temp file).
pub fn save(tree: &Tree, header: &Header) -> io::Result<u64> {
    fs::create_dir_all(dir())?;
    save_to(tree, header, &file_for(&tree.root_path))
}

pub fn save_to(tree: &Tree, header: &Header, path: &Path) -> io::Result<u64> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = BufWriter::new(File::create(&tmp)?);
        write_header(&mut f, header)?;
        let mut z = zstd::Encoder::new(f, 3)?;
        let _ = z.multithread(4);
        let mut w = BufWriter::with_capacity(1 << 20, z);

        // Breadth-first: a node's new id is its position in this order, and
        // each node's children are enqueued (so numbered) consecutively.
        let mut queue: VecDeque<(u32, u32)> = VecDeque::from([(0, NO_PARENT)]);
        let mut me = 0u32; // new id of the node being written
        let mut next_free = 1u32; // new id of the next node to be enqueued
        while let Some((old, parent)) = queue.pop_front() {
            let n = &tree.nodes[old as usize];
            let kids = if n.flags & DIR != 0 { tree.sorted_kids(old) } else { Vec::new() };
            let name = if old == 0 { &[][..] } else { tree.name_bytes(old) };
            put_u64(&mut w, n.size)?;
            put_u32(&mut w, if kids.is_empty() { 0 } else { next_free })?;
            put_u32(&mut w, kids.len() as u32)?;
            put_u32(&mut w, parent)?;
            put_u32(&mut w, n.count)?;
            w.write_all(&[n.flags & DIR])?;
            w.write_all(&(name.len() as u16).to_le_bytes())?;
            w.write_all(name)?;
            next_free += kids.len() as u32;
            queue.extend(kids.into_iter().map(|k| (k, me)));
            me += 1;
        }
        let z = w.into_inner().map_err(|e| e.into_error())?;
        z.finish()?.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(fs::metadata(path)?.len())
}

pub fn load(path: &Path) -> io::Result<(Header, Tree)> {
    let mut f = BufReader::new(File::open(path)?);
    let header = read_header(&mut f)?;
    let mut z = BufReader::with_capacity(1 << 20, zstd::Decoder::new(f)?);
    let mut nodes = Nodes::default();
    let mut names = Names::default();
    let mut name = Vec::with_capacity(256);
    loop {
        let size = match get_u64(&mut z) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        let first = get_u32(&mut z)?;
        let len = get_u32(&mut z)?;
        let parent = get_u32(&mut z)?;
        let count = get_u32(&mut z)?;
        let mut b = [0; 3];
        z.read_exact(&mut b)?;
        let name_len = u16::from_le_bytes([b[1], b[2]]) as usize;
        name.resize(name_len, 0);
        z.read_exact(&mut name)?;
        let name_off = names.push_slice(&name) as u64;
        nodes.push(Node {
            size,
            name_off,
            first,
            len,
            parent,
            count,
            name_len: name_len as u16,
            flags: b[0] & DIR,
            first_arena: 0,
        });
    }
    if nodes.len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty scan cache"));
    }
    let root = PathBuf::from(&header.root);
    Ok((header, Tree::new(nodes, names, root)))
}

pub fn list() -> Vec<CachedScan> {
    let Ok(rd) = fs::read_dir(dir()) else { return Vec::new() };
    let mut out: Vec<CachedScan> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "smc"))
        .filter_map(|e| {
            let bytes = e.metadata().ok()?.len();
            let header = read_header(&mut BufReader::new(File::open(e.path()).ok()?)).ok()?;
            Some(CachedScan { header, bytes })
        })
        .collect();
    out.sort_by(|a, b| b.header.scanned_at.cmp(&a.header.scanned_at));
    out
}

pub fn header_for(root: &Path) -> Option<Header> {
    read_header(&mut BufReader::new(File::open(file_for(root)).ok()?)).ok()
}

pub fn clear() -> io::Result<()> {
    match fs::remove_dir_all(dir()) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, Progress};
    use crate::tree::testutil::snapshot;

    #[test]
    fn round_trip_with_edits() {
        let root = std::env::temp_dir().join(format!("space-maker-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/one.bin"), vec![1u8; 50_000]).unwrap();
        fs::write(root.join("a/two.txt"), vec![2u8; 9_000]).unwrap();
        fs::write(root.join("c/three.mov"), vec![3u8; 70_000]).unwrap();
        let mut tree = scan(&root, &Progress::default());

        // Remove a file and graft a new folder, as live updates would.
        let (a, _) = tree.find(&root.join("a"));
        let two = tree.kids(a).find(|&k| tree.name_bytes(k) == b"two.txt").unwrap();
        tree.remove(two);
        fs::create_dir_all(root.join("extra")).unwrap();
        fs::write(root.join("extra/four.zip"), vec![4u8; 20_000]).unwrap();
        let sub = scan(&root.join("extra"), &Progress::default());
        let g = tree.graft(Some(0), b"extra", &sub);
        let (s, c) = (tree.nodes[g as usize].size as i64, tree.nodes[g as usize].count as i64);
        tree.add_delta(0, s, c, &mut Default::default());

        let header = Header {
            root: root.to_string_lossy().into(),
            event_id: 42,
            volume: [7; 16],
            scanned_at: 1,
            elapsed_ms: 2,
            size: tree.nodes[0].size,
            files: tree.nodes[0].count as u64,
        };
        let file = root.with_extension("smc");
        save_to(&tree, &header, &file).unwrap();
        let (h, loaded) = load(&file).unwrap();
        assert_eq!(h.event_id, 42);
        assert_eq!(h.volume, [7; 16]);
        assert_eq!(snapshot(&loaded), snapshot(&tree));
        assert!(snapshot(&loaded).contains_key("/extra/four.zip"));
        assert!(!snapshot(&loaded).contains_key("/a/two.txt"));
        // Parent links survive.
        let (b, exact) = loaded.find(&root.join("a/b"));
        assert!(exact);
        assert_eq!(loaded.path(b), root.join("a/b"));

        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir_all(&root);
    }

    /// `cargo test --release bench_cache -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_cache() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let t = std::time::Instant::now();
        let tree = scan(&home, &Progress::default());
        eprintln!("scan {:?}", t.elapsed());
        let header = Header { root: home.to_string_lossy().into(), event_id: 0, volume: [0; 16], scanned_at: 0, elapsed_ms: 0, size: 0, files: 0 };
        let file = std::env::temp_dir().join("space-maker-bench.smc");
        let t = std::time::Instant::now();
        let bytes = save_to(&tree, &header, &file).unwrap();
        eprintln!("save {:?} -> {:.0} MB", t.elapsed(), bytes as f64 / 1e6);
        drop(tree);
        let t = std::time::Instant::now();
        let (_, loaded) = load(&file).unwrap();
        eprintln!("load {:?} ({} nodes)", t.elapsed(), loaded.nodes.len);
        let _ = fs::remove_file(&file);
    }
}
