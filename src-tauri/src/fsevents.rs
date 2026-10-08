//! Minimal binding to macOS FSEvents: the kernel's journal of which
//! directories changed. Lets us refresh only what changed instead of
//! re-reading the whole disk, and replay changes since a saved event id.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;

type CFRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes([u8; 16]);

type Callback = extern "C" fn(FSEventStreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const FSEventStreamContext,
        paths: CFRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(alloc: CFRef, s: *const c_char, encoding: u32) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, n: isize, callbacks: *const c_void) -> CFRef;
    fn CFUUIDGetUUIDBytes(uuid: CFRef) -> CFUUIDBytes;
    fn CFRelease(r: CFRef);
}

extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(queue: DispatchQueue, ctx: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

extern "C" fn noop(_: *mut c_void) {}

const UTF8: u32 = 0x0800_0100;
const FLAG_NO_DEFER: u32 = 0x02;
const FLAG_WATCH_ROOT: u32 = 0x04;

pub const MUST_SCAN_SUBDIRS: u32 = 0x01;
pub const USER_DROPPED: u32 = 0x02;
pub const KERNEL_DROPPED: u32 = 0x04;
pub const IDS_WRAPPED: u32 = 0x08;
pub const HISTORY_DONE: u32 = 0x10;
pub const ROOT_CHANGED: u32 = 0x20;

pub struct Event {
    pub path: PathBuf,
    pub flags: u32,
    pub id: u64,
}

pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// The FSEvents database UUID for a device. It changes when the journal is
/// reset, which invalidates any saved event ids.
pub fn volume_uuid(dev: u64) -> [u8; 16] {
    unsafe {
        let uuid = FSEventsCopyUUIDForDevice(dev as libc::dev_t);
        if uuid.is_null() {
            return [0; 16];
        }
        let bytes = CFUUIDGetUUIDBytes(uuid);
        CFRelease(uuid);
        bytes.0
    }
}

type Handler = Box<dyn Fn(Vec<Event>) + Send + Sync>;

/// A running stream; stops when dropped.
pub struct Watcher {
    stream: FSEventStreamRef,
    queue: DispatchQueue,
    handler: *mut Handler,
}

unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

extern "C" fn trampoline(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    let handler = unsafe { &*(info as *const Handler) };
    let paths = paths as *const *const c_char;
    let events = (0..n)
        .map(|i| unsafe {
            let p = CStr::from_ptr(*paths.add(i));
            Event {
                path: PathBuf::from(std::ffi::OsStr::from_encoded_bytes_unchecked(p.to_bytes())),
                flags: *flags.add(i),
                id: *ids.add(i),
            }
        })
        .collect();
    handler(events);
}

impl Watcher {
    /// Watch `root`, replaying history from `since` (an event id).
    pub fn start(root: &std::path::Path, since: u64, handler: impl Fn(Vec<Event>) + Send + Sync + 'static) -> Option<Watcher> {
        let handler: *mut Handler = Box::into_raw(Box::new(Box::new(handler)));
        unsafe {
            let c = CString::new(root.as_os_str().as_encoded_bytes()).ok()?;
            let s = CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8);
            let arr = CFArrayCreate(std::ptr::null(), &s, 1, &kCFTypeArrayCallBacks as *const _ as *const c_void);
            CFRelease(s);
            let ctx = FSEventStreamContext {
                version: 0,
                info: handler as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(std::ptr::null(), trampoline, &ctx, arr, since, 1.0, FLAG_NO_DEFER | FLAG_WATCH_ROOT);
            CFRelease(arr);
            if stream.is_null() {
                drop(Box::from_raw(handler));
                return None;
            }
            let queue = dispatch_queue_create(c"space-maker.fsevents".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                drop(Box::from_raw(handler));
                return None;
            }
            Some(Watcher { stream, queue, handler })
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            // Drain the serial queue so no callback is still using the handler.
            dispatch_sync_f(self.queue, std::ptr::null_mut(), noop);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.handler));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn reports_changes_and_replays_history() {
        // FSEvents reports canonical paths (/private/var/... not /var/...).
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("space-maker-fse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let since = current_event_id();

        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let w = Watcher::start(&root, since, move |events| {
            for e in events {
                let _ = tx.lock().unwrap().send((e.path, e.flags));
            }
        })
        .expect("stream starts");
        std::fs::write(root.join("sub/hello.txt"), b"hi").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            if let Ok((path, _)) = rx.recv_timeout(Duration::from_millis(200)) {
                if path.starts_with(root.join("sub")) {
                    saw = true;
                    break;
                }
            }
        }
        drop(w);
        assert!(saw, "got an event for the changed folder");

        // Replaying from the old id reports the same change, then HistoryDone.
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let w = Watcher::start(&root, since, move |events| {
            for e in events {
                let _ = tx.lock().unwrap().send((e.path, e.flags));
            }
        })
        .unwrap();
        let (mut replayed, mut done) = (false, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !(replayed && done) {
            if let Ok((path, flags)) = rx.recv_timeout(Duration::from_millis(200)) {
                replayed |= path.starts_with(root.join("sub"));
                done |= flags & HISTORY_DONE != 0;
            }
        }
        drop(w);
        assert!(replayed && done, "history replay works (replayed={replayed} done={done})");
        assert_ne!(volume_uuid(std::fs::metadata(&root).map(|m| std::os::unix::fs::MetadataExt::dev(&m)).unwrap()), [0; 16]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
