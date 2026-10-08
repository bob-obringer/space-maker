// Browser preview mode: when the UI runs outside Tauri (e.g. `bun run dev` in
// a normal browser) this fakes the Rust backend with a synthetic disk so the
// interface can be developed and checked without scanning anything real.

import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { categoryOf } from "./format";

interface N {
  name: string;
  size: number;
  count: number;
  parent: number;
  dir: boolean;
  kids: number[];
}

const nodes: N[] = [];
let seed = 7;
const rand = () => ((seed = (seed * 16807) % 2147483647) - 1) / 2147483646;
const pick = <T>(xs: T[]) => xs[Math.floor(rand() * xs.length)];

function add(name: string, parent: number, dir: boolean, size = 0): number {
  const id = nodes.length;
  nodes.push({ name, size, count: dir ? 0 : 1, parent, dir, kids: [] });
  if (parent >= 0) nodes[parent].kids.push(id);
  return id;
}

const EXTS: Record<string, string[]> = {
  media: ["mov", "mp4", "mkv"],
  photo: ["heic", "jpg", "png", "dng"],
  audio: ["m4a", "mp3", "wav"],
  code: ["ts", "js", "json", "rs", "map", "md"],
  docs: ["pdf", "docx", "key", "pages"],
  bin: ["dylib", "o", "rlib", "node"],
  data: ["db", "sqlite", "cache", "pack", "safetensors"],
  archive: ["zip", "dmg", "tar", "pkg"],
};
const WORDS = "alpha nova atlas ember orbit pixel vector delta lumen quartz cobalt harbor maple cedar fjord prism solar tidal willow zephyr".split(" ");

function files(parent: number, n: number, kind: keyof typeof EXTS, avg: number) {
  for (let i = 0; i < n; i++) {
    const size = Math.round(avg * Math.pow(rand(), 2.2) * 3 + 4096);
    add(`${pick(WORDS)}-${pick(WORDS)}-${Math.floor(rand() * 999)}.${pick(EXTS[kind])}`, parent, false, size);
  }
}

function tree(parent: number, depth: number, kind: keyof typeof EXTS, avg: number, breadth = 6) {
  files(parent, Math.floor(rand() * 18) + 3, kind, avg);
  if (depth <= 0) return;
  const dirs = Math.floor(rand() * breadth) + 1;
  for (let i = 0; i < dirs; i++) {
    const d = add(`${pick(WORDS)}${i ? `-${i}` : ""}`, parent, true);
    tree(d, depth - 1, rand() < 0.15 ? pick(Object.keys(EXTS) as (keyof typeof EXTS)[]) : kind, avg * (0.5 + rand()), breadth);
  }
}

function build() {
  const GB = 1e9;
  const MB = 1e6;
  const root = add("/Users/demo", -1, true);
  const movies = add("Movies", root, true);
  files(movies, 14, "media", 4 * GB);
  tree(add("Final Cut Projects.fcpbundle", movies, true), 2, "media", 900 * MB, 4);
  const pics = add("Pictures", root, true);
  tree(add("Photos Library.photoslibrary", pics, true), 3, "photo", 6 * MB, 7);
  const lib = add("Library", root, true);
  tree(add("Caches", lib, true), 3, "data", 40 * MB, 8);
  const dev = add("Developer", lib, true);
  tree(add("DerivedData", add("Xcode", dev, true), true), 3, "bin", 25 * MB, 6);
  tree(add("CoreSimulator", dev, true), 3, "data", 60 * MB, 5);
  tree(add("Containers", lib, true), 3, "data", 12 * MB, 9);
  const code = add("code", root, true);
  for (const repo of ["space-maker", "dotfiles", "website", "playground", "experiments"]) {
    const r = add(repo, code, true);
    tree(r, 2, "code", 30_000, 5);
    tree(add("node_modules", r, true), 3, "code", 120_000, 9);
    tree(add("target", r, true), 2, "bin", 8 * MB, 4);
  }
  const dl = add("Downloads", root, true);
  files(dl, 30, "archive", 900 * MB);
  files(dl, 40, "docs", 6 * MB);
  tree(add("Music", root, true), 3, "audio", 9 * MB, 6);
  tree(add("Documents", root, true), 3, "docs", 3 * MB, 6);
  add("Empty Folder", root, true);

  // Totals, then sort children largest first (like the Rust side).
  for (let i = nodes.length - 1; i >= 0; i--) {
    const n = nodes[i];
    if (n.dir) {
      n.size = n.kids.reduce((a, k) => a + nodes[k].size, 0);
      n.count = n.kids.reduce((a, k) => a + nodes[k].count, 0);
    }
  }
  for (const n of nodes) n.kids.sort((a, b) => nodes[b].size - nodes[a].size);
}

const info = (id: number) => {
  const n = nodes[id];
  return { id, name: n.name, size: n.size, count: n.count, dir: n.dir, kids: n.kids.length };
};

const path = (id: number): string => {
  const parts: string[] = [];
  for (let c = id; c > 0; c = nodes[c].parent) parts.unshift(nodes[c].name);
  return [nodes[0].name, ...parts].join("/");
};

let instant = true;
let pendingChange = false;

/** Simulate a file appearing on disk (call `__mockChange()` in the console). */
(window as unknown as { __mockChange: () => void }).__mockChange = () => {
  pendingChange = true;
  emit("disk-changed", { count: 1, full: false, history_done: true });
};

function mockApply() {
  if (!pendingChange) return { changed: [], reset: [], updates: [], full_rescan: false, folders: 0 };
  pendingChange = false;
  // A big new download lands in ~/Downloads.
  const dl = nodes.findIndex((n) => n.name === "Downloads");
  const size = 28e9;
  const id = add(`big-new-download-${nodes.length}.dmg`, dl, false, size);
  const updates: [number, number, number][] = [[id, size, 1]];
  for (let c = dl; c >= 0; c = nodes[c].parent) {
    nodes[c].size += size;
    nodes[c].count += 1;
    if (nodes[c].parent >= 0) nodes[nodes[c].parent].kids.sort((x, y) => nodes[y].size - nodes[x].size);
    updates.push([c, nodes[c].size, nodes[c].count]);
  }
  nodes[dl].kids.sort((x, y) => nodes[y].size - nodes[x].size);
  return { changed: [dl], reset: [], updates, full_rescan: false, folders: 1 };
}

async function fakeScan() {
  const total = nodes[0].size;
  const files = nodes[0].count;
  for (let i = 1; i <= 12; i++) {
    await new Promise((r) => setTimeout(r, 90));
    await emit("scan-progress", {
      files: Math.round((files * i) / 12),
      dirs: Math.round((nodes.length * i) / 40),
      bytes: Math.round((total * i) / 12),
      current: path(Math.floor(rand() * nodes.length)),
    });
  }
  await emit("scan-done", { root: info(0), path: nodes[0].name, elapsed_ms: 1234, errors: 3, cancelled: false, cached: false, scanned_at: Date.now() / 1000 });
}

build();

mockIPC(
  (cmd, args) => {
    const a = args as Record<string, unknown>;
    switch (cmd) {
      case "home_dir":
        return "/Users/demo";
      case "disk_info":
        return { total: 994_662_584_320, free: 211_000_000_000 };
      case "start_scan":
        fakeScan();
        return null;
      case "cache_info":
        return instant
          ? { enabled: true, bytes: 110_000_000, scans: [{ root: "/Users/demo", scanned_at: Date.now() / 1000 - 7200, size: nodes[0].size, files: nodes[0].count, bytes: 110_000_000 }] }
          : { enabled: false, bytes: 0, scans: [] };
      case "set_instant_launch":
        instant = a.enabled as boolean;
        return instant ? { enabled: true, bytes: 110_000_000, scans: [] } : { enabled: false, bytes: 0, scans: [] };
      case "open_cached":
        setTimeout(async () => {
          await emit("scan-done", { root: info(0), path: nodes[0].name, elapsed_ms: 1234, errors: 0, cancelled: false, cached: true, scanned_at: Date.now() / 1000 - 7200 });
          setTimeout(() => (window as unknown as { __mockChange: () => void }).__mockChange(), 900);
        }, 150);
        return null;
      case "apply_changes":
        return mockApply();
      case "children":
        return (a.ids as number[]).map((id) => {
          const n = nodes[id];
          const limit = a.limit as number;
          const shown = n.kids.slice(0, limit);
          const cut = n.kids.slice(limit);
          return {
            id,
            kids: shown.map(info),
            rest: cut.length ? { count: cut.length, size: cut.reduce((s, k) => s + nodes[k].size, 0) } : null,
          };
        });
      case "summary": {
        const files: number[] = [];
        const categories = new Array(9).fill(0);
        const stack = [a.id as number];
        while (stack.length) {
          const n = nodes[stack.pop()!];
          if (n.dir) stack.push(...n.kids);
          else categories[categoryOf(n.name)] += n.size;
        }
        const walk = [a.id as number];
        while (walk.length) {
          const id = walk.pop()!;
          if (nodes[id].dir) walk.push(...nodes[id].kids);
          else files.push(id);
        }
        files.sort((x, y) => nodes[y].size - nodes[x].size);
        const base = path(a.id as number);
        return {
          categories,
          largest: files.slice(0, a.n as number).map((id) => ({ info: info(id), path: path(id).slice(base.length + 1) })),
        };
      }
      case "node_path":
        return path(a.id as number);
      case "trash": {
        const removed: number[] = [];
        const updates = new Map<number, [number, number, number]>();
        let freed = 0;
        for (const id of a.ids as number[]) {
          const n = nodes[id];
          if (!n || n.parent < 0) continue;
          const p = nodes[n.parent];
          p.kids = p.kids.filter((k) => k !== id);
          for (let c = n.parent; c >= 0; c = nodes[c].parent) {
            nodes[c].size -= n.size;
            nodes[c].count -= n.count;
            updates.set(c, [c, nodes[c].size, nodes[c].count]);
            if (nodes[c].parent >= 0) nodes[nodes[c].parent].kids.sort((x, y) => nodes[y].size - nodes[x].size);
          }
          n.parent = -1;
          freed += n.size;
          removed.push(id);
        }
        return { removed, failed: [], updates: [...updates.values()], freed };
      }
      default:
        console.info("[mock]", cmd, a);
        return null;
    }
  },
  { shouldMockEvents: true },
);

console.info(`[mock] synthetic disk with ${nodes.length.toLocaleString()} nodes`);
