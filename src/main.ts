import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { CATEGORIES, bytes, categoryOf, count, escapeHtml, pct, swatch } from "./format";
import { TreeMap, VNode, type NodeInfo } from "./treemap";

interface ScanProgress {
  files: number;
  dirs: number;
  bytes: number;
  current: string;
}
interface ScanDone {
  root: NodeInfo;
  path: string;
  elapsed_ms: number;
  errors: number;
  cancelled: boolean;
  cached: boolean;
  scanned_at: number;
}
interface CachedScan {
  root: string;
  scanned_at: number;
  size: number;
  files: number;
  bytes: number;
}
interface CacheInfo {
  enabled: boolean;
  bytes: number;
  scans: CachedScan[];
}
interface ChangeSet {
  changed: number[];
  reset: number[];
  updates: [number, number, number][];
  full_rescan: boolean;
  folders: number;
}
interface Summary {
  largest: { info: NodeInfo; path: string }[];
  categories: number[];
}
interface TrashResult {
  removed: number[];
  failed: [number, string][];
  updates: [number, number, number][];
  freed: number;
}

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const screens = { start: $("start"), scanning: $("scanning"), explorer: $("explorer") };
const map = new TreeMap($<HTMLCanvasElement>("map"), $<HTMLCanvasElement>("minimap"));

let home = "/";
let scanPath = "";
let scanInfo: ScanDone | null = null;
let freed = 0;
let selection: VNode[] = [];

function show(name: keyof typeof screens) {
  for (const [k, el] of Object.entries(screens)) el.classList.toggle("hidden", k !== name);
  const exploring = name === "explorer";
  $("btn-rescan").classList.toggle("hidden", !exploring);
  $("btn-new").classList.toggle("hidden", !exploring);
  $("live").classList.toggle("hidden", !exploring);
  if (!exploring) $("crumbs").innerHTML = "";
  if (name === "start") refreshCacheInfo();
}

function expandHome(path: string): string {
  return path === "~" ? home : path.startsWith("~/") ? home + path.slice(1) : path;
}

function ago(secs: number): string {
  const d = Date.now() / 1000 - secs;
  if (d < 90) return "just now";
  if (d < 3600) return `${Math.round(d / 60)} min ago`;
  if (d < 86400) return `${Math.round(d / 3600)} h ago`;
  return `${Math.round(d / 86400)} d ago`;
}

function prettyRoot(path: string): string {
  if (path === "/") return "Macintosh HD";
  if (path === home) return "Home";
  return path.split("/").filter(Boolean).pop() ?? path;
}

function tildify(path: string): string {
  return path.startsWith(home + "/") ? "~" + path.slice(home.length) : path === home ? "~" : path;
}

// ------------------------------------------------------------------ start

async function initStart() {
  home = await invoke<string>("home_dir");
  $("home-path").textContent = tildify(home);
  refreshCacheInfo();
  try {
    const d = await invoke<{ total: number; free: number }>("disk_info", { path: "/" });
    const used = d.total - d.free;
    $("disk-text").textContent = `${bytes(d.free)} free of ${bytes(d.total)}`;
    $("disk-used").style.width = `${(used / d.total) * 100}%`;
    $("disk-card").classList.remove("hidden");
  } catch {
    /* no disk info is fine */
  }
}

let cacheInfo: CacheInfo = { enabled: false, bytes: 0, scans: [] };

async function refreshCacheInfo() {
  cacheInfo = await invoke<CacheInfo>("cache_info");
  const toggle = $<HTMLInputElement>("instant-toggle");
  toggle.checked = cacheInfo.enabled;
  $("instant-sub").textContent = cacheInfo.enabled
    ? cacheInfo.bytes > 0
      ? `Reopening takes a second, then catches up on what changed. Saved scans use ${bytes(cacheInfo.bytes)}.`
      : "Your next scan will be saved so reopening takes a second."
    : "Off. Every launch does a full scan, and nothing is saved to disk.";
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".target")) {
    btn.querySelector(".badge")?.remove();
    const p = btn.dataset.path!;
    const hit = p !== "pick" && cachedFor(expandHome(p));
    if (hit) {
      const badge = document.createElement("em");
      badge.className = "badge";
      badge.textContent = `⚡ Instant · ${ago(hit.scanned_at)}`;
      btn.append(badge);
    }
  }
}

function cachedFor(path: string): CachedScan | undefined {
  return cacheInfo.enabled ? cacheInfo.scans.find((c) => c.root === path) : undefined;
}

$<HTMLInputElement>("instant-toggle").addEventListener("change", async (e) => {
  const on = (e.target as HTMLInputElement).checked;
  try {
    cacheInfo = await invoke<CacheInfo>("set_instant_launch", { enabled: on });
    if (!on) toast("Instant launch off. Saved scans deleted.", "info");
  } catch (err) {
    toast(escapeHtml(String(err)), "error");
  }
  refreshCacheInfo();
});

/** Open a folder: instantly from a saved scan when we have one. */
async function openFolder(path: string, btn?: HTMLElement) {
  const full = expandHome(path);
  if (cachedFor(full)) {
    btn?.classList.add("loading");
    try {
      scanPath = full;
      await invoke("open_cached", { path: full });
      return;
    } catch {
      /* stale or unreadable: fall through to a fresh scan */
    } finally {
      btn?.classList.remove("loading");
    }
  }
  startScan(path);
}

for (const btn of document.querySelectorAll<HTMLButtonElement>(".target")) {
  btn.addEventListener("click", async () => {
    let path = btn.dataset.path!;
    if (path === "pick") {
      const picked = await open({ directory: true, multiple: false, title: "Choose a folder to scan" });
      if (typeof picked !== "string") return;
      path = picked;
    }
    openFolder(path, btn);
  });
}

// --------------------------------------------------------------- scanning

async function startScan(path: string) {
  scanPath = path;
  $("scan-root").textContent = tildify(path.replace(/^~/, home));
  for (const id of ["scan-bytes", "scan-files", "scan-dirs"]) $(id).textContent = "0";
  $("scan-path").textContent = "";
  show("scanning");
  try {
    await invoke("start_scan", { path });
  } catch (e) {
    toast(String(e), "error");
    show("start");
  }
}

let pathShownAt = 0;
let charWidth = 0;

/** Keep the end of a path, trimming from the left so it fits on one line. */
function tail(el: HTMLElement, text: string): string {
  if (!charWidth) {
    const g = document.createElement("canvas").getContext("2d")!;
    g.font = getComputedStyle(el).font;
    charWidth = g.measureText("0000000000").width / 10 || 6.6;
  }
  const max = Math.max(8, Math.floor(el.clientWidth / charWidth));
  return text.length <= max ? text : "…" + text.slice(text.length - max + 1);
}

listen<ScanProgress>("scan-progress", ({ payload: p }) => {
  $("scan-bytes").textContent = bytes(p.bytes);
  $("scan-files").textContent = count(p.files);
  $("scan-dirs").textContent = count(p.dirs);
  // Paths change constantly; a few updates a second reads as motion, not flicker.
  const now = performance.now();
  if (p.current && now - pathShownAt > 220) {
    pathShownAt = now;
    const el = $("scan-path");
    el.textContent = tail(el, tildify(p.current));
  }
});

listen<ScanDone>("scan-done", ({ payload }) => {
  if (payload.cancelled) {
    show("start");
    return;
  }
  scanInfo = payload;
  scanPath = payload.path;
  payload.root.name = prettyRoot(payload.path);
  show("explorer");
  requestAnimationFrame(() => map.setRoot(payload.root));
  live.catchingUp = payload.cached;
  setLive(payload.cached ? "Catching up…" : "Live", payload.cached);
  if (payload.cached) {
    toast(`Opened your scan from ${ago(payload.scanned_at)}. Catching up on changes…`, "info");
  } else if (payload.errors > 0) {
    toast(`${count(payload.errors)} items couldn't be read. Full Disk Access shows more.`, "info");
  }
});

// -------------------------------------------------------------- live updates

const live = { applying: false, timer: 0, last: 0, catchingUp: false, historyDone: false };

function setLive(text: string, busy: boolean) {
  $("live-text").textContent = text;
  $("live").classList.toggle("busy", busy);
}

listen<{ count: number; full: boolean; history_done: boolean }>("disk-changed", ({ payload }) => {
  if (screens.explorer.classList.contains("hidden")) return;
  live.historyDone ||= payload.history_done;
  if (payload.count > 0 || payload.full) scheduleApply(400);
  else if (live.catchingUp && live.historyDone) finishCatchUp(0);
});

function scheduleApply(ms: number) {
  clearTimeout(live.timer);
  live.timer = window.setTimeout(tryApply, ms);
}

/** Apply disk changes, but only while the user isn't in the middle of something. */
async function tryApply() {
  const busyUi = !modal.classList.contains("hidden") || !menu.classList.contains("hidden");
  if (live.applying || !map.idle || busyUi) return scheduleApply(700);
  const since = performance.now() - live.last;
  if (since < 2500 && !live.catchingUp) return scheduleApply(2500 - since);
  live.applying = true;
  setLive("Updating…", true);
  try {
    const cs = await invoke<ChangeSet>("apply_changes");
    if (cs.full_rescan) {
      toast("A lot changed on disk at once.", "info", { label: "Rescan", fn: () => startScan(scanPath) });
    } else if (cs.folders > 0) {
      await map.applyChanges(cs);
    }
    if (live.catchingUp && live.historyDone) finishCatchUp(cs.folders);
  } catch (e) {
    console.error(e);
  } finally {
    live.applying = false;
    live.last = performance.now();
    if (!live.catchingUp) setLive("Live", false);
  }
}

function finishCatchUp(folders: number) {
  live.catchingUp = false;
  setLive("Live", false);
  const what = folders === 1 ? "1 folder" : `${count(folders)} folders`;
  toast(folders > 0 ? `Caught up: ${what} changed since last time.` : "Up to date: nothing changed.", "success");
}

$("btn-cancel").addEventListener("click", () => invoke("cancel_scan"));
$("btn-rescan").addEventListener("click", () => scanPath && startScan(scanPath));
$("btn-rescan").title = "Full rescan (⌘R). The map already updates itself as files change.";
$("btn-new").addEventListener("click", () => show("start"));

// ------------------------------------------------------------ breadcrumbs

function renderCrumbs(focus: VNode) {
  const el = $("crumbs");
  el.innerHTML = "";
  const chain = focus.ancestors();
  chain.forEach((n, i) => {
    if (i > 0) {
      const sep = document.createElement("span");
      sep.className = "crumb-sep";
      sep.textContent = "›";
      el.append(sep);
    }
    const b = document.createElement("button");
    b.className = "crumb" + (i === chain.length - 1 ? " current" : "");
    b.innerHTML = `${escapeHtml(n.name)} <small>${bytes(n.size)}</small>`;
    b.addEventListener("click", () => map.flyTo(n));
    el.append(b);
  });
  el.scrollLeft = el.scrollWidth;
}

// ---------------------------------------------------------------- sidebar

let summaryTimer = 0;
let summaryFor: VNode | null = null;

function onFocus(focus: VNode) {
  renderCrumbs(focus);
  $("focus-label").textContent = focus === map.root ? "Scanned" : focus.name;
  $("focus-size").textContent = bytes(focus.size);
  const parts = [`${count(focus.count)} files`];
  if (focus !== map.root) parts.push(`${pct(focus.size, map.root.size)} of ${map.root.name}`);
  else if (scanInfo?.cached) parts.push(`saved scan · live`);
  else if (scanInfo) parts.push(`scanned in ${(scanInfo.elapsed_ms / 1000).toFixed(1)}s · live`);
  $("focus-meta").textContent = parts.join(" · ");
  $("biggest-scope").textContent = focus === map.root ? "" : `in ${focus.name}`;
  clearTimeout(summaryTimer);
  summaryTimer = window.setTimeout(() => loadSummary(focus), 140);
}

async function loadSummary(focus: VNode) {
  summaryFor = focus;
  const s = await invoke<Summary | null>("summary", { id: focus.id, n: 14 });
  if (!s || summaryFor !== focus) return;
  const total = s.categories.reduce((a, b) => a + b, 0) || 1;
  const order = s.categories.map((v, i) => [i, v]).filter(([, v]) => v > 0).sort((a, b) => b[1] - a[1]);
  $("catbar").innerHTML = order
    .map(([i, v]) => `<i style="flex:${v / total};background:${swatch(i)}" title="${CATEGORIES[i].name}: ${bytes(v)}"></i>`)
    .join("");
  $("legend").innerHTML = order
    .slice(0, 6)
    .map(([i, v]) => `<div><span class="dot" style="background:${swatch(i)}"></span>${CATEGORIES[i].name}<b>${bytes(v)}</b></div>`)
    .join("");

  const list = $("biggest-list");
  const max = s.largest[0]?.info.size || 1;
  list.innerHTML = "";
  for (const item of s.largest) {
    const li = document.createElement("li");
    const cat = categoryOf(item.info.name);
    const dir = item.path.includes("/") ? item.path.slice(0, item.path.lastIndexOf("/")) : "";
    li.innerHTML = `
      <span class="dot" style="background:${swatch(cat)}"></span>
      <div class="row-main">
        <div class="row-name">${escapeHtml(item.info.name)}</div>
        <div class="row-dir">${escapeHtml(dir || "—")}</div>
        <div class="row-bar"><i style="width:${(item.info.size / max) * 100}%;background:${swatch(cat)}"></i></div>
      </div>
      <span class="row-size">${bytes(item.info.size)}</span>
      <button class="row-trash" title="Move to Trash">
        <svg viewBox="0 0 24 24"><path d="M4 7h16M10 11v6M14 11v6M5 7l1 12a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2l1-12M9 7V4h6v3" /></svg>
      </button>`;
    li.addEventListener("click", async (e) => {
      const n = await reveal(item.info.id);
      if (!n) return;
      if ((e.target as HTMLElement).closest(".row-trash")) {
        map.select(n, false);
        confirmTrash();
        return;
      }
      map.select(n, e.shiftKey || e.metaKey);
      map.flyTo(n);
    });
    list.append(li);
  }
  if (!s.largest.length) list.innerHTML = `<li class="none">No files here.</li>`;
}

/** Make sure a node (and its ancestors) are loaded into the map. */
async function reveal(id: number): Promise<VNode | null> {
  if (map.nodes.has(id)) return map.nodes.get(id)!;
  // Walk down from the focus, loading each level until we find it.
  const path = await invoke<string | null>("node_path", { id });
  const rootPath = await invoke<string | null>("node_path", { id: map.root.id });
  if (!path || !rootPath) return null;
  const rel = path.slice(rootPath.length).split("/").filter(Boolean);
  let cur = map.root;
  for (const name of rel) {
    if (!cur.kids) {
      const [r] = await invoke<{ id: number; kids: NodeInfo[]; rest: { count: number; size: number } | null }[]>("children", {
        ids: [cur.id],
        limit: 100000,
      });
      map.loadInto(cur, r);
    }
    const next = cur.kids?.find((k) => k.name === name && !k.rest);
    if (!next) {
      // Hidden inside "smaller items": reload this level fully.
      const [r] = await invoke<{ id: number; kids: NodeInfo[]; rest: { count: number; size: number } | null }[]>("children", {
        ids: [cur.id],
        limit: 100000,
      });
      map.loadInto(cur, r, true);
      const again = cur.kids?.find((k) => k.name === name && !k.rest);
      if (!again) return null;
      cur = again;
    } else cur = next;
  }
  return cur.id === id ? cur : (map.nodes.get(id) ?? null);
}

// --------------------------------------------------------------- inspector

async function onSelect(sel: VNode[]) {
  selection = sel;
  const empty = sel.length === 0;
  $("inspector-empty").classList.toggle("hidden", !empty);
  $("inspector-body").classList.toggle("hidden", empty);
  if (empty) return;
  const total = sel.reduce((a, n) => a + n.size, 0);
  $("trash-label").textContent = sel.length > 1 ? `Move ${sel.length} items to Trash` : "Move to Trash";
  if (sel.length > 1) {
    $("sel-swatch").style.background = "linear-gradient(135deg, var(--c0), var(--c5))";
    $("sel-name").textContent = `${sel.length} items`;
    $("sel-kind").textContent = "Multiple selection";
    $("sel-size").textContent = bytes(total);
    $("sel-share").style.width = `${Math.min(100, (total / map.root.size) * 100)}%`;
    $("sel-share-text").textContent = `${pct(total, map.root.size)} of total`;
    $("sel-path").innerHTML = sel.map((n) => `<div>${escapeHtml(n.name)} <span>${bytes(n.size)}</span></div>`).join("");
    return;
  }
  const n = sel[0];
  $("sel-swatch").style.background = n.dir ? "linear-gradient(135deg, #3b4466, #232a40)" : swatch(n.cat);
  $("sel-name").textContent = n.name;
  $("sel-kind").textContent = n.dir
    ? `${n.pkg ? "Package" : "Folder"} · ${count(n.count)} files`
    : CATEGORIES[n.cat].name;
  $("sel-size").textContent = bytes(n.size);
  const parent = n.parent ?? n;
  $("sel-share").style.width = `${Math.min(100, (n.size / Math.max(1, parent.size)) * 100)}%`;
  $("sel-share-text").textContent = n.parent ? `${pct(n.size, parent.size)} of ${parent.name}` : "everything";
  $("sel-path").textContent = "";
  const path = await invoke<string | null>("node_path", { id: n.id });
  if (selection[0] === n && path) $("sel-path").textContent = tildify(path);
}

$("act-zoom").addEventListener("click", () => selection[0] && map.diveToward(selection[0], true));
$("act-look").addEventListener("click", () => selection[0] && invoke("quick_look", { id: selection[0].id }));
$("act-reveal").addEventListener("click", () => selection[0] && invoke("reveal", { id: selection[0].id }));
$("act-trash").addEventListener("click", () => confirmTrash());

// ---------------------------------------------------------------- tooltip

const tip = $("tooltip");
function onHover(n: VNode | null, x: number, y: number) {
  if (!n || n === map.root) {
    tip.classList.add("hidden");
    return;
  }
  const parent = n.parent;
  const kind = n.rest ? "Grouped" : n.dir ? `${n.pkg ? "Package" : "Folder"} · ${count(n.count)} files` : CATEGORIES[n.cat].name;
  tip.innerHTML = `
    <div class="tip-name"><span class="dot" style="background:${n.dir ? "#56607f" : swatch(n.cat)}"></span>${escapeHtml(n.name)}</div>
    <div class="tip-size">${bytes(n.size)}</div>
    <div class="tip-meta">${kind}${parent ? ` · ${pct(n.size, parent.size)} of ${escapeHtml(parent.name)}` : ""}</div>`;
  tip.classList.remove("hidden");
  const stage = tip.parentElement!.getBoundingClientRect();
  const tw = tip.offsetWidth;
  const th = tip.offsetHeight;
  let tx = x + 16;
  let ty = y + 18;
  if (tx + tw > stage.width - 8) tx = x - tw - 12;
  if (ty + th > stage.height - 8) ty = y - th - 12;
  tip.style.transform = `translate(${Math.max(8, tx)}px, ${Math.max(8, ty)}px)`;
}

// ------------------------------------------------------------ context menu

const menu = $("menu");
function onContext(n: VNode, x: number, y: number) {
  tip.classList.add("hidden");
  const multi = selection.length > 1;
  const items: ([string, string, () => void] | null)[] = [
    [n.dir ? "Zoom Into Folder" : "Zoom To File", "↵", () => map.diveToward(n, true)],
    ["Quick Look", "Space", () => invoke("quick_look", { id: n.id })],
    ["Open", "⌘O", () => invoke("open_item", { id: n.id })],
    ["Reveal in Finder", "⌘⇧R", () => invoke("reveal", { id: n.id })],
    ["Copy Path", "⌘C", () => copyPath(n)],
    null,
    [multi ? `Move ${selection.length} Items to Trash` : "Move to Trash", "⌘⌫", () => confirmTrash()],
  ];
  menu.innerHTML = "";
  for (const it of items) {
    if (!it) {
      menu.append(Object.assign(document.createElement("hr")));
      continue;
    }
    const [label, key, fn] = it;
    const b = document.createElement("button");
    if (label.includes("Trash")) b.className = "danger";
    b.innerHTML = `<span>${escapeHtml(label)}</span><kbd>${key}</kbd>`;
    b.addEventListener("click", () => {
      hideMenu();
      fn();
    });
    menu.append(b);
  }
  menu.classList.remove("hidden");
  const mw = menu.offsetWidth;
  const mh = menu.offsetHeight;
  menu.style.left = `${Math.min(x, window.innerWidth - mw - 8)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - mh - 8)}px`;
}
function hideMenu() {
  menu.classList.add("hidden");
}
window.addEventListener("pointerdown", (e) => {
  if (!menu.contains(e.target as Node)) hideMenu();
});

async function copyPath(n: VNode) {
  const path = await invoke<string | null>("node_path", { id: n.id });
  if (!path) return;
  try {
    await navigator.clipboard.writeText(path);
  } catch {
    const ta = Object.assign(document.createElement("textarea"), { value: path });
    document.body.append(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  toast("Path copied", "info");
}

// ------------------------------------------------------------------ trash

const modal = $("modal");
let pendingTrash: VNode[] = [];

function confirmTrash() {
  const items = selection.filter((n) => n !== map.root && !n.rest);
  if (!items.length) return;
  pendingTrash = items;
  const total = items.reduce((a, n) => a + n.size, 0);
  $("modal-title").textContent =
    items.length === 1 ? `Move “${items[0].name}” to the Trash?` : `Move ${items.length} items to the Trash?`;
  $("modal-sub").innerHTML = `That frees up <b>${bytes(total)}</b>. You can put things back from the Trash until you empty it.`;
  $("modal-list").innerHTML = items
    .slice(0, 8)
    .map(
      (n) =>
        `<li><span class="dot" style="background:${n.dir ? "#56607f" : swatch(n.cat)}"></span><span class="ml-name">${escapeHtml(n.name)}</span><span class="ml-size">${bytes(n.size)}</span></li>`,
    )
    .join("") + (items.length > 8 ? `<li class="more">and ${items.length - 8} more…</li>` : "");
  modal.classList.remove("hidden");
  requestAnimationFrame(() => $("modal-ok").focus());
}

function closeModal() {
  modal.classList.add("hidden");
  pendingTrash = [];
}

$("modal-cancel").addEventListener("click", closeModal);
modal.addEventListener("pointerdown", (e) => {
  if (e.target === modal) closeModal();
});
$("modal-ok").addEventListener("click", async () => {
  const items = pendingTrash;
  closeModal();
  if (!items.length) return;
  try {
    const r = await invoke<TrashResult>("trash", { ids: items.map((n) => n.id) });
    map.applyRemoval(r.removed, r.updates);
    if (r.removed.length) {
      freed += r.freed;
      $("freed").classList.remove("hidden");
      $("freed-val").textContent = bytes(freed);
      toast(
        `Moved ${r.removed.length === 1 ? "1 item" : `${r.removed.length} items`} to the Trash and freed <b>${bytes(r.freed)}</b>`,
        "success",
        { label: "Open Trash", fn: () => invoke("open_trash") },
      );
    }
    for (const [, msg] of r.failed) toast(escapeHtml(msg), "error");
  } catch (e) {
    toast(escapeHtml(String(e)), "error");
  }
});

// ------------------------------------------------------------------ toasts

function toast(html: string, kind: "success" | "error" | "info", action?: { label: string; fn: () => void }) {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `<span>${html}</span>`;
  if (action) {
    const b = document.createElement("button");
    b.textContent = action.label;
    b.addEventListener("click", action.fn);
    el.append(b);
  }
  $("toasts").append(el);
  setTimeout(() => {
    el.classList.add("out");
    setTimeout(() => el.remove(), 300);
  }, kind === "error" ? 7000 : 4500);
}

// ---------------------------------------------------------------- controls

$("zoom-in").addEventListener("click", () => map.zoomBy(2));
$("zoom-out").addEventListener("click", () => map.zoomBy(0.5));
$("zoom-up").addEventListener("click", () => map.up());
$("zoom-fit").addEventListener("click", () => map.fitAll());

function formatZoom(z: number): string {
  if (z < 10) return `${z.toFixed(1).replace(/\.0$/, "")}×`;
  if (z < 1000) return `${Math.round(z)}×`;
  if (z < 1e6) return `${(z / 1000).toFixed(z < 1e4 ? 1 : 0)}k×`;
  return `${(z / 1e6).toFixed(1)}M×`;
}

map.events = {
  focus: onFocus,
  select: onSelect,
  hover: onHover,
  context: onContext,
  camera: (z) => ($("zoom-level").textContent = formatZoom(z)),
};

window.addEventListener("keydown", (e) => {
  const exploring = !screens.explorer.classList.contains("hidden");
  if (!modal.classList.contains("hidden")) {
    if (e.key === "Escape") closeModal();
    return;
  }
  if (!menu.classList.contains("hidden") && e.key === "Escape") {
    hideMenu();
    return;
  }
  if (e.metaKey && e.key.toLowerCase() === "n") {
    e.preventDefault();
    show("start");
    return;
  }
  if (!exploring) return;
  const sel = selection[0];
  const k = e.key;
  if (e.metaKey && (k === "Backspace" || k === "Delete")) {
    e.preventDefault();
    confirmTrash();
  } else if (e.metaKey && e.shiftKey && k.toLowerCase() === "r") {
    e.preventDefault();
    if (sel) invoke("reveal", { id: sel.id });
  } else if (e.metaKey && k.toLowerCase() === "r") {
    e.preventDefault();
    startScan(scanPath);
  } else if (e.metaKey && k.toLowerCase() === "o") {
    e.preventDefault();
    if (sel) invoke("open_item", { id: sel.id });
  } else if (e.metaKey && k.toLowerCase() === "c") {
    if (sel) {
      e.preventDefault();
      copyPath(sel);
    }
  } else if (k === "Escape") {
    map.up();
  } else if (k === " ") {
    e.preventDefault();
    if (sel) invoke("quick_look", { id: sel.id });
  } else if (k === "Enter") {
    if (sel) map.diveToward(sel, true);
  } else if (k === "0" || (e.metaKey && k === "0")) {
    map.fitAll();
  } else if (k === "=" || k === "+") {
    map.zoomBy(2);
  } else if (k === "-" || k === "_") {
    map.zoomBy(0.5);
  }
});

initStart();
