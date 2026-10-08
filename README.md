# Space Maker

A zoomable disk-space map for macOS, in the spirit of GrandPerspective. Built
with [Tauri 2](https://tauri.app): a Rust scanner and the system WebKit view,
so the app is about 4 MB.

## Using it

| Action | How |
| --- | --- |
| Zoom | Pinch, or ⌘ + scroll, or a mouse wheel |
| Pan | Two-finger scroll, or drag |
| Dive one level into a folder | Double-click (⌥ double-click goes straight to the deepest folder) |
| Back out one level | Esc |
| Show everything | 0 |
| Jump anywhere | Click or drag on the minimap, or click a breadcrumb |
| Select | Click; ⇧/⌘-click to add more |
| Quick Look | Space |
| Reveal in Finder | ⌘⇧R |
| Move to Trash | ⌘⌫, the sidebar button, or right-click |

Items always go to the Trash, never straight to deletion. Grant Full Disk Access
in System Settings → Privacy & Security to see protected folders.

### Live map and instant launch

After a scan, the map keeps itself current. Space Maker watches the macOS
FSEvents journal and re-reads only the folders that changed. Updates are applied
when you're not in the middle of zooming or dragging, and new items animate in.

**Instant launch** (a switch on the start screen, on by default) saves a
compressed copy of each scan to `~/Library/Caches/com.bobringer.spacemaker`.
That's about 110 MB for a home folder of roughly 9M files. Reopening a folder
loads in about half a second, then catches up on everything that changed since.
Turning the switch off deletes the saved scans immediately.

## Building it

There's no signed download yet, so build it yourself. You need macOS,
[Rust](https://rustup.rs), [Bun](https://bun.sh) and the Xcode command line
tools.

```bash
git clone https://github.com/bob-obringer/space-maker
cd space-maker
bun install
bun run tauri build --bundles app
open "src-tauri/target/release/bundle/macos/Space Maker.app"
```

## Development

```bash
bun install
bun run tauri dev               # the real app
bun run dev                     # UI only, in a browser, against a fake disk (src/mock.ts)
cd src-tauri && cargo test      # scanner tests
cargo test --release bench_home -- --ignored --nocapture   # read-only scan of ~
bun run tauri build --bundles app
```

## How it works

- **Scanner** (`src-tauri/src/scan.rs`): parallel walk with rayon. It reads
  directories with `getattrlistbulk`, which returns a batch of entries per
  syscall, and falls back to `read_dir` when that isn't supported. Each
  worker appends 40-byte nodes to its own block-allocated arena, and the
  arenas are stitched together without copying. Hard links are counted once,
  symlinks are not followed, and the scan stays on the scanned volume (plus
  the APFS data volume, so `/` works).
- **Tree** (`src-tauri/src/tree.rs`): ids are stable for the whole session.
  The UI asks for children lazily, and removal only marks a node and shrinks
  its ancestors.
- **Live updates** (`src-tauri/src/fsevents.rs`, `live.rs`): an FSEvents
  stream starts at the event id from the start of the scan (or from the saved
  scan's id). Changed folders are diffed against the tree, and new subfolders
  are scanned and grafted in. Ids stay stable, so the map never jumps. Coalesced
  events trigger a subtree rescan, and a lost journal triggers a full rescan.
- **Cache** (`src-tauri/src/cache.rs`): a breadth-first zstd stream, compacted
  while it's written. It's valid only while the volume's FSEvents journal UUID
  matches and the scan is under 30 days old.
- **Map** (`src/treemap.ts`): a squarified treemap with a fixed world layout
  and a camera. Folders load their children when they get big enough on
  screen, and jumps use van Wijk–Nuij smooth zoom.

## License

[MIT](LICENSE)
