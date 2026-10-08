// Zoomable, lazily-loaded squarified treemap rendered on a canvas.
//
// Every node has a fixed rectangle in "world" space (the root is 1000 units
// wide). The camera maps world to screen, so zooming is just changing the
// camera — like a map. Directory contents are fetched from Rust on demand as
// soon as a directory is big enough on screen to be worth subdividing.

import { invoke } from "@tauri-apps/api/core";
import { bytes, categoryOf, color, isPackage, shadeOf } from "./format";

export interface NodeInfo {
  id: number;
  name: string;
  size: number;
  count: number;
  dir: boolean;
  kids: number;
}

interface ChildrenResp {
  id: number;
  kids: NodeInfo[];
  rest: { count: number; size: number } | null;
}

export class VNode {
  kids: VNode[] | null = null;
  loading = false;
  rest = false;
  cat: number;
  shade: number;
  pkg: boolean;
  /** Arrived via a live update; grows in from its center. */
  born = false;
  x = 0;
  y = 0;
  w = 0;
  h = 0;
  // Rectangle at the start of a layout tween; fw < 0 means "no tween".
  fx = 0;
  fy = 0;
  fw = -1;
  fh = 0;

  constructor(
    public id: number,
    public name: string,
    public size: number,
    public count: number,
    public dir: boolean,
    public kidCount: number,
    public parent: VNode | null,
  ) {
    this.cat = dir ? 8 : categoryOf(name);
    this.shade = shadeOf(name);
    this.pkg = dir && isPackage(name);
    if (dir && kidCount === 0) this.kids = [];
  }

  get depth(): number {
    let d = 0;
    for (let p = this.parent; p; p = p.parent) d++;
    return d;
  }

  ancestors(): VNode[] {
    const out: VNode[] = [];
    for (let p: VNode | null = this; p; p = p.parent) out.unshift(p);
    return out;
  }

  isAncestorOf(n: VNode): boolean {
    for (let p = n.parent; p; p = p.parent) if (p === this) return true;
    return false;
  }
}

interface View {
  x: number;
  y: number;
  k: number;
  W: number;
  H: number;
}

type ZoomView = [number, number, number]; // center x, center y, visible world width

const WORLD_W = 1000;
const HEADER = 0.045;
const PAD = 0.006;
const KID_LIMIT = 600;
const LOAD_AREA = 30; // px² before a folder gets subdivided
const MAX_ZOOM = 1e9;

const DIR_BG = Array.from({ length: 12 }, (_, d) => `hsl(226 20% ${7 + Math.min(d, 9) * 2.1}%)`);
const DIR_HEAD = Array.from({ length: 12 }, (_, d) => `hsl(226 22% ${11 + Math.min(d, 9) * 2.3}%)`);
const PKG_HEAD = Array.from({ length: 12 }, (_, d) => `hsl(234 34% ${16 + Math.min(d, 9) * 2.3}%)`);
const UNLOADED = "hsl(226 16% 30%)";

const easeInOut = (t: number) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);

/** Van Wijk & Nuij smooth zoom/pan interpolation (the "fly to" curve). */
function zoomInterp(a: ZoomView, b: ZoomView) {
  const rho = Math.SQRT2;
  const [ux0, uy0, w0] = a;
  const [ux1, uy1, w1] = b;
  const dx = ux1 - ux0;
  const dy = uy1 - uy0;
  const d2 = dx * dx + dy * dy;
  let S: number;
  let fn: (t: number) => ZoomView;
  if (d2 < 1e-12 * w0 * w0) {
    S = Math.log(w1 / w0) / rho;
    fn = (t) => [ux0 + t * dx, uy0 + t * dy, w0 * Math.exp(rho * t * S)];
  } else {
    const d1 = Math.sqrt(d2);
    const b0 = (w1 * w1 - w0 * w0 + rho ** 4 * d2) / (2 * w0 * rho ** 2 * d1);
    const b1 = (w1 * w1 - w0 * w0 - rho ** 4 * d2) / (2 * w1 * rho ** 2 * d1);
    const r0 = Math.log(Math.sqrt(b0 * b0 + 1) - b0);
    const r1 = Math.log(Math.sqrt(b1 * b1 + 1) - b1);
    S = (r1 - r0) / rho;
    fn = (t) => {
      const s = t * S;
      const c0 = Math.cosh(r0);
      const u = (w0 / (rho ** 2 * d1)) * (c0 * Math.tanh(rho * s + r0) - Math.sinh(r0));
      return [ux0 + u * dx, uy0 + u * dy, (w0 * c0) / Math.cosh(rho * s + r0)];
    };
  }
  const duration = Math.min(1400, Math.max(320, Math.abs(S) * 520));
  return { fn, duration };
}

function squarify(items: VNode[], x: number, y: number, w: number, h: number) {
  let total = 0;
  for (const it of items) total += it.size;
  const n = items.length;
  if (total <= 0 || w <= 0 || h <= 0) {
    for (const it of items) Object.assign(it, { x, y, w: 0, h: 0 });
    return;
  }
  const scale = (w * h) / total;
  let i = 0;
  while (i < n) {
    if (items[i].size <= 0) {
      for (; i < n; i++) Object.assign(items[i], { x, y, w: 0, h: 0 });
      break;
    }
    const short = Math.min(w, h);
    const sh2 = short * short;
    let rowArea = 0;
    let minA = Infinity;
    let maxA = 0;
    let worst = Infinity;
    let j = i;
    while (j < n && items[j].size > 0) {
      const a = items[j].size * scale;
      const nRow = rowArea + a;
      const nMin = Math.min(minA, a);
      const nMax = Math.max(maxA, a);
      const s2 = nRow * nRow;
      const nWorst = Math.max((sh2 * nMax) / s2, s2 / (sh2 * nMin));
      if (j > i && nWorst > worst) break;
      rowArea = nRow;
      minA = nMin;
      maxA = nMax;
      worst = nWorst;
      j++;
    }
    if (w >= h) {
      const cw = Math.min(w, rowArea / h);
      let yy = y;
      for (let k = i; k < j; k++) {
        const ih = (items[k].size * scale) / cw;
        Object.assign(items[k], { x, y: yy, w: cw, h: ih });
        yy += ih;
      }
      x += cw;
      w = Math.max(0, w - cw);
    } else {
      const rh = Math.min(h, rowArea / w);
      let xx = x;
      for (let k = i; k < j; k++) {
        const iw = (items[k].size * scale) / rh;
        Object.assign(items[k], { x: xx, y, w: iw, h: rh });
        xx += iw;
      }
      y += rh;
      h = Math.max(0, h - rh);
    }
    i = j;
  }
}

function makeCushion(): HTMLCanvasElement {
  const s = 128;
  const c = document.createElement("canvas");
  c.width = c.height = s;
  const g = c.getContext("2d")!;
  const hi = g.createRadialGradient(s * 0.32, s * 0.28, 0, s * 0.32, s * 0.28, s * 0.85);
  hi.addColorStop(0, "rgba(255,255,255,0.30)");
  hi.addColorStop(0.45, "rgba(255,255,255,0.06)");
  hi.addColorStop(1, "rgba(255,255,255,0)");
  g.fillStyle = hi;
  g.fillRect(0, 0, s, s);
  const lo = g.createLinearGradient(0, 0, s, s);
  lo.addColorStop(0.45, "rgba(0,0,0,0)");
  lo.addColorStop(1, "rgba(0,0,0,0.38)");
  g.fillStyle = lo;
  g.fillRect(0, 0, s, s);
  return c;
}

function makeHatch(ctx: CanvasRenderingContext2D): CanvasPattern {
  const c = document.createElement("canvas");
  c.width = c.height = 8;
  const g = c.getContext("2d")!;
  g.fillStyle = "hsl(222 12% 24%)";
  g.fillRect(0, 0, 8, 8);
  g.strokeStyle = "hsl(222 12% 32%)";
  g.lineWidth = 2;
  g.beginPath();
  g.moveTo(-2, 10);
  g.lineTo(10, -2);
  g.stroke();
  return ctx.createPattern(c, "repeat")!;
}

export interface TreeMapEvents {
  focus?: (n: VNode) => void;
  select?: (sel: VNode[]) => void;
  hover?: (n: VNode | null, x: number, y: number) => void;
  context?: (n: VNode, x: number, y: number) => void;
  layout?: () => void;
  camera?: (zoom: number) => void;
}

export class TreeMap {
  readonly ctx: CanvasRenderingContext2D;
  root!: VNode;
  nodes = new Map<number, VNode>();
  selection = new Set<VNode>();
  hover: VNode | null = null;
  focus!: VNode;
  events: TreeMapEvents = {};

  private W = 1;
  private H = 1;
  private dpr = 1;
  private cam = { x: 0, y: 0, k: 1 };
  private worldH = 600;
  private need: VNode[] = [];
  private inflight = 0;
  private raf = 0;
  private fly: { start: number; dur: number; fn: (t: number) => ZoomView } | null = null;
  private tween: { start: number; dur: number } | null = null;
  private t = 1;
  private cushion = makeCushion();
  private hatch: CanvasPattern;
  private drag: { x: number; y: number; moved: boolean; id: number } | null = null;
  private gesture: { scale: number } | null = null;
  private mouse: [number, number] | null = null;
  private lastInput = 0;
  private mini: { canvas: HTMLCanvasElement; image: HTMLCanvasElement; k: number; timer: number };

  constructor(
    private canvas: HTMLCanvasElement,
    miniCanvas: HTMLCanvasElement,
  ) {
    this.ctx = canvas.getContext("2d")!;
    this.hatch = makeHatch(this.ctx);
    this.mini = { canvas: miniCanvas, image: document.createElement("canvas"), k: 1, timer: 0 };
    this.bindInput();
    this.bindMinimap();
    new ResizeObserver(() => this.resize()).observe(canvas);
  }

  // ---------------------------------------------------------------- data

  setRoot(info: NodeInfo) {
    this.nodes.clear();
    this.selection.clear();
    this.hover = null;
    this.root = this.make(info, null);
    this.focus = this.root;
    this.resize(true);
    this.events.focus?.(this.root);
    this.events.select?.([]);
  }

  private make(info: NodeInfo, parent: VNode | null): VNode {
    const n = new VNode(info.id, info.name, info.size, info.count, info.dir, info.kids, parent);
    this.nodes.set(n.id, n);
    return n;
  }

  private attach(n: VNode, r: ChildrenResp) {
    const kids = r.kids.map((k) => this.make(k, n));
    if (r.rest && r.rest.size > 0) {
      const rest = new VNode(-1 - n.id, `${r.rest.count.toLocaleString()} smaller items`, r.rest.size, 0, false, 0, n);
      rest.rest = true;
      kids.push(rest);
    }
    n.kids = kids;
  }

  private flushLoads() {
    if (!this.need.length || this.inflight >= 2) return;
    const batch = this.need.sort((a, b) => b.w * b.h - a.w * a.h).slice(0, 250);
    for (const n of batch) n.loading = true;
    this.inflight++;
    invoke<ChildrenResp[]>("children", { ids: batch.map((n) => n.id), limit: KID_LIMIT })
      .then((resps) => {
        for (const r of resps) {
          const n = this.nodes.get(r.id);
          if (!n || n.kids) continue;
          this.attach(n, r);
          this.layout(n);
        }
      })
      .finally(() => {
        for (const n of batch) n.loading = false;
        this.inflight--;
        this.invalidate();
        this.scheduleMinimap();
      });
  }

  /** Load (or with `replace`, reload) a node's children from a response. */
  loadInto(n: VNode, r: ChildrenResp, replace = false) {
    if (n.kids && !replace) return;
    if (n.kids) for (const k of n.kids) if (!k.rest) this.forget(k);
    this.attach(n, r);
    this.layout(n);
    this.invalidate();
    this.scheduleMinimap();
  }

  /**
   * Merge a live update from the disk watcher: refresh sizes, refetch the
   * children of folders that changed, keep everything else (and the user's
   * place) exactly where it is.
   */
  async applyChanges(cs: { changed: number[]; reset: number[]; updates: [number, number, number][] }) {
    for (const [id, size, count] of cs.updates) {
      const n = this.nodes.get(id);
      if (n) Object.assign(n, { size, count });
    }
    const reset = new Set(cs.reset);
    const ids = [...new Set([...cs.changed, ...cs.reset])].filter((id) => this.nodes.get(id)?.kids);
    if (ids.length) {
      const resps = await invoke<ChildrenResp[]>("children", { ids, limit: KID_LIMIT });
      for (const r of resps) {
        const n = this.nodes.get(r.id);
        if (n?.kids) this.merge(n, r, reset.has(r.id));
      }
    }
    // Sizes moved, so sibling order may have too.
    const sortKids = (n: VNode) => {
      if (!n.kids) return;
      n.kids.sort((a, b) => (a.rest ? 1 : b.rest ? -1 : b.size - a.size));
      n.kids.forEach(sortKids);
    };
    sortKids(this.root);
    // Keep focus/selection pointing at things that still exist.
    while (this.focus !== this.root && this.nodes.get(this.focus.id) !== this.focus) this.focus = this.focus.parent ?? this.root;
    for (const s of [...this.selection]) if (this.nodes.get(s.id) !== s) this.selection.delete(s);
    if (this.hover && !this.hover.rest && this.nodes.get(this.hover.id) !== this.hover) this.hover = null;
    this.relayoutAnimated();
    this.events.select?.([...this.selection]);
    this.events.focus?.(this.focus);
  }

  private merge(n: VNode, r: ChildrenResp, reset: boolean) {
    const old = new Map((n.kids ?? []).filter((k) => !k.rest).map((k) => [k.id, k]));
    const kids = r.kids.map((info) => {
      const keep = reset ? undefined : old.get(info.id);
      if (keep) {
        old.delete(info.id);
        Object.assign(keep, { size: info.size, count: info.count, kidCount: info.kids });
        if (keep.dir && info.kids === 0) keep.kids = [];
        else if (keep.dir && keep.kids?.length === 0) keep.kids = null; // was empty, now isn't
        return keep;
      }
      const fresh = this.make(info, n);
      fresh.born = true;
      return fresh;
    });
    for (const gone of old.values()) this.forget(gone);
    if (r.rest && r.rest.size > 0) {
      const rest = new VNode(-1 - n.id, `${r.rest.count.toLocaleString()} smaller items`, r.rest.size, 0, false, 0, n);
      rest.rest = true;
      kids.push(rest);
    }
    n.kids = kids;
    n.kidCount = r.kids.length + (r.rest?.count ?? 0);
  }

  /** Apply the result of moving items to the Trash. */
  applyRemoval(removed: number[], updates: [number, number, number][]) {
    const focusBefore = this.focus;
    for (const id of removed) {
      const n = this.nodes.get(id);
      if (!n?.parent) continue;
      const p = n.parent;
      if (p.kids) p.kids = p.kids.filter((k) => k !== n);
      p.kidCount = Math.max(0, p.kidCount - 1);
      this.selection.delete(n);
      for (const s of [...this.selection]) if (n.isAncestorOf(s)) this.selection.delete(s);
      if (this.hover && (this.hover === n || n.isAncestorOf(this.hover))) this.hover = null;
      if (this.focus === n || n.isAncestorOf(this.focus)) this.focus = p;
      this.forget(n);
    }
    for (const [id, size, count] of updates) {
      const n = this.nodes.get(id);
      if (!n) continue;
      n.size = size;
      n.count = count;
    }
    for (const [id] of updates) {
      const p = this.nodes.get(id)?.parent;
      p?.kids?.sort((a, b) => (a.rest ? 1 : b.rest ? -1 : b.size - a.size));
    }
    this.relayoutAnimated();
    if (this.focus !== focusBefore) this.flyTo(this.focus);
    this.events.select?.([...this.selection]);
    this.events.focus?.(this.focus);
  }

  private forget(n: VNode) {
    this.nodes.delete(n.id);
    if (n.kids) for (const k of n.kids) this.forget(k);
  }

  // -------------------------------------------------------------- layout

  private content(n: VNode) {
    const s = Math.min(n.w, n.h);
    const pad = n.parent ? s * PAD : 0;
    const head = n.parent ? s * HEADER : 0;
    return {
      x: n.x + pad,
      y: n.y + pad + head,
      w: Math.max(0, n.w - 2 * pad),
      h: Math.max(0, n.h - 2 * pad - head),
    };
  }

  private layout(n: VNode) {
    if (!n.kids) return;
    const c = this.content(n);
    squarify(n.kids, c.x, c.y, c.w, c.h);
    for (const k of n.kids) if (k.kids) this.layout(k);
  }

  private relayoutAnimated() {
    const snap = (n: VNode) => {
      n.fx = n.x;
      n.fy = n.y;
      n.fw = n.w;
      n.fh = n.h;
      n.kids?.forEach(snap);
    };
    snap(this.root);
    this.layout(this.root);
    const grow = (n: VNode) => {
      if (n.born) {
        n.born = false;
        n.fx = n.x + n.w / 2;
        n.fy = n.y + n.h / 2;
        n.fw = n.fh = 0;
      }
      n.kids?.forEach(grow);
    };
    grow(this.root);
    this.tween = { start: performance.now(), dur: 480 };
    this.invalidate();
    this.scheduleMinimap();
  }

  private resize(reset = false) {
    const rect = this.canvas.getBoundingClientRect();
    if (rect.width < 2 || rect.height < 2) return;
    this.dpr = window.devicePixelRatio || 1;
    this.W = rect.width;
    this.H = rect.height;
    this.canvas.width = Math.round(rect.width * this.dpr);
    this.canvas.height = Math.round(rect.height * this.dpr);
    if (!this.root) return;
    this.worldH = (WORLD_W * this.H) / this.W;
    Object.assign(this.root, { x: 0, y: 0, w: WORLD_W, h: this.worldH, fw: -1 });
    this.layout(this.root);
    this.fitInstant(reset ? this.root : this.focus);
    this.resizeMinimap();
  }

  // -------------------------------------------------------------- camera

  private get fitK() {
    return this.W / WORLD_W;
  }

  get zoom() {
    return this.cam.k / this.fitK;
  }

  private viewFor(n: VNode, margin = 0.05): ZoomView {
    const k = Math.min((this.W * (1 - 2 * margin)) / Math.max(n.w, 1e-12), (this.H * (1 - 2 * margin)) / Math.max(n.h, 1e-12));
    const kk = Math.min(k, this.fitK * MAX_ZOOM);
    return [n.x + n.w / 2, n.y + n.h / 2, this.W / kk];
  }

  private currentView(): ZoomView {
    const { x, y, k } = this.cam;
    return [x + this.W / (2 * k), y + this.H / (2 * k), this.W / k];
  }

  private applyView([ux, uy, w]: ZoomView) {
    const k = this.W / w;
    this.cam.k = k;
    this.cam.x = ux - this.W / (2 * k);
    this.cam.y = uy - this.H / (2 * k);
  }

  private fitInstant(n: VNode) {
    this.fly = null;
    this.applyView(n === this.root ? [WORLD_W / 2, this.worldH / 2, WORLD_W] : this.viewFor(n));
    this.afterCamera();
  }

  private flyToView(target: ZoomView) {
    const { fn, duration } = zoomInterp(this.currentView(), target);
    this.fly = { start: performance.now(), dur: duration, fn };
    this.invalidate();
  }

  /** Animate the camera so `n` fills the view. Directories become the focus. */
  flyTo(n: VNode) {
    if (n === this.root) this.flyToView([WORLD_W / 2, this.worldH / 2, WORLD_W]);
    else this.flyToView(this.viewFor(n, n.dir ? 0.035 : 0.12));
    const f = n.dir && !n.rest ? n : n.parent;
    if (f && f !== this.focus) {
      this.focus = f;
      this.events.focus?.(f);
    }
  }

  up() {
    const target = this.focus.parent ?? this.root;
    this.flyTo(target);
  }

  fitAll() {
    this.flyTo(this.root);
  }

  zoomBy(f: number) {
    const [ux, uy, w] = this.currentView();
    this.flyToView([ux, uy, w / f]);
  }

  /** True when the user isn't mid-gesture and nothing is animating. */
  get idle(): boolean {
    return !this.fly && !this.tween && !this.drag?.moved && !this.gesture && performance.now() - this.lastInput > 1200;
  }

  private zoomAt(px: number, py: number, f: number) {
    const { cam } = this;
    const nk = Math.min(this.fitK * MAX_ZOOM, Math.max(this.fitK * 0.5, cam.k * f));
    const wx = cam.x + px / cam.k;
    const wy = cam.y + py / cam.k;
    cam.k = nk;
    cam.x = wx - px / nk;
    cam.y = wy - py / nk;
    this.afterCamera(true);
  }

  private pan(dx: number, dy: number) {
    this.cam.x += dx / this.cam.k;
    this.cam.y += dy / this.cam.k;
    this.afterCamera(true);
  }

  private afterCamera(manual = false) {
    const { cam } = this;
    if (manual) this.lastInput = performance.now();
    // Keep the view center over the map.
    const cx = Math.min(WORLD_W, Math.max(0, cam.x + this.W / (2 * cam.k)));
    const cy = Math.min(this.worldH, Math.max(0, cam.y + this.H / (2 * cam.k)));
    cam.x = cx - this.W / (2 * cam.k);
    cam.y = cy - this.H / (2 * cam.k);
    if (manual) {
      this.fly = null;
      this.updateFocus();
    }
    this.events.camera?.(this.zoom);
    // Whatever is under the cursor changes as the map moves beneath it.
    if (this.mouse && !this.drag?.moved) {
      if (this.fly) this.setHover(null, 0, 0);
      else this.setHover(this.hit(...this.mouse), ...this.mouse);
    }
    this.invalidate();
  }

  /** After free-form zoom/pan, move the focus up or down the tree. */
  private updateFocus() {
    const { W, H } = this;
    const scr = (n: VNode) => {
      const { x, y, k } = this.cam;
      return [(n.x - x) * k, (n.y - y) * k, n.w * k, n.h * k];
    };
    let f = this.focus;
    for (;;) {
      if (!f.parent) break;
      const [sx, sy, sw, sh] = scr(f);
      const containsCenter = sx <= W / 2 && sx + sw >= W / 2 && sy <= H / 2 && sy + sh >= H / 2;
      if (containsCenter && sw * sh >= 0.2 * W * H) break;
      f = f.parent;
    }
    for (;;) {
      const next = f.kids?.find((k) => {
        if (!k.dir || k.rest) return false;
        const [sx, sy, sw, sh] = scr(k);
        return sx <= W * 0.04 && sy <= H * 0.04 && sx + sw >= W * 0.96 && sy + sh >= H * 0.96;
      });
      if (!next) break;
      f = next;
    }
    if (f !== this.focus) {
      this.focus = f;
      this.events.focus?.(f);
    }
  }

  // --------------------------------------------------------------- input

  private local(e: { clientX: number; clientY: number }) {
    const r = this.canvas.getBoundingClientRect();
    return [e.clientX - r.left, e.clientY - r.top];
  }

  hit(px: number, py: number): VNode | null {
    if (!this.root) return null;
    const wx = this.cam.x + px / this.cam.k;
    const wy = this.cam.y + py / this.cam.k;
    const inside = (n: VNode) => wx >= n.x && wx < n.x + n.w && wy >= n.y && wy < n.y + n.h;
    if (!inside(this.root)) return null;
    let n = this.root;
    for (;;) {
      const next = n.kids?.find((k) => k.w > 0 && inside(k));
      if (!next) return n;
      n = next;
    }
  }

  private bindInput() {
    const c = this.canvas;

    c.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        if (!this.root) return;
        const [px, py] = this.local(e);
        let { deltaX: dx, deltaY: dy } = e;
        if (e.deltaMode === 1) {
          dx *= 16;
          dy *= 16;
        }
        const legacy = (e as unknown as { wheelDeltaY?: number }).wheelDeltaY;
        const trackpad = legacy ? legacy === -3 * e.deltaY : e.deltaMode === 0;
        if (e.ctrlKey) {
          if (!this.gesture) this.zoomAt(px, py, Math.exp(-dy * 0.012));
        } else if (e.metaKey || e.altKey || !trackpad) {
          this.zoomAt(px, py, Math.exp(-dy * 0.0028));
        } else {
          this.pan(dx, dy);
        }
      },
      { passive: false },
    );

    // WebKit trackpad pinch.
    c.addEventListener("gesturestart", (e) => {
      e.preventDefault();
      this.gesture = { scale: 1 };
    });
    c.addEventListener("gesturechange", (e) => {
      e.preventDefault();
      const g = e as unknown as { scale: number; clientX: number; clientY: number };
      if (!this.gesture) return;
      const f = g.scale / this.gesture.scale;
      this.gesture.scale = g.scale;
      const [px, py] = this.local(g);
      this.zoomAt(px, py, f);
    });
    c.addEventListener("gestureend", (e) => {
      e.preventDefault();
      this.gesture = null;
    });

    c.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      this.drag = { x: e.clientX, y: e.clientY, moved: false, id: e.pointerId };
    });
    c.addEventListener("pointermove", (e) => {
      const [px, py] = this.local(e);
      this.mouse = [px, py];
      if (this.drag && e.buttons & 1) {
        const dx = e.clientX - this.drag.x;
        const dy = e.clientY - this.drag.y;
        if (!this.drag.moved && Math.hypot(dx, dy) > 3) {
          this.drag.moved = true;
          c.setPointerCapture(this.drag.id);
          c.classList.add("grabbing");
          this.setHover(null, px, py);
        }
        if (this.drag.moved) {
          this.drag.x = e.clientX;
          this.drag.y = e.clientY;
          this.pan(-dx, -dy);
        }
        return;
      }
      this.setHover(this.hit(px, py), px, py);
    });
    c.addEventListener("pointerup", (e) => {
      const d = this.drag;
      this.drag = null;
      c.classList.remove("grabbing");
      if (!d || d.moved || e.button !== 0) return;
      const [px, py] = this.local(e);
      const n = this.hit(px, py);
      this.select(n, e.shiftKey || e.metaKey);
    });
    c.addEventListener("pointerleave", () => {
      this.mouse = null;
      if (!this.drag) this.setHover(null, 0, 0);
    });
    c.addEventListener("dblclick", (e) => {
      const [px, py] = this.local(e);
      const n = this.hit(px, py);
      if (!n) return;
      this.diveToward(n, e.altKey);
    });
    c.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      const [px, py] = this.local(e);
      const n = this.hit(px, py);
      if (!n || n.rest) return;
      if (!this.selection.has(n)) this.select(n, false);
      this.events.context?.(n, e.clientX, e.clientY);
    });
  }

  /** Zoom one level deeper toward `n` (or all the way with `deep`). */
  diveToward(n: VNode, deep = false) {
    if (deep) {
      this.flyTo(n.dir && !n.rest ? n : (n.parent ?? this.root));
      return;
    }
    const chain = n.ancestors();
    const i = chain.indexOf(this.focus);
    const next = i >= 0 ? chain[i + 1] : n;
    if (!next) {
      this.flyTo(n);
      return;
    }
    if (!next.dir) this.select(next, false);
    this.flyTo(next);
  }

  select(n: VNode | null, additive: boolean) {
    if (n?.rest) n = null;
    if (!additive) this.selection.clear();
    if (n) {
      if (additive && this.selection.has(n)) this.selection.delete(n);
      else {
        // Don't let a folder and something inside it both be selected.
        for (const s of [...this.selection]) if (s.isAncestorOf(n) || n.isAncestorOf(s)) this.selection.delete(s);
        this.selection.add(n);
      }
    }
    this.events.select?.([...this.selection]);
    this.invalidate();
  }

  private setHover(n: VNode | null, x: number, y: number) {
    if (n !== this.hover) {
      this.hover = n;
      this.invalidate();
    }
    this.events.hover?.(n, x, y);
  }

  // ------------------------------------------------------------- minimap

  private bindMinimap() {
    const c = this.mini.canvas;
    const go = (e: PointerEvent) => {
      const r = c.getBoundingClientRect();
      const wx = (e.clientX - r.left) / this.mini.k;
      const wy = (e.clientY - r.top) / this.mini.k;
      const [, , w] = this.currentView();
      this.fly = null;
      this.applyView([wx, wy, w]);
      this.afterCamera(true);
    };
    c.addEventListener("pointerdown", (e) => {
      c.setPointerCapture(e.pointerId);
      go(e);
    });
    c.addEventListener("pointermove", (e) => {
      if (e.buttons & 1) go(e);
    });
  }

  private resizeMinimap() {
    const m = this.mini;
    const cssW = 200;
    const cssH = Math.round((cssW * this.H) / this.W);
    m.k = cssW / WORLD_W;
    for (const c of [m.canvas, m.image]) {
      c.width = cssW * this.dpr;
      c.height = cssH * this.dpr;
    }
    m.canvas.style.width = `${cssW}px`;
    m.canvas.style.height = `${cssH}px`;
    this.renderMinimapImage();
  }

  private scheduleMinimap() {
    clearTimeout(this.mini.timer);
    this.mini.timer = window.setTimeout(() => this.renderMinimapImage(), 250);
  }

  private renderMinimapImage() {
    const m = this.mini;
    if (!this.root) return;
    const g = m.image.getContext("2d")!;
    g.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    g.fillStyle = DIR_BG[0];
    g.fillRect(0, 0, m.image.width, m.image.height);
    const savedT = this.t;
    this.t = 1;
    const v: View = { x: 0, y: 0, k: m.k, W: m.image.width / this.dpr, H: m.image.height / this.dpr };
    this.drawNode(g, this.root, 0, v, true);
    this.t = savedT;
    this.invalidate();
  }

  private drawMinimap() {
    const m = this.mini;
    const g = m.canvas.getContext("2d")!;
    g.setTransform(1, 0, 0, 1, 0, 0);
    g.clearRect(0, 0, m.canvas.width, m.canvas.height);
    g.drawImage(m.image, 0, 0);
    g.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    const { x, y, k } = this.cam;
    let rx = x * m.k;
    let ry = y * m.k;
    let rw = (this.W / k) * m.k;
    let rh = (this.H / k) * m.k;
    const cw = m.canvas.width / this.dpr;
    const ch = m.canvas.height / this.dpr;
    // Dim everything outside the viewport.
    g.fillStyle = "rgba(6,8,14,0.55)";
    g.beginPath();
    g.rect(0, 0, cw, ch);
    g.rect(rx, ry, rw, rh);
    g.fill("evenodd");
    if (rw < 8 || rh < 8) {
      const cx = rx + rw / 2;
      const cy = ry + rh / 2;
      rw = rh = 8;
      rx = cx - 4;
      ry = cy - 4;
      g.strokeStyle = "rgba(255,255,255,0.9)";
      g.lineWidth = 1;
      g.beginPath();
      g.moveTo(cx - 9, cy);
      g.lineTo(cx + 9, cy);
      g.moveTo(cx, cy - 9);
      g.lineTo(cx, cy + 9);
      g.stroke();
    }
    g.strokeStyle = "rgba(255,255,255,0.95)";
    g.lineWidth = 1.5;
    g.strokeRect(rx, ry, rw, rh);
  }

  // ------------------------------------------------------------ rendering

  invalidate() {
    if (!this.raf) this.raf = requestAnimationFrame((now) => this.frame(now));
  }

  private frame(now: number) {
    this.raf = 0;
    if (!this.root) return;
    let again = false;
    if (this.fly) {
      const t = Math.min(1, (now - this.fly.start) / this.fly.dur);
      this.applyView(this.fly.fn(easeInOut(t)));
      this.afterCamera();
      if (t >= 1) this.fly = null;
      else again = true;
    }
    if (this.tween) {
      const t = Math.min(1, (now - this.tween.start) / this.tween.dur);
      this.t = easeInOut(t);
      if (t >= 1) {
        this.tween = null;
        this.t = 1;
      } else again = true;
    }

    const g = this.ctx;
    g.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    g.fillStyle = "#0a0c11";
    g.fillRect(0, 0, this.W, this.H);
    this.need = [];
    const v: View = { ...this.cam, W: this.W, H: this.H };
    this.drawNode(g, this.root, 0, v, false);
    this.drawOverlays(g, v);
    this.drawMinimap();
    this.flushLoads();
    if (again) this.invalidate();
  }

  private rectOf(n: VNode, v: View): [number, number, number, number] {
    let { x, y, w, h } = n;
    const t = this.t;
    if (t < 1 && n.fw >= 0) {
      x = n.fx + (x - n.fx) * t;
      y = n.fy + (y - n.fy) * t;
      w = n.fw + (w - n.fw) * t;
      h = n.fh + (h - n.fh) * t;
    }
    return [(x - v.x) * v.k, (y - v.y) * v.k, w * v.k, h * v.k];
  }

  private drawNode(g: CanvasRenderingContext2D, n: VNode, depth: number, v: View, mini: boolean) {
    const [sx, sy, sw, sh] = this.rectOf(n, v);
    if (sx > v.W || sy > v.H || sx + sw < 0 || sy + sh < 0) return;
    if (sw < 0.35 || sh < 0.35) return;

    if (n.dir && !n.rest) {
      if (!n.kids) {
        this.block(g, sx, sy, sw, sh, UNLOADED, v);
        if (!mini && !n.loading && sw * sh > LOAD_AREA) this.need.push(n);
        return;
      }
      const d = Math.min(depth, DIR_BG.length - 1);
      this.fill(g, sx, sy, sw, sh, DIR_BG[d], v);
      const s = Math.min(sw, sh);
      const head = n.parent ? s * HEADER : 0;
      const pad = n.parent ? s * PAD : 0;
      if (head > 2) this.fill(g, sx, sy, sw, head + pad, n.pkg ? PKG_HEAD[d] : DIR_HEAD[d], v);
      if (sw >= 1.5 && sh >= 1.5) for (const k of n.kids) this.drawNode(g, k, depth + 1, v, mini);
      if (!mini && head >= 11 && sw > 50) this.headerLabel(g, n, sx, sy, sw, head, pad, v);
      return;
    }

    if (n.rest) {
      this.fill(g, sx, sy, sw, sh, this.hatch, v);
      if (!mini && sw > 90 && sh > 30) this.fileLabel(g, n, sx, sy, sw, sh, v, "rgba(255,255,255,0.75)", "rgba(255,255,255,0.45)");
      return;
    }
    this.block(g, sx, sy, sw, sh, color(n.cat, n.shade), v);
    if (!mini && sw > 64 && sh > 26) this.fileLabel(g, n, sx, sy, sw, sh, v, "rgba(6,8,16,0.9)", "rgba(6,8,16,0.6)");
  }

  /** Fill a rectangle clipped to the viewport (avoids giant canvas ops). */
  private fill(g: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, style: string | CanvasPattern, v: View) {
    const x0 = Math.max(x, -2);
    const y0 = Math.max(y, -2);
    const x1 = Math.min(x + w, v.W + 2);
    const y1 = Math.min(y + h, v.H + 2);
    g.fillStyle = style;
    g.fillRect(x0, y0, x1 - x0, y1 - y0);
  }

  /** A shaded "cushion" tile with a hairline gap so neighbours separate. */
  private block(g: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, style: string, v: View) {
    const gap = w > 5 && h > 5 ? 0.5 : 0;
    x += gap;
    y += gap;
    w -= 2 * gap;
    h -= 2 * gap;
    this.fill(g, x, y, w, h, style, v);
    if (w < 5 || h < 5) return;
    const x0 = Math.max(x, -2);
    const y0 = Math.max(y, -2);
    const x1 = Math.min(x + w, v.W + 2);
    const y1 = Math.min(y + h, v.H + 2);
    const S = this.cushion.width;
    const u0 = ((x0 - x) / w) * S;
    const v0 = ((y0 - y) / h) * S;
    const u1 = ((x1 - x) / w) * S;
    const v1 = ((y1 - y) / h) * S;
    if (u1 - u0 < 0.01 || v1 - v0 < 0.01) return;
    g.drawImage(this.cushion, u0, v0, u1 - u0, v1 - v0, x0, y0, x1 - x0, y1 - y0);
  }

  private fitText(g: CanvasRenderingContext2D, text: string, max: number): string {
    if (max <= 8) return "";
    if (g.measureText(text).width <= max) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (g.measureText(text.slice(0, mid) + "…").width <= max) lo = mid;
      else hi = mid - 1;
    }
    return lo > 0 ? text.slice(0, lo) + "…" : "";
  }

  private headerLabel(g: CanvasRenderingContext2D, n: VNode, sx: number, sy: number, sw: number, head: number, pad: number, v: View) {
    const fs = Math.min(15, Math.max(10, head * 0.6));
    // Stick the label to the visible part of the header.
    const left = Math.max(sx + pad + 6, 8);
    const right = Math.min(sx + sw - 6, v.W - 8);
    const cy = sy + pad + head / 2;
    if (cy < -head || cy > v.H + head || right - left < 30) return;
    g.textBaseline = "middle";
    g.font = `500 ${fs}px -apple-system, "SF Pro Text", system-ui, sans-serif`;
    const sizeText = bytes(n.size);
    const sizeW = g.measureText(sizeText).width;
    const name = this.fitText(g, n.name, right - left - sizeW - 12);
    g.fillStyle = "rgba(255,255,255,0.88)";
    if (name) g.fillText(name, left, cy);
    g.fillStyle = "rgba(255,255,255,0.45)";
    g.font = `400 ${fs}px -apple-system, "SF Pro Text", system-ui, sans-serif`;
    if (name) g.fillText(sizeText, left + g.measureText(name).width + 8, cy);
  }

  private fileLabel(g: CanvasRenderingContext2D, n: VNode, sx: number, sy: number, sw: number, sh: number, v: View, ink: string, muted: string) {
    const fs = Math.min(26, Math.max(10, Math.min(sh * 0.2, sw * 0.08)));
    const left = Math.max(sx + 7, 8);
    const right = Math.min(sx + sw - 7, v.W - 8);
    const top = Math.max(sy + 6, 8);
    if (right - left < 30 || top + fs > Math.min(sy + sh, v.H) - 4) return;
    g.textBaseline = "top";
    g.font = `600 ${fs}px -apple-system, "SF Pro Text", system-ui, sans-serif`;
    const name = this.fitText(g, n.name, right - left);
    if (!name) return;
    g.fillStyle = ink;
    g.fillText(name, left, top);
    if (top + fs * 2.3 < Math.min(sy + sh, v.H) - 4) {
      g.font = `400 ${fs * 0.86}px -apple-system, "SF Pro Text", system-ui, sans-serif`;
      g.fillStyle = muted;
      g.fillText(bytes(n.size), left, top + fs * 1.25);
    }
  }

  private drawOverlays(g: CanvasRenderingContext2D, v: View) {
    const visible = (n: VNode) => this.nodes.get(n.id) === n || n.rest;
    if (this.hover && visible(this.hover) && !this.selection.has(this.hover)) {
      const [x, y, w, h] = this.rectOf(this.hover, v);
      g.strokeStyle = "rgba(255,255,255,0.75)";
      g.lineWidth = 1.5;
      g.strokeRect(Math.max(x, -4) + 0.75, Math.max(y, -4) + 0.75, Math.min(w, v.W + 8) - 1.5, Math.min(h, v.H + 8) - 1.5);
    }
    for (const n of this.selection) {
      if (!visible(n)) continue;
      const [x, y, w, h] = this.rectOf(n, v);
      const rx = Math.max(x, -4);
      const ry = Math.max(y, -4);
      const rw = Math.min(x + w, v.W + 4) - rx;
      const rh = Math.min(y + h, v.H + 4) - ry;
      if (rw <= 0 || rh <= 0) continue;
      g.save();
      g.shadowColor = "rgba(140,160,255,0.9)";
      g.shadowBlur = 14;
      g.strokeStyle = "#ffffff";
      g.lineWidth = 2;
      g.strokeRect(rx + 1, ry + 1, Math.max(0, rw - 2), Math.max(0, rh - 2));
      g.restore();
      if (w < 6 && h < 6) {
        // Tiny selection: draw a locator ring so it can be found.
        g.strokeStyle = "rgba(255,255,255,0.9)";
        g.lineWidth = 1.5;
        g.beginPath();
        g.arc(x + w / 2, y + h / 2, 10, 0, Math.PI * 2);
        g.stroke();
      }
    }
  }
}
