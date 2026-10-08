mod cache;
mod fsevents;
mod live;
mod scan;
mod tree;

use live::{ChangeSet, Pending};
use scan::Progress;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tree::{Children, NodeInfo, Summary, Tree};

#[derive(Default)]
struct AppState {
    tree: RwLock<Option<Tree>>,
    progress: Mutex<Option<Arc<Progress>>>,
    session: Mutex<Option<Session>>,
    settings: Mutex<Settings>,
    /// Serializes cache writes.
    saving: Mutex<()>,
}

/// The scan currently on screen and its live connection to the disk.
struct Session {
    root: PathBuf,
    _watcher: Option<fsevents::Watcher>,
    pending: Arc<Mutex<Pending>>,
    /// FSEvents id the tree is current through.
    applied_through: u64,
    volume: [u8; 16],
    scanned_at: u64,
    elapsed_ms: u64,
    /// The tree changed since it was last saved.
    dirty: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    instant_launch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { instant_launch: true }
    }
}

fn settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/com.bobringer.spacemaker/settings.json")
}

impl Settings {
    fn load() -> Settings {
        std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn store(&self) {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serde_json::to_vec_pretty(self).unwrap_or_default());
    }
}

#[derive(Serialize, Clone)]
struct DiskChanged {
    count: usize,
    full: bool,
    history_done: bool,
}

#[derive(Serialize)]
struct CacheInfo {
    enabled: bool,
    bytes: u64,
    scans: Vec<cache::CachedScan>,
}

/// Saved event ids are only meaningful while the volume's FSEvents journal
/// is the same one.
fn volume_of(root: &Path) -> [u8; 16] {
    let probe = if root == Path::new("/") { Path::new("/System/Volumes/Data") } else { root };
    std::fs::metadata(probe).map_or([0; 16], |m| fsevents::volume_uuid(m.dev()))
}

/// Start watching `root` from event id `since` and make it the session.
fn begin_session(app: &AppHandle, root: PathBuf, since: u64, scanned_at: u64, elapsed_ms: u64) {
    let pending = Arc::new(Mutex::new(Pending { last_id: since, ..Default::default() }));
    let watcher = {
        let pending = pending.clone();
        let app = app.clone();
        let root = root.clone();
        let ignore = cache::dir();
        fsevents::Watcher::start(&root.clone(), since, move |events| {
            let mut p = pending.lock().unwrap();
            p.add(events, &root, &ignore);
            let _ = app.emit(
                "disk-changed",
                DiskChanged { count: p.len(), full: p.full, history_done: p.history_done },
            );
        })
    };
    let state = app.state::<AppState>();
    *state.session.lock().unwrap() = Some(Session {
        volume: volume_of(&root),
        root,
        _watcher: watcher,
        pending,
        applied_through: since,
        scanned_at,
        elapsed_ms,
        dirty: false,
    });
}

/// Write the current tree to the cache if instant launch is on.
fn save_cache(app: &AppHandle) {
    let state = app.state::<AppState>();
    if !state.settings.lock().unwrap().instant_launch {
        return;
    }
    let _one_at_a_time = state.saving.lock().unwrap();
    let header = {
        let mut session = state.session.lock().unwrap();
        let Some(s) = session.as_mut() else { return };
        s.dirty = false;
        cache::Header {
            root: s.root.to_string_lossy().into_owned(),
            event_id: s.applied_through,
            volume: s.volume,
            scanned_at: s.scanned_at,
            elapsed_ms: s.elapsed_ms,
            size: 0,
            files: 0,
        }
    };
    let guard = state.tree.read().unwrap();
    let Some(tree) = guard.as_ref() else { return };
    if tree.root_path.to_string_lossy() != header.root {
        return;
    }
    let header = cache::Header { size: tree.nodes[0].size, files: tree.nodes[0].count as u64, ..header };
    if let Err(e) = cache::save(tree, &header) {
        eprintln!("cache save failed: {e}");
    }
}

#[derive(Serialize, Clone)]
struct ScanProgress {
    files: u64,
    dirs: u64,
    bytes: u64,
    current: String,
}

#[derive(Serialize, Clone)]
struct ScanDone {
    root: NodeInfo,
    path: String,
    elapsed_ms: u64,
    errors: u64,
    cancelled: bool,
    /// Loaded from the cache rather than scanned.
    cached: bool,
    scanned_at: u64,
}

#[derive(Serialize)]
struct DiskInfo {
    total: u64,
    free: u64,
}

#[derive(Serialize)]
struct TrashResult {
    removed: Vec<u32>,
    failed: Vec<(u32, String)>,
    /// Ancestors whose size changed: (id, size, count).
    updates: Vec<(u32, u64, u64)>,
    freed: u64,
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}{rest}"))
        }
        None => PathBuf::from(path),
    }
}

#[tauri::command]
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

#[tauri::command]
fn disk_info(path: String) -> Result<DiskInfo, String> {
    let c = std::ffi::CString::new(expand(&path).to_string_lossy().as_bytes())
        .map_err(|e| e.to_string())?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let bs = s.f_bsize as u64;
    Ok(DiskInfo {
        total: s.f_blocks as u64 * bs,
        free: s.f_bavail as u64 * bs,
    })
}

#[tauri::command]
fn start_scan(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<(), String> {
    let root = expand(&path);
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let progress = Arc::new(Progress::default());
    if let Some(old) = state.progress.lock().unwrap().replace(progress.clone()) {
        old.cancel.store(true, Ordering::Relaxed);
    }

    // Progress ticker.
    let ticker = {
        let app = app.clone();
        let p = progress.clone();
        std::thread::spawn(move || {
            while Arc::strong_count(&p) > 2 {
                let _ = app.emit(
                    "scan-progress",
                    ScanProgress {
                        files: p.files.load(Ordering::Relaxed),
                        dirs: p.dirs.load(Ordering::Relaxed),
                        bytes: p.bytes.load(Ordering::Relaxed),
                        current: p.current.lock().unwrap().clone(),
                    },
                );
                std::thread::sleep(Duration::from_millis(80));
            }
        })
    };

    std::thread::spawn(move || {
        let started = Instant::now();
        // Anything that changes during the scan is replayed from here.
        let since = fsevents::current_event_id();
        let tree = scan::scan(&root, &progress);
        let cancelled = progress.cancel.load(Ordering::Relaxed);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let done = ScanDone {
            root: tree.info(0),
            path: root.to_string_lossy().into_owned(),
            elapsed_ms,
            errors: progress.errors.load(Ordering::Relaxed),
            cancelled,
            cached: false,
            scanned_at: cache::now(),
        };
        let state = app.state::<AppState>();
        let mut slot = state.progress.lock().unwrap();
        let current = slot.as_ref().is_some_and(|p| Arc::ptr_eq(p, &progress));
        if current {
            *slot = None;
        }
        drop(slot);
        drop(progress);
        let _ = ticker.join();
        if current {
            if !cancelled {
                *state.tree.write().unwrap() = Some(tree);
                begin_session(&app, root, since, done.scanned_at, elapsed_ms);
            }
            let _ = app.emit("scan-done", done);
            if !cancelled {
                save_cache(&app);
            }
        }
    });
    Ok(())
}

/// Open a saved scan instantly, then catch up on changes via FSEvents.
#[tauri::command]
async fn open_cached(app: AppHandle, path: String) -> Result<(), String> {
    let root = expand(&path);
    let file = cache::file_for(&root);
    tauri::async_runtime::spawn_blocking(move || {
        let header = cache::header_for(&root).ok_or("No saved scan")?;
        let age = cache::now().saturating_sub(header.scanned_at);
        if header.volume != volume_of(&root) || header.volume == [0; 16] || age > 30 * 24 * 3600 {
            let _ = std::fs::remove_file(&file);
            return Err("Saved scan is out of date".to_string());
        }
        let (header, tree) = cache::load(&file).map_err(|e| e.to_string())?;
        let done = ScanDone {
            root: tree.info(0),
            path: root.to_string_lossy().into_owned(),
            elapsed_ms: header.elapsed_ms,
            errors: 0,
            cancelled: false,
            cached: true,
            scanned_at: header.scanned_at,
        };
        let state = app.state::<AppState>();
        *state.tree.write().unwrap() = Some(tree);
        begin_session(&app, root, header.event_id, header.scanned_at, header.elapsed_ms);
        let _ = app.emit("scan-done", done);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Apply pending FSEvents changes to the tree.
#[tauri::command]
async fn apply_changes(app: AppHandle) -> Result<ChangeSet, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (pending, through) = {
            let session = state.session.lock().unwrap();
            let Some(s) = session.as_ref() else { return ChangeSet::default() };
            let mut p = s.pending.lock().unwrap();
            let taken = Pending {
                dirs: std::mem::take(&mut p.dirs),
                deep: std::mem::take(&mut p.deep),
                full: std::mem::replace(&mut p.full, false),
                last_id: p.last_id,
                history_done: p.history_done,
            };
            (taken, p.last_id)
        };
        if pending.is_empty() {
            return ChangeSet::default();
        }
        let first_catch_up = pending.history_done;
        let cs = live::apply(&state.tree, pending);
        if let Some(s) = state.session.lock().unwrap().as_mut() {
            s.applied_through = s.applied_through.max(through);
            s.dirty |= cs.folders > 0;
        }
        // Keep the cache fresh after catching up on a launch.
        if first_catch_up && cs.folders > 0 {
            let app = app.clone();
            std::thread::spawn(move || save_cache(&app));
        }
        cs
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn cache_info(state: State<'_, AppState>) -> CacheInfo {
    let enabled = state.settings.lock().unwrap().instant_launch;
    let scans = if enabled { cache::list() } else { Vec::new() };
    CacheInfo { enabled, bytes: scans.iter().map(|s| s.bytes).sum(), scans }
}

/// Turn instant launch on or off. Off deletes every saved scan.
#[tauri::command]
async fn set_instant_launch(app: AppHandle, enabled: bool) -> Result<CacheInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        {
            let mut settings = state.settings.lock().unwrap();
            settings.instant_launch = enabled;
            settings.store();
        }
        if enabled {
            save_cache(&app);
        } else {
            let _wait = state.saving.lock().unwrap();
            cache::clear().map_err(|e| e.to_string())?;
        }
        Ok(cache_info(app.state::<AppState>()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn cancel_scan(state: State<'_, AppState>) {
    if let Some(p) = state.progress.lock().unwrap().as_ref() {
        p.cancel.store(true, Ordering::Relaxed);
    }
}

#[tauri::command]
fn children(state: State<'_, AppState>, ids: Vec<u32>, limit: usize) -> Vec<Children> {
    let guard = state.tree.read().unwrap();
    let Some(tree) = guard.as_ref() else { return Vec::new() };
    ids.into_iter()
        .filter(|&id| tree.is_live(id))
        .map(|id| tree.children(id, limit))
        .collect()
}

#[tauri::command]
fn summary(state: State<'_, AppState>, id: u32, n: usize) -> Option<Summary> {
    let guard = state.tree.read().unwrap();
    guard.as_ref().filter(|t| t.is_live(id)).map(|t| t.summary(id, n))
}

#[tauri::command]
fn node_path(state: State<'_, AppState>, id: u32) -> Option<String> {
    let guard = state.tree.read().unwrap();
    guard
        .as_ref()
        .filter(|t| t.is_live(id))
        .map(|t| t.path(id).to_string_lossy().into_owned())
}

fn is_protected(path: &Path) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    path.components().count() <= 2
        || path == Path::new(&home)
        || ["/System", "/usr", "/bin", "/sbin", "/private/var", "/Library/Apple"]
            .iter()
            .any(|p| path.starts_with(p))
}

#[tauri::command]
fn trash(state: State<'_, AppState>, ids: Vec<u32>) -> Result<TrashResult, String> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut guard = state.tree.write().unwrap();
    let tree = guard.as_mut().ok_or("nothing scanned")?;
    let mut ctx = trash::TrashContext::default();
    ctx.set_delete_method(DeleteMethod::NsFileManager);

    let mut result = TrashResult { removed: vec![], failed: vec![], updates: vec![], freed: 0 };
    for id in ids {
        if id == 0 {
            result.failed.push((id, "Can't trash the scanned folder itself".into()));
            continue;
        }
        if !tree.is_live(id) {
            result.failed.push((id, "That item is no longer in the scan".into()));
            continue;
        }
        let path = tree.path(id);
        if is_protected(&path) {
            result.failed.push((id, format!("{} is protected", path.display())));
            continue;
        }
        match ctx.delete(&path) {
            Ok(()) => {
                result.freed += tree.nodes[id as usize].size;
                result.removed.push(id);
                for u in tree.remove(id) {
                    result.updates.retain(|x| x.0 != u.0);
                    result.updates.push(u);
                }
            }
            Err(e) => result.failed.push((id, e.to_string())),
        }
    }
    if !result.removed.is_empty() {
        if let Some(s) = state.session.lock().unwrap().as_mut() {
            s.dirty = true;
        }
    }
    Ok(result)
}

#[tauri::command]
fn reveal(state: State<'_, AppState>, id: u32) {
    run_with_path(&state, id, &["-R"], "open");
}

#[tauri::command]
fn open_item(state: State<'_, AppState>, id: u32) {
    run_with_path(&state, id, &[], "open");
}

#[tauri::command]
fn quick_look(state: State<'_, AppState>, id: u32) {
    run_with_path(&state, id, &["-p"], "qlmanage");
}

#[tauri::command]
fn open_trash() {
    let home = std::env::var("HOME").unwrap_or_default();
    let _ = std::process::Command::new("open").arg(format!("{home}/.Trash")).spawn();
}

fn run_with_path(state: &State<'_, AppState>, id: u32, args: &[&str], cmd: &str) {
    let path = {
        let guard = state.tree.read().unwrap();
        match guard.as_ref() {
            Some(t) if t.is_live(id) => t.path(id),
            _ => return,
        }
    };
    let _ = std::process::Command::new(cmd)
        .args(args)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = AppState { settings: Mutex::new(Settings::load()), ..Default::default() };
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            home_dir,
            disk_info,
            start_scan,
            cancel_scan,
            children,
            summary,
            node_path,
            trash,
            reveal,
            open_item,
            quick_look,
            open_trash,
            open_cached,
            apply_changes,
            cache_info,
            set_instant_launch,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Save on quit if live updates or trashing changed the tree.
            if let tauri::RunEvent::Exit = event {
                let dirty = app.state::<AppState>().session.lock().unwrap().as_ref().is_some_and(|s| s.dirty);
                if dirty {
                    save_cache(app);
                }
            }
        });
}
