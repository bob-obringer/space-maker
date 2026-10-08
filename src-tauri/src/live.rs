//! Applying FSEvents changes to an existing tree, folder by folder.
//!
//! Work happens in three phases so the tree lock is never held during disk
//! I/O: resolve changed paths to nodes (read lock), read the disk (no lock),
//! then patch the tree (write lock).

use crate::fsevents::{self, Event};
use crate::scan::{self, Progress};
use crate::tree::{Tree, DIR};
use std::os::unix::ffi::OsStrExt;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// Changes reported by FSEvents that haven't been applied yet.
#[derive(Default)]
pub struct Pending {
    /// Directories whose direct contents changed.
    pub dirs: BTreeSet<PathBuf>,
    /// Directories that must be rescanned recursively (coalesced events).
    pub deep: BTreeSet<PathBuf>,
    /// The journal can't be trusted; only a full rescan will do.
    pub full: bool,
    /// Highest event id seen.
    pub last_id: u64,
    /// The replay of history (since the saved/scan-start id) has finished.
    pub history_done: bool,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.deep.is_empty() && !self.full
    }

    pub fn len(&self) -> usize {
        self.dirs.len() + self.deep.len()
    }

    /// Fold a batch of events in. `root` is the scanned folder and `ignore`
    /// a folder whose changes are our own (the scan cache).
    pub fn add(&mut self, events: Vec<Event>, root: &Path, ignore: &Path) {
        for e in events {
            if e.flags & fsevents::HISTORY_DONE != 0 {
                self.history_done = true;
                continue;
            }
            self.last_id = self.last_id.max(e.id);
            if e.flags & (fsevents::IDS_WRAPPED | fsevents::ROOT_CHANGED) != 0 {
                self.full = true;
                continue;
            }
            let path = normalize(&e.path, root);
            if !path.starts_with(root) || path.starts_with(ignore) {
                continue;
            }
            let dropped = e.flags & (fsevents::USER_DROPPED | fsevents::KERNEL_DROPPED) != 0;
            if e.flags & fsevents::MUST_SCAN_SUBDIRS != 0 || dropped {
                if path == root {
                    self.full = true;
                } else {
                    self.deep.insert(path);
                }
            } else {
                self.dirs.insert(path);
            }
        }
    }
}

/// FSEvents reports paths on the APFS data volume under its real mount
/// point; map them back to the firmlinked paths we scanned.
fn normalize(path: &Path, root: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    if !p.starts_with(root) {
        if let Ok(rest) = p.strip_prefix("/System/Volumes/Data") {
            p = Path::new("/").join(rest);
        }
    }
    // Event paths end with a slash; PathBuf comparisons don't care.
    p
}

#[derive(Serialize, Default)]
pub struct ChangeSet {
    /// Folders whose list of children changed (refetch them).
    pub changed: Vec<u32>,
    /// Folders whose whole subtree was replaced (drop and refetch).
    pub reset: Vec<u32>,
    /// New (id, size, count) for every node whose totals changed.
    pub updates: Vec<(u32, u64, u64)>,
    /// Nothing incremental is possible; do a full scan.
    pub full_rescan: bool,
    /// How many folders were refreshed.
    pub folders: usize,
}

enum Task {
    /// Re-read one folder and diff its direct children.
    Shallow { id: u32, path: PathBuf, known: HashMap<Vec<u8>, (u32, bool, u64)> },
    /// Rescan a whole subtree.
    Deep { id: u32, path: PathBuf },
}

enum Outcome {
    Gone(u32),
    Shallow {
        id: u32,
        files: Vec<(Vec<u8>, u64)>,
        dirs: Vec<(Vec<u8>, Tree)>,
        resized: Vec<(u32, u64)>,
        removed: Vec<u32>,
    },
    Deep { id: u32, tree: Tree },
}

fn refresh_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| scan::new_pool(4))
}

pub fn apply(lock: &RwLock<Option<Tree>>, pending: Pending) -> ChangeSet {
    if pending.full {
        return ChangeSet { full_rescan: true, ..Default::default() };
    }

    // Phase 1: resolve paths to nodes.
    let tasks = {
        let guard = lock.read().unwrap();
        let Some(tree) = guard.as_ref() else { return ChangeSet::default() };
        plan(tree, pending)
    };

    // Phase 2: read the disk.
    let outcomes: Vec<Outcome> = tasks.into_iter().map(read).collect();

    // Phase 3: patch the tree.
    let mut guard = lock.write().unwrap();
    let Some(tree) = guard.as_mut() else { return ChangeSet::default() };
    let mut out = ChangeSet::default();
    let mut totals: HashMap<u32, (u64, u64)> = HashMap::new();
    for o in outcomes {
        patch(tree, o, &mut out, &mut totals);
    }
    out.updates = totals.into_iter().map(|(id, (s, c))| (id, s, c)).collect();
    out
}

fn plan(tree: &Tree, pending: Pending) -> Vec<Task> {
    let mut by_id: HashMap<u32, Task> = HashMap::new();
    let mut deep_ids: Vec<u32> = Vec::new();

    // Deep rescans first; a deep task makes any task beneath it redundant.
    for path in &pending.deep {
        let (id, exact) = tree.find(path);
        if exact && id != 0 {
            deep_ids.push(id);
            by_id.insert(id, Task::Deep { id, path: path.clone() });
        } else {
            // Not in the tree yet: re-reading the nearest known folder will
            // discover it and scan it whole.
            shallow(tree, id, &mut by_id);
        }
    }
    let mut dirs: Vec<&PathBuf> = pending.dirs.iter().collect();
    dirs.sort_by_key(|p| p.components().count());
    for path in dirs {
        let (id, _) = tree.find(path);
        shallow(tree, id, &mut by_id);
    }

    let under_deep = |id: u32| {
        let mut cur = tree.nodes[id as usize].parent;
        while cur != crate::tree::NO_PARENT {
            if deep_ids.contains(&cur) {
                return true;
            }
            cur = tree.nodes[cur as usize].parent;
        }
        false
    };
    by_id.into_iter().filter(|(id, _)| !under_deep(*id)).map(|(_, t)| t).collect()
}

fn shallow(tree: &Tree, id: u32, by_id: &mut HashMap<u32, Task>) {
    if by_id.contains_key(&id) {
        return;
    }
    let known = tree
        .kids(id)
        .map(|k| {
            let n = &tree.nodes[k as usize];
            (tree.name_bytes(k).to_vec(), (k, n.flags & DIR != 0, n.size))
        })
        .collect();
    by_id.insert(id, Task::Shallow { id, path: tree.path(id), known });
}

fn read(task: Task) -> Outcome {
    match task {
        Task::Deep { id, path } => {
            if !path.is_dir() {
                return Outcome::Gone(id);
            }
            let tree = scan::scan_with(refresh_pool(), &path, &Progress::default());
            Outcome::Deep { id, tree }
        }
        Task::Shallow { id, path, mut known } => {
            let Ok((names, entries)) = scan::read_entries(&path) else {
                return Outcome::Gone(id);
            };
            let devices = scan::devices_for(&path);
            let (mut files, mut dirs, mut resized, mut removed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for e in &entries {
                let name = &names[e.name.0..e.name.1];
                match known.remove(name) {
                    Some((kid, was_dir, size)) if was_dir == e.dir => {
                        if !e.dir && size != e.size {
                            resized.push((kid, e.size));
                        }
                        continue;
                    }
                    // Changed between file and folder: drop the old, add the new.
                    Some((kid, _, _)) => removed.push(kid),
                    None => {}
                }
                if e.dir {
                    let child = path.join(std::ffi::OsStr::from_bytes(name));
                    if scan::keep_dir(&devices, e.dev, &child) {
                        let sub = scan::scan_with(refresh_pool(), &child, &Progress::default());
                        dirs.push((name.to_vec(), sub));
                    }
                } else {
                    files.push((name.to_vec(), e.size));
                }
            }
            // Whatever we didn't see on disk is gone.
            removed.extend(known.into_values().map(|(k, _, _)| k));
            Outcome::Shallow { id, files, dirs, resized, removed }
        }
    }
}

fn patch(tree: &mut Tree, o: Outcome, out: &mut ChangeSet, totals: &mut HashMap<u32, (u64, u64)>) {
    let live = |tree: &Tree, id: u32| tree.reachable(id);
    match o {
        Outcome::Gone(id) => {
            if id == 0 || !live(tree, id) {
                return;
            }
            let parent = tree.nodes[id as usize].parent;
            for (aid, s, c) in tree.remove(id) {
                totals.insert(aid, (s, c));
            }
            out.changed.push(parent);
            out.folders += 1;
        }
        Outcome::Deep { id, tree: sub } => {
            if !live(tree, id) {
                return;
            }
            let (ds, dc) = tree.replace_subtree(id, &sub);
            tree.add_delta(id, ds, dc, totals);
            out.reset.push(id);
            out.folders += 1;
        }
        Outcome::Shallow { id, files, dirs, resized, removed } => {
            if !live(tree, id) {
                return;
            }
            for k in removed {
                if live(tree, k) {
                    for (aid, s, c) in tree.remove(k) {
                        totals.insert(aid, (s, c));
                    }
                }
            }
            for (k, size) in resized {
                if live(tree, k) {
                    let old = tree.nodes[k as usize].size;
                    tree.nodes[k as usize].size = size;
                    totals.insert(k, (size, 1));
                    tree.add_delta(id, size as i64 - old as i64, 0, totals);
                }
            }
            for (name, size) in files {
                tree.add_file(id, &name, size);
                tree.add_delta(id, size as i64, 1, totals);
            }
            for (name, sub) in dirs {
                let new = tree.graft(Some(id), &name, &sub);
                let n = tree.nodes[new as usize];
                tree.add_delta(id, n.size as i64, n.count as i64, totals);
            }
            out.changed.push(id);
            out.folders += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use crate::tree::testutil::snapshot;
    use std::fs;

    fn pending(root: &Path, dirs: &[&str], deep: &[&str]) -> Pending {
        Pending {
            dirs: dirs.iter().map(|d| root.join(d)).collect(),
            deep: deep.iter().map(|d| root.join(d)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn incremental_matches_full_rescan() {
        let root = std::env::temp_dir().join(format!("space-maker-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big/inner")).unwrap();
        fs::create_dir_all(root.join("small")).unwrap();
        fs::create_dir_all(root.join("gone/deeper")).unwrap();
        fs::write(root.join("big/movie.mov"), vec![1u8; 400_000]).unwrap();
        fs::write(root.join("big/inner/data.db"), vec![2u8; 100_000]).unwrap();
        fs::write(root.join("small/note.txt"), vec![3u8; 10]).unwrap();
        fs::write(root.join("gone/deeper/x.bin"), vec![4u8; 30_000]).unwrap();
        let lock = RwLock::new(Some(scan(&root, &Progress::default())));

        // Edit the disk: add, delete, resize, new nested folder, delete a folder.
        fs::write(root.join("small/new.png"), vec![5u8; 60_000]).unwrap();
        fs::remove_file(root.join("big/inner/data.db")).unwrap();
        fs::write(root.join("small/note.txt"), vec![3u8; 90_000]).unwrap();
        fs::create_dir_all(root.join("big/fresh/nested")).unwrap();
        fs::write(root.join("big/fresh/nested/y.mp4"), vec![6u8; 200_000]).unwrap();
        fs::remove_dir_all(root.join("gone")).unwrap();

        // Events as FSEvents would report them (parents of each change).
        let cs = apply(&lock, pending(&root, &["small", "big/inner", "big", "big/fresh", "big/fresh/nested", "", "gone", "gone/deeper"], &[]));
        assert!(!cs.full_rescan);
        assert!(!cs.updates.is_empty());
        let fresh = scan(&root, &Progress::default());
        assert_eq!(snapshot(lock.read().unwrap().as_ref().unwrap()), snapshot(&fresh));

        // A coalesced "rescan everything under big" event.
        fs::write(root.join("big/fresh/z.zip"), vec![7u8; 33_000]).unwrap();
        fs::remove_file(root.join("big/movie.mov")).unwrap();
        let cs = apply(&lock, pending(&root, &[], &["big"]));
        assert_eq!(cs.reset.len(), 1);
        let fresh = scan(&root, &Progress::default());
        assert_eq!(snapshot(lock.read().unwrap().as_ref().unwrap()), snapshot(&fresh));

        // Root-level coalesced events can't be handled incrementally.
        let cs = apply(&lock, Pending { full: true, ..Default::default() });
        assert!(cs.full_rescan);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pending_filters_and_normalizes() {
        let mut p = Pending::default();
        let root = Path::new("/Users/me");
        let ignore = Path::new("/Users/me/Library/Caches/com.bobringer.spacemaker");
        p.add(
            vec![
                Event { path: "/System/Volumes/Data/Users/me/Documents/".into(), flags: 0, id: 5 },
                Event { path: "/Users/me/Library/Caches/com.bobringer.spacemaker/scans/".into(), flags: 0, id: 6 },
                Event { path: "/private/tmp/".into(), flags: 0, id: 7 },
                Event { path: "/Users/me/Code/".into(), flags: fsevents::MUST_SCAN_SUBDIRS, id: 8 },
                Event { path: "".into(), flags: fsevents::HISTORY_DONE, id: 0 },
            ],
            root,
            ignore,
        );
        assert_eq!(p.dirs.iter().collect::<Vec<_>>(), [Path::new("/Users/me/Documents")]);
        assert_eq!(p.deep.iter().collect::<Vec<_>>(), [Path::new("/Users/me/Code")]);
        assert_eq!(p.last_id, 8);
        assert!(p.history_done && !p.full);
    }
}
