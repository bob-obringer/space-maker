//! In-memory file tree produced by a scan. Nodes live in one flat arena and
//! are addressed by `u32` ids so the webview can lazily ask for any subtree.
//! Each node is 40 bytes; names live in a single shared byte buffer. Ids never
//! change after a scan (the UI holds on to them), so removal only marks nodes.

use serde::Serialize;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

pub const DIR: u8 = 1;
pub const REMOVED: u8 = 2;
pub const NO_PARENT: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub struct Node {
    /// Allocated bytes on disk (whole subtree for directories).
    pub size: u64,
    pub name_off: u64,
    /// Children occupy `first..first + len`, sorted largest first.
    pub first: u32,
    pub len: u32,
    pub parent: u32,
    /// Number of files in the subtree (1 for a file).
    pub count: u32,
    pub name_len: u16,
    pub flags: u8,
    /// Which scan-time arena `first` refers to; only meaningful mid-scan.
    pub first_arena: u8,
}

/// Append-only storage in fixed-size blocks: growing never copies or leaves
/// doubling slack behind, and blocks from several arenas can be stitched
/// together by moving pointers.
pub struct Chunked<T, const C: usize> {
    pub blocks: Vec<Box<[T]>>,
    pub len: usize,
}

impl<T: Copy + Default, const C: usize> Default for Chunked<T, C> {
    fn default() -> Self {
        Self { blocks: Vec::new(), len: 0 }
    }
}

impl<T: Copy + Default, const C: usize> Chunked<T, C> {
    fn grow(&mut self) {
        self.blocks.push(vec![T::default(); C].into_boxed_slice());
    }

    pub fn push(&mut self, v: T) -> usize {
        if self.len == self.blocks.len() * C {
            self.grow();
        }
        let i = self.len;
        self.blocks[i / C][i % C] = v;
        self.len += 1;
        i
    }

    /// Start a run of `n` items that should share a block if possible.
    pub fn align_for(&mut self, n: usize) {
        let room = C - self.len % C;
        if self.len % C != 0 && n <= C && n > room {
            self.len += room;
        }
    }

    /// Append a slice without splitting it across blocks (`v.len() <= C`).
    pub fn push_slice(&mut self, v: &[T]) -> usize {
        if v.is_empty() {
            return self.len;
        }
        self.align_for(v.len());
        if self.len + v.len() > self.blocks.len() * C {
            self.grow();
        }
        let start = self.len;
        self.blocks[start / C][start % C..start % C + v.len()].copy_from_slice(v);
        self.len += v.len();
        start
    }

    /// Total addressable slots (including padding at the end of blocks).
    pub fn capacity(&self) -> usize {
        self.blocks.len() * C
    }

    /// Mutable view of `start..start + n` when it lies inside one block.
    pub fn run_mut(&mut self, start: usize, n: usize) -> Option<&mut [T]> {
        if n == 0 {
            return Some(&mut []);
        }
        let (b, o) = (start / C, start % C);
        (o + n <= C).then(|| &mut self.blocks[b][o..o + n])
    }

    pub fn slice(&self, start: usize, n: usize) -> &[T] {
        if n == 0 {
            return &[];
        }
        &self.blocks[start / C][start % C..start % C + n]
    }
}

impl<T, const C: usize> std::ops::Index<usize> for Chunked<T, C> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        &self.blocks[i / C][i % C]
    }
}

impl<T, const C: usize> std::ops::IndexMut<usize> for Chunked<T, C> {
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.blocks[i / C][i % C]
    }
}

pub type Nodes = Chunked<Node, { 1 << 16 }>;
pub type Names = Chunked<u8, { 1 << 20 }>;

impl Default for Node {
    /// Unused slots (block padding) read as removed so nothing can reach them.
    fn default() -> Self {
        Node {
            size: 0,
            name_off: 0,
            first: 0,
            len: 0,
            parent: NO_PARENT,
            count: 0,
            name_len: 0,
            flags: REMOVED,
            first_arena: 0,
        }
    }
}

pub struct Tree {
    pub nodes: Nodes,
    pub names: Names,
    /// Absolute path of the scan root (node 0).
    pub root_path: PathBuf,
    /// Children added after the scan. Ranges can't grow in place and ids
    /// must stay stable, so newcomers live here.
    pub extras: HashMap<u32, Vec<u32>>,
}

#[derive(Serialize, Clone)]
pub struct NodeInfo {
    pub id: u32,
    pub name: String,
    pub size: u64,
    pub count: u64,
    pub dir: bool,
    pub kids: u32,
}

#[derive(Serialize)]
pub struct Rest {
    pub count: u32,
    pub size: u64,
}

#[derive(Serialize)]
pub struct Children {
    pub id: u32,
    pub kids: Vec<NodeInfo>,
    /// Aggregate of the children that were cut off by `limit`.
    pub rest: Option<Rest>,
}

#[derive(Serialize)]
pub struct Largest {
    pub info: NodeInfo,
    pub path: String,
}

#[derive(Serialize)]
pub struct Summary {
    pub largest: Vec<Largest>,
    /// Bytes per category, indexed like `category()`.
    pub categories: Vec<u64>,
}

impl Tree {
    pub fn is_live(&self, id: u32) -> bool {
        (id as usize) < self.nodes.capacity() && self.nodes[id as usize].flags & REMOVED == 0
    }

    /// Live and still connected to the root (no removed ancestor).
    pub fn reachable(&self, id: u32) -> bool {
        let mut cur = id;
        loop {
            if !self.is_live(cur) {
                return false;
            }
            if cur == 0 {
                return true;
            }
            cur = self.nodes[cur as usize].parent;
            if cur == NO_PARENT {
                return false;
            }
        }
    }

    pub fn name_bytes(&self, id: u32) -> &[u8] {
        let n = &self.nodes[id as usize];
        self.names.slice(n.name_off as usize, n.name_len as usize)
    }

    pub fn name(&self, id: u32) -> String {
        if id == 0 {
            return self.root_path.to_string_lossy().into_owned();
        }
        String::from_utf8_lossy(self.name_bytes(id)).into_owned()
    }

    pub fn new(nodes: Nodes, names: Names, root_path: PathBuf) -> Tree {
        Tree { nodes, names, root_path, extras: HashMap::new() }
    }

    /// Live children: the scanned range plus anything added since.
    pub fn kids(&self, id: u32) -> impl Iterator<Item = u32> + '_ {
        let n = &self.nodes[id as usize];
        let extra = self.extras.get(&id).map(|v| v.as_slice()).unwrap_or(&[]);
        (n.first..n.first + n.len)
            .chain(extra.iter().copied())
            .filter(|&k| self.nodes[k as usize].flags & REMOVED == 0)
    }

    /// Find the node for `path`. Returns the deepest existing node and
    /// whether it is the path itself.
    pub fn find(&self, path: &Path) -> (u32, bool) {
        let Ok(rel) = path.strip_prefix(&self.root_path) else { return (0, false) };
        let mut cur = 0u32;
        for part in rel.components() {
            let want = part.as_os_str().as_encoded_bytes();
            let hit = self
                .kids(cur)
                .find(|&k| self.name_bytes(k) == want)
                .or_else(|| {
                    // APFS is usually case-insensitive; events may differ in case.
                    let want = String::from_utf8_lossy(want).to_lowercase();
                    self.kids(cur).find(|&k| String::from_utf8_lossy(self.name_bytes(k)).to_lowercase() == want)
                });
            match hit {
                Some(k) if self.nodes[k as usize].flags & DIR != 0 => cur = k,
                _ => return (cur, false),
            }
        }
        (cur, true)
    }

    /// Apply a size/count change to `from` and all of its ancestors.
    pub fn add_delta(&mut self, from: u32, dsize: i64, dcount: i64, out: &mut HashMap<u32, (u64, u64)>) {
        let mut cur = from;
        while cur != NO_PARENT {
            let n = &mut self.nodes[cur as usize];
            n.size = n.size.saturating_add_signed(dsize);
            n.count = (n.count as i64 + dcount).max(0) as u32;
            out.insert(cur, (n.size, n.count as u64));
            cur = n.parent;
        }
    }

    /// Copy a separately scanned tree in. Its root becomes a new node named
    /// `name`; with `parent` it is attached as an extra child (sizes of the
    /// ancestors are left to the caller).
    pub fn graft(&mut self, parent: Option<u32>, name: &[u8], sub: &Tree) -> u32 {
        let cap = sub.nodes.capacity();
        let mut map = vec![u32::MAX; cap];
        for old in 0..cap {
            let n = sub.nodes[old];
            if n.flags & REMOVED != 0 {
                continue; // block padding
            }
            let nm = if old == 0 { name } else { sub.name_bytes(old as u32) };
            let mut c = n;
            c.name_off = self.names.push_slice(nm) as u64;
            c.name_len = nm.len() as u16;
            map[old] = self.nodes.push(c) as u32;
        }
        for old in 0..cap {
            let new = map[old];
            if new == u32::MAX {
                continue;
            }
            let n = &mut self.nodes[new as usize];
            if n.len > 0 {
                n.first = map[n.first as usize];
            }
            n.parent = if old == 0 { parent.unwrap_or(NO_PARENT) } else { map[n.parent as usize] };
        }
        if let Some(p) = parent {
            self.extras.entry(p).or_default().push(map[0]);
        }
        map[0]
    }

    /// Swap `id`'s whole subtree for a freshly scanned one. Descendants get
    /// new ids; `id` keeps its own. Returns (size delta, count delta).
    pub fn replace_subtree(&mut self, id: u32, sub: &Tree) -> (i64, i64) {
        let old: Vec<u32> = self.kids(id).collect();
        for k in old {
            self.nodes[k as usize].flags |= REMOVED;
        }
        let tmp = self.graft(None, b"", sub);
        let t = self.nodes[tmp as usize];
        self.nodes[tmp as usize].flags |= REMOVED;
        let kids: Vec<u32> = self.kids(tmp).collect();
        for &k in &kids {
            self.nodes[k as usize].parent = id;
        }
        let n = &mut self.nodes[id as usize];
        let delta = (t.size as i64 - n.size as i64, t.count as i64 - n.count as i64);
        n.first = t.first;
        n.len = t.len;
        self.extras.remove(&id);
        delta
    }

    /// Add a single file under `parent` (sizes of ancestors left to caller).
    pub fn add_file(&mut self, parent: u32, name: &[u8], size: u64) -> u32 {
        let name_off = self.names.push_slice(name) as u64;
        let id = self.nodes.push(Node {
            size,
            name_off,
            name_len: name.len() as u16,
            parent,
            count: 1,
            flags: 0,
            ..Node::default()
        }) as u32;
        self.extras.entry(parent).or_default().push(id);
        id
    }

    /// Live children sorted by current size, largest first.
    pub fn sorted_kids(&self, id: u32) -> Vec<u32> {
        let mut kids: Vec<u32> = self.kids(id).collect();
        kids.sort_by(|&a, &b| self.nodes[b as usize].size.cmp(&self.nodes[a as usize].size));
        kids
    }

    pub fn info(&self, id: u32) -> NodeInfo {
        let n = &self.nodes[id as usize];
        NodeInfo {
            id,
            name: self.name(id),
            size: n.size,
            count: n.count as u64,
            dir: n.flags & DIR != 0,
            kids: self.kids(id).count() as u32,
        }
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != 0 && cur != NO_PARENT {
            parts.push(cur);
            cur = self.nodes[cur as usize].parent;
        }
        let mut p = self.root_path.clone();
        for &part in parts.iter().rev() {
            p.push(std::ffi::OsStr::from_bytes(self.name_bytes(part)));
        }
        p
    }

    pub fn children(&self, id: u32, limit: usize) -> Children {
        let kids = self.sorted_kids(id);
        let shown = kids.len().min(limit);
        let rest = (kids.len() > shown).then(|| Rest {
            count: (kids.len() - shown) as u32,
            size: kids[shown..].iter().map(|&c| self.nodes[c as usize].size).sum(),
        });
        let kids = kids[..shown].iter().map(|&c| self.info(c)).collect();
        Children { id, kids, rest }
    }

    /// Largest files in a subtree plus a per-category byte breakdown.
    pub fn summary(&self, id: u32, n: usize) -> Summary {
        let mut heap: BinaryHeap<std::cmp::Reverse<(u64, u32)>> = BinaryHeap::new();
        let mut categories = vec![0u64; CATEGORY_COUNT];
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let node = &self.nodes[cur as usize];
            if node.flags & DIR != 0 {
                stack.extend(self.kids(cur));
                continue;
            }
            let name = std::str::from_utf8(self.name_bytes(cur)).unwrap_or("");
            categories[category(name)] += node.size;
            if heap.len() < n {
                heap.push(std::cmp::Reverse((node.size, cur)));
            } else if heap.peek().is_some_and(|r| r.0 .0 < node.size) {
                heap.pop();
                heap.push(std::cmp::Reverse((node.size, cur)));
            }
        }
        let root = self.path(id);
        let mut largest: Vec<_> = heap.into_iter().map(|r| r.0).collect();
        largest.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let largest = largest
            .into_iter()
            .map(|(_, cid)| {
                let full = self.path(cid);
                let rel = full.strip_prefix(&root).unwrap_or(&full);
                Largest {
                    info: self.info(cid),
                    path: rel.to_string_lossy().into_owned(),
                }
            })
            .collect();
        Summary { largest, categories }
    }

    /// Mark `id` removed and shrink every ancestor. Returns the ancestors'
    /// new `(id, size, count)` so the view can update in place.
    pub fn remove(&mut self, id: u32) -> Vec<(u32, u64, u64)> {
        let n = self.nodes[id as usize];
        if n.parent == NO_PARENT || n.flags & REMOVED != 0 {
            return Vec::new();
        }
        self.nodes[id as usize].flags |= REMOVED;
        let mut updates = Vec::new();
        let mut cur = n.parent;
        while cur != NO_PARENT {
            let a = &mut self.nodes[cur as usize];
            a.size = a.size.saturating_sub(n.size);
            a.count = a.count.saturating_sub(n.count);
            updates.push((cur, a.size, a.count as u64));
            cur = a.parent;
        }
        updates
    }
}

use std::os::unix::ffi::OsStrExt;

pub const CATEGORY_COUNT: usize = 9;

/// Bucket a file name into a display category. Must stay in sync with
/// `CATEGORIES` in the webview.
pub fn category(name: &str) -> usize {
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => return 8,
    };
    match ext.as_str() {
        "mp4" | "mov" | "mkv" | "avi" | "m4v" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "prores"
        | "braw" | "r3d" | "mxf" => 0,
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "webp" | "tiff" | "tif" | "raw" | "cr2"
        | "cr3" | "nef" | "arw" | "dng" | "psd" | "svg" | "bmp" | "ico" | "icns" | "avif" | "exr" => 1,
        "mp3" | "wav" | "aac" | "flac" | "m4a" | "aiff" | "aif" | "ogg" | "opus" | "caf" | "alac"
        | "logicx" | "band" => 2,
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" | "dmg" | "iso" | "img"
        | "pkg" | "xip" | "sparseimage" | "sparsebundle" | "ipsw" | "vmdk" | "qcow2" | "vdi" => 3,
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "rs" | "go" | "py" | "rb" | "java" | "kt"
        | "swift" | "c" | "h" | "cc" | "cpp" | "hpp" | "m" | "mm" | "cs" | "php" | "html" | "css"
        | "scss" | "json" | "yaml" | "yml" | "toml" | "xml" | "md" | "sh" | "zig" | "lua" | "sql"
        | "map" | "wasm" | "lock" | "txt" | "csv" | "log" => 4,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "pages" | "numbers" | "key"
        | "rtf" | "epub" | "odt" | "sketch" | "fig" | "ai" | "indd" => 5,
        "dylib" | "so" | "a" | "o" | "framework" | "node" | "exe" | "dll" | "bin" | "app"
        | "appex" | "car" | "nib" | "metallib" | "rlib" | "dsym" => 6,
        "db" | "sqlite" | "sqlite3" | "realm" | "pack" | "idx" | "cache" | "dat" | "data" | "mlmodel"
        | "safetensors" | "gguf" | "pt" | "pth" | "ckpt" | "onnx" | "parquet" | "arrow" | "npy"
        | "h5" | "tfrecord" | "vmem" | "asset" | "bundle" => 7,
        _ => 8,
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::collections::BTreeMap;

    /// Every reachable node keyed by relative path: (size, count, is_dir).
    pub fn snapshot(t: &Tree) -> BTreeMap<String, (u64, u32, bool)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![(0u32, String::new())];
        while let Some((id, path)) = stack.pop() {
            let n = &t.nodes[id as usize];
            out.insert(path.clone(), (n.size, n.count, n.flags & DIR != 0));
            for k in t.kids(id) {
                stack.push((k, format!("{path}/{}", String::from_utf8_lossy(t.name_bytes(k)))));
            }
        }
        out
    }
}
