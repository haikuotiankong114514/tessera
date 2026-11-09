/**
 * Treemap view engine: keeps one tweened item per rect id, runs the frame loop only
 * while something moves, and does hit-testing locally (no round trip to Rust).
 * It knows nothing about the backend; the app hands it decoded layouts.
 */
import { Kind, type Layout, type Rect } from '../backend/codec';
import type { Theme } from '../theme';
import type { Motion } from './ease';
import { BoxBatch, type GpuMode } from './gpu';

export type MotionKind = 'instant' | 'update' | 'scan' | 'live' | 'zoom' | 'morph' | 'mode' | 'delete';

export interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}
interface Frame extends Box {
  a: number;
}

export interface MotionSet {
  zoom: Motion;
  update: Motion;
  scan: Motion;
  live: Motion;
  morph: Motion;
  mode: Motion;
  delete: Motion;
  instant: Motion;
  remove: Motion;
  reflow: Motion;
  reflowDelay: number;
  appear: Motion;
  appearFrom(r: Box): Box;
  removeTo(r: Box): Box;
}

export class Item {
  x = 0;
  y = 0;
  w = 0;
  h = 0;
  a = 0;
  /** hover amount 0..1, eased */
  hv = 0;
  /** dimmed by the category filter 0..1, eased */
  dm = 0;
  /** visible alpha after dimming */
  va = 0;
  /** leaving the map */
  rm = false;
  done = true;
  ord = 0;
  private f: Frame = { x: 0, y: 0, w: 0, h: 0, a: 0 };
  private t: Frame = { x: 0, y: 0, w: 0, h: 0, a: 0 };
  private t0 = 0;
  private dur = 1;
  private ease: (t: number) => number = t => t;
  constructor(
    readonly id: number,
    public r: Rect,
  ) {}

  sample(now: number) {
    if (this.done) return;
    const t = (now - this.t0) / this.dur;
    const f = this.f;
    const g = this.t;
    if (t <= 0) {
      this.x = f.x; this.y = f.y; this.w = f.w; this.h = f.h; this.a = f.a;
      return;
    }
    if (t >= 1) {
      this.x = g.x; this.y = g.y; this.w = g.w; this.h = g.h; this.a = g.a;
      this.done = true;
      return;
    }
    const p = this.ease(t);
    const q = p < 0 ? 0 : p > 1 ? 1 : p;
    this.x = f.x + (g.x - f.x) * p;
    this.y = f.y + (g.y - f.y) * p;
    this.w = Math.max(0, f.w + (g.w - f.w) * p);
    this.h = Math.max(0, f.h + (g.h - f.h) * p);
    this.a = f.a + (g.a - f.a) * q;
  }

  go(from: Frame, to: Frame, now: number, m: Motion, delay = 0) {
    this.f = { ...from };
    this.t = { x: to.x, y: to.y, w: to.w, h: to.h, a: to.a };
    this.t0 = now + delay;
    this.dur = m.dur;
    this.ease = m.ease;
    this.done = false;
  }

  snap(r: Box) {
    this.x = r.x; this.y = r.y; this.w = r.w; this.h = r.h; this.a = 1;
    this.t = { x: r.x, y: r.y, w: r.w, h: r.h, a: 1 };
    this.done = true;
  }

  /** already heading to this target */
  headingTo(r: Box) {
    const t = this.t;
    return !this.done && t.x === r.x && t.y === r.y && t.w === r.w && t.h === r.h && t.a === 1;
  }
}

const cur = (it: Item): Frame => ({ x: it.x, y: it.y, w: it.w, h: it.h, a: it.a });
/** map a box from camera A's frame into camera B's frame */
export function camMap(A: Box, B: Box, r: Box): Box {
  const sx = B.w / A.w;
  const sy = B.h / A.h;
  return { x: B.x + (r.x - A.x) * sx, y: B.y + (r.y - A.y) * sy, w: r.w * sx, h: r.h * sy };
}

export interface EngineHost {
  sel: { has(id: number): boolean; [Symbol.iterator](): Iterator<number> };
  hoverId: number | null;
  /** category filter: tiles of other categories are dimmed */
  filter: number | null;
  scanning: boolean;
  reduced: boolean;
  theme: Theme;
}

export interface PaintState {
  now: number;
  dpr: number;
  W: number;
  H: number;
  theme: Theme;
  sel: { has(id: number): boolean };
  hoverId: number | null;
  mouse: { x: number; y: number; in: boolean };
  scanning: boolean;
}

export interface Painter {
  /**
   * the map itself: changes only with the layout, the theme or the category filter (cached).
   * With `gpu` the boxes go to it and `ctx` gets only what is drawn over them.
   */
  scene(ctx: CanvasRenderingContext2D, list: readonly Item[], S: PaintState, gpu: BoxBatch | null): void;
  /**
   * what changes between layouts, drawn over the scene every frame: hover and selection on
   * `marked` (the hovered and selected items, in paint order), the pointer light, and tiles
   * that animate by themselves
   */
  overlay(ctx: CanvasRenderingContext2D, list: readonly Item[], marked: readonly Item[], S: PaintState): void;
  /** tiles that overlay() paints instead of scene(); while scanning they animate */
  inOverlay(it: Item): boolean;
  /** radius of the pointer light in CSS px, 0 for none */
  readonly light: number;
  /** forget caches that depend on the theme or DPR */
  reset(): void;
}

export interface ApplyOpts {
  /** camera move for zooms: A is the old frame of the region, B the new one */
  cam?: (rects: Rect[]) => { A: Box; B: Box } | null;
  /** existing tiles slide with the reflow curve after a short delay (deletes) */
  reflow?: boolean;
}

export interface EngineStats {
  rects: number;
  fps: number;
  frameMs: number;
  busy: boolean;
  /** boxes are drawn with WebGL */
  gpu: boolean;
}

/** draw-order comparator: shallow before deep, leaving tiles under staying ones, then layout order */
const paintOrder = (a: Item, b: Item) => a.r.depth - b.r.depth || +a.rm - +b.rm || a.ord - b.ord;

/** dirty regions beyond this count are merged into their bounding box */
const MAX_DIRTY = 6;

/** a new layout that moves no edge of a resting tile by this many CSS px puts it there at once */
const SNAP_PX = 0.5;

/** every edge of `a` lies within SNAP_PX of the same edge of `b` */
export function nearlySame(a: Box, b: Box): boolean {
  return (
    Math.abs(a.x - b.x) < SNAP_PX &&
    Math.abs(a.y - b.y) < SNAP_PX &&
    Math.abs(a.x + a.w - (b.x + b.w)) < SNAP_PX &&
    Math.abs(a.y + a.h - (b.y + b.h)) < SNAP_PX
  );
}

export class TreemapEngine {
  readonly items = new Map<number, Item>();
  drawList: Item[] = [];
  W = 0;
  H = 0;
  dpr = 1;
  readonly mouse = { x: -1, y: -1, in: false, moved: false };
  readonly stats: EngineStats = { rects: 0, fps: 0, frameMs: 0, busy: false, gpu: false };
  onStats: ((s: EngineStats) => void) | null = null;
  private ctx: CanvasRenderingContext2D;
  /** folders, header bands and tiles, on a WebGL canvas under the 2D one; null draws them in 2D */
  private gpu: BoxBatch | null = null;
  private batch: BoxBatch | null = null;
  private raf = 0;
  private lastNow = 0;
  private frames: number[] = [];
  private statsAt = 0;
  /**
   * The settled scene, cached in device pixels. While nothing moves, a frame restores only
   * the regions whose overlay changed from here and redraws the overlay inside them, so
   * hovering costs the same in a small window and a maximized 4K one.
   */
  private layer: HTMLCanvasElement | null = null;
  private layerCtx: CanvasRenderingContext2D | null = null;
  private layerOk = false;
  /** the scene must be repainted: new layout, size or DPR */
  private sceneDirty = true;
  private sceneTheme: unknown = null;
  /** hovered and selected items of this frame, in paint order */
  private marked: Item[] = [];
  /** tiles the painter draws in the overlay */
  private ovTiles: Item[] = [];
  /** what the overlay showed per item in the last frame: hover level and selection */
  private shown = new Map<number, number>();
  private shownLight: Box | null = null;

  constructor(
    readonly canvas: HTMLCanvasElement,
    private host: EngineHost,
    public painter: Painter,
    public motion: MotionSet,
    public outer: number,
    /** a canvas under `canvas`, the same size, for the boxes */
    readonly gpuCanvas: HTMLCanvasElement | null = null,
    gpuMode: GpuMode = 'auto',
  ) {
    const ctx = canvas.getContext('2d', { alpha: true });
    if (!ctx) throw new Error('canvas 2d unavailable');
    this.ctx = ctx;
    if (gpuCanvas) {
      try {
        this.batch = BoxBatch.create(gpuCanvas, gpuMode);
      } catch {
        this.batch = null;
      }
      this.gpu = this.batch;
      gpuCanvas.addEventListener('webglcontextlost', this.onLost);
      gpuCanvas.addEventListener('webglcontextrestored', this.onRestored);
      this.showGpu();
    }
  }

  /** a lost context (driver reset, GPU switch) falls back to the 2D painter until it is back */
  private onLost = (e: Event) => {
    e.preventDefault();
    this.gpu = null;
    this.showGpu();
    this.invalidate();
  };

  private onRestored = () => {
    try {
      this.batch?.init();
      this.gpu = this.batch;
    } catch {
      this.gpu = null;
    }
    this.showGpu();
    this.invalidate();
  };

  private showGpu() {
    if (this.gpuCanvas) this.gpuCanvas.style.visibility = this.gpu ? 'visible' : 'hidden';
    this.stats.gpu = !!this.gpu;
  }

  /** returns true when the size or DPR changed (a new layout is needed) */
  resize(W: number, H: number, dpr: number): boolean {
    const changed = W !== this.W || H !== this.H || dpr !== this.dpr;
    if (!changed) return false;
    if (dpr !== this.dpr) this.painter.reset();
    this.W = W;
    this.H = H;
    this.dpr = dpr;
    this.canvas.width = Math.round(W * dpr);
    this.canvas.height = Math.round(H * dpr);
    this.canvas.style.width = W + 'px';
    this.canvas.style.height = H + 'px';
    const g = this.gpuCanvas;
    if (g) {
      g.width = this.canvas.width;
      g.height = this.canvas.height;
      g.style.width = W + 'px';
      g.style.height = H + 'px';
    }
    if (this.layer) {
      this.layer.width = this.canvas.width;
      this.layer.height = this.canvas.height;
    }
    this.layerOk = false;
    this.sceneDirty = true;
    this.kick();
    return true;
  }

  /** repaint everything on the next frame (e.g. after the painter's caches were reset) */
  invalidate() {
    this.sceneDirty = true;
    this.kick();
  }

  stageBox(): Box {
    const o = this.outer;
    return { x: o, y: o, w: this.W - 2 * o, h: this.H - 2 * o };
  }

  /**
   * Draw now instead of on the next animation frame. A resize clears the canvas; called from
   * the ResizeObserver (which runs before paint) this keeps that frame from showing it empty.
   */
  drawNow() {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.frame(performance.now());
  }

  apply(L: Layout, kind: MotionKind, opt: ApplyOpts = {}) {
    const rects = L.rects;
    const now = performance.now();
    const M = this.motion;
    const m = M[kind] ?? M.update;
    const cam = opt.cam ? opt.cam(rects) : null;
    const instant = kind === 'instant' || this.host.reduced;
    const seen = new Set<number>();
    for (let i = 0; i < rects.length; i++) {
      const r = rects[i];
      seen.add(r.id);
      let it = this.items.get(r.id);
      if (instant) {
        if (!it) this.items.set(r.id, (it = new Item(r.id, r)));
        it.r = r;
        it.ord = i;
        it.rm = false;
        it.snap(r);
        continue;
      }
      if (it) {
        it.sample(now);
        const from = cur(it);
        it.rm = false;
        it.r = r;
        it.ord = i;
        if (it.headingTo(r)) continue;
        // a sub-pixel nudge (a file somewhere grew a little) would animate the whole map for
        // nothing: the tile just takes its new place in the one repaint this layout costs
        if (it.done && it.a === 1 && nearlySame(it, r)) it.snap(r);
        else if (it.x !== r.x || it.y !== r.y || it.w !== r.w || it.h !== r.h || it.a !== 1)
          it.go(from, { ...r, a: 1 }, now, opt.reflow ? M.reflow : m, opt.reflow ? M.reflowDelay : 0);
      } else {
        this.items.set(r.id, (it = new Item(r.id, r)));
        it.ord = i;
        let from: Frame;
        if (cam) from = { ...camMap(cam.B, cam.A, r), a: 0 };
        else if (kind === 'morph' || kind === 'mode') from = { ...r, a: 0 };
        else from = { ...M.appearFrom(r), a: 0 };
        it.go(from, { ...r, a: 1 }, now, cam || kind === 'morph' ? m : M.appear);
      }
    }
    for (const [id, it] of this.items) {
      if (seen.has(id)) continue;
      if (instant) {
        this.items.delete(id);
        continue;
      }
      if (it.rm && !cam) continue;
      it.sample(now);
      const from = cur(it);
      it.rm = true;
      if (cam) it.go(from, { ...camMap(cam.A, cam.B, from), a: 0 }, now, m);
      else it.go(from, { ...M.removeTo(from), a: 0 }, now, M.remove);
    }
    this.rebuild();
    this.stats.rects = rects.length;
    this.kick();
  }

  private rebuild() {
    this.drawList = [...this.items.values()].sort(paintOrder);
    this.sceneDirty = true;
  }

  kick() {
    if (!this.raf) this.raf = requestAnimationFrame(this.frame);
  }

  private frame = (now: number) => {
    this.raf = 0;
    const t0 = performance.now();
    const dt = Math.min(64, this.lastNow ? now - this.lastNow : 16);
    this.lastNow = now;
    const host = this.host;
    /** the scene changed in this frame */
    let changed = false;
    /** the scene keeps changing: the next frame paints it again */
    let moving = false;
    /** hover is still easing (overlay only) */
    let easing = false;
    let purge = false;
    const k = 1 - Math.exp(-dt / 55);
    const kd = 1 - Math.exp(-dt / 140);
    const hover = host.hoverId;
    const f = host.filter;
    const marked = this.marked;
    marked.length = 0;
    const ovTiles = this.ovTiles;
    ovTiles.length = 0;
    for (const it of this.drawList) {
      if (!it.done) {
        it.sample(now);
        changed = true;
        if (!it.done) moving = true;
        else if (it.rm) purge = true;
      }
      const ht = it.id === hover ? 1 : 0;
      if (it.hv !== ht) {
        it.hv += (ht - it.hv) * (host.reduced ? 1 : k);
        if (Math.abs(it.hv - ht) < 0.004) it.hv = ht;
        else easing = true;
      }
      const r = it.r;
      const dtgt = f != null && r.kind !== Kind.Dir && (r.kind === Kind.Free || r.kind === Kind.Unscanned || r.cat !== f) ? 1 : 0;
      if (it.dm !== dtgt) {
        it.dm += (dtgt - it.dm) * (host.reduced ? 1 : kd);
        if (Math.abs(it.dm - dtgt) < 0.004) it.dm = dtgt;
        else moving = true;
        changed = true;
      }
      it.va = it.a * (1 - it.dm * 0.78);
      if (it.hv > 0.01) marked.push(it);
      if (this.painter.inOverlay(it)) ovTiles.push(it);
    }
    if (purge) {
      for (const it of this.drawList) if (it.rm && it.done) this.items.delete(it.id);
      this.drawList = this.drawList.filter(it => !(it.rm && it.done));
      for (let i = marked.length - 1; i >= 0; i--) if (marked[i].rm && marked[i].done) marked.splice(i, 1);
      for (let i = ovTiles.length - 1; i >= 0; i--) if (ovTiles[i].rm && ovTiles[i].done) ovTiles.splice(i, 1);
    }
    for (const id of host.sel) {
      const it = this.items.get(id);
      if (it && it.hv <= 0.01) marked.push(it);
    }
    if (marked.length > 1) marked.sort(paintOrder);
    if (host.theme !== this.sceneTheme) {
      this.sceneTheme = host.theme;
      this.sceneDirty = true;
    }

    const S: PaintState = {
      now,
      dpr: this.dpr,
      W: this.W,
      H: this.H,
      theme: host.theme,
      sel: host.sel,
      hoverId: hover,
      mouse: this.mouse,
      scanning: host.scanning,
    };
    if (changed || this.sceneDirty || !this.layerOk) this.paintAll(S, moving);
    else this.paintDirty(S);
    this.remember();

    const cost = performance.now() - t0;
    this.stats.frameMs = this.stats.frameMs ? this.stats.frameMs * 0.9 + cost * 0.1 : cost;
    this.frames.push(now);
    while (this.frames.length && this.frames[0] < now - 1000) this.frames.shift();
    const cont = (host.scanning && !host.reduced) || (this.mouse.in && this.mouse.moved);
    this.mouse.moved = false;
    const going = moving || easing || cont;
    if (going) this.kick();
    else this.frames.length = 0;
    this.stats.fps = this.frames.length;
    if (this.statsAt === 0 || this.stats.busy !== going || now - this.statsAt > 250) {
      this.stats.busy = going;
      this.statsAt = now;
      this.onStats?.(this.stats);
    }
  };

  /**
   * Paint the whole frame. While tiles move the scene goes straight to the screen; once it
   * holds still it is painted into the cache layer first, and later frames start from there.
   */
  private paintAll(S: PaintState, moving: boolean) {
    const ctx = this.ctx;
    const d = this.dpr;
    const gpu = this.gpu;
    this.sceneDirty = false;
    gpu?.begin();
    if (moving) {
      this.layerOk = false;
      ctx.setTransform(d, 0, 0, d, 0, 0);
      this.painter.scene(ctx, this.drawList, S, gpu);
    } else {
      const lc = this.ensureLayer();
      lc.setTransform(d, 0, 0, d, 0, 0);
      this.painter.scene(lc, this.drawList, S, gpu);
      this.layerOk = true;
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
      ctx.drawImage(this.layer!, 0, 0);
      ctx.setTransform(d, 0, 0, d, 0, 0);
    }
    // the GPU canvas holds the boxes until the scene changes again; hover frames leave it alone
    gpu?.draw();
    this.painter.overlay(ctx, this.drawList, this.marked, S);
    ctx.globalAlpha = 1;
  }

  /** restore the regions whose overlay changed from the cached scene, then redraw the overlay there */
  private paintDirty(S: PaintState) {
    const dirty = this.dirtyRegions(S);
    if (!dirty.length) return;
    const ctx = this.ctx;
    const d = this.dpr;
    const cw = this.canvas.width;
    const ch = this.canvas.height;
    const px: Box[] = [];
    for (const b of dirty) {
      // device pixels, one pixel wider for antialiased edges
      const x0 = Math.max(0, Math.floor(b.x * d) - 1);
      const y0 = Math.max(0, Math.floor(b.y * d) - 1);
      const x1 = Math.min(cw, Math.ceil((b.x + b.w) * d) + 1);
      const y1 = Math.min(ch, Math.ceil((b.y + b.h) * d) + 1);
      if (x1 > x0 && y1 > y0) px.push({ x: x0, y: y0, w: x1 - x0, h: y1 - y0 });
    }
    // one rect clip per region: a single integer rect clips exactly (a scissor), a union of
    // rects may be clipped through an antialiased mask, which leaves seams at its edges
    for (const r of px) {
      ctx.save();
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.beginPath();
      ctx.rect(r.x, r.y, r.w, r.h);
      ctx.clip();
      ctx.clearRect(r.x, r.y, r.w, r.h);
      ctx.drawImage(this.layer!, r.x, r.y, r.w, r.h, r.x, r.y, r.w, r.h);
      ctx.setTransform(d, 0, 0, d, 0, 0);
      this.painter.overlay(ctx, this.drawList, this.marked, S);
      ctx.restore();
    }
  }

  /** CSS-px regions where this frame's overlay differs from the last one */
  private dirtyRegions(S: PaintState): Box[] {
    let out: Box[] = [];
    const seen = new Set<number>();
    for (const it of this.marked) {
      seen.add(it.id);
      if (this.shown.get(it.id) !== this.markOf(it)) out.push(boxOf(it));
    }
    for (const id of this.shown.keys()) {
      if (seen.has(id)) continue;
      const it = this.items.get(id);
      if (it) out.push(boxOf(it));
    }
    const light = this.lightBox();
    const was = this.shownLight;
    if (!sameBox(light, was)) {
      if (was) out.push(was);
      if (light) out.push(light);
    }
    if (S.scanning) for (const it of this.ovTiles) out.push(boxOf(it));
    if (!out.length) return out;
    if (out.length > MAX_DIRTY) out = [union(out)];
    // A clip edge through an antialiased shape can change how it is rasterized, so a region
    // never cuts through a tile the overlay paints: it grows to hold all of it.
    const whole = this.ovTiles.length ? this.marked.concat(this.ovTiles) : this.marked;
    for (const r of out) {
      for (let grew = true, n = 0; grew && n < 4; n++) {
        grew = false;
        for (const it of whole) {
          const b = boxOf(it);
          if (b.x < r.x + r.w && b.x + b.w > r.x && b.y < r.y + r.h && b.y + b.h > r.y && !contains(r, b)) {
            const u = union([r, b]);
            r.x = u.x;
            r.y = u.y;
            r.w = u.w;
            r.h = u.h;
            grew = true;
          }
        }
      }
    }
    return out;
  }

  /** keep what the overlay shows now, to diff the next frame against */
  private remember() {
    this.shown.clear();
    for (const it of this.marked) this.shown.set(it.id, this.markOf(it));
    this.shownLight = this.lightBox();
  }

  private markOf(it: Item) {
    return Math.round(it.hv * 255) * 2 + (this.host.sel.has(it.id) ? 1 : 0);
  }

  private lightBox(): Box | null {
    const R = this.painter.light;
    const m = this.mouse;
    return R > 0 && m.in ? { x: m.x - R, y: m.y - R, w: 2 * R, h: 2 * R } : null;
  }

  private ensureLayer() {
    if (!this.layer) {
      this.layer = document.createElement('canvas');
      this.layer.width = this.canvas.width;
      this.layer.height = this.canvas.height;
      this.layerCtx = this.layer.getContext('2d', { alpha: true });
      if (!this.layerCtx) throw new Error('canvas 2d unavailable');
    }
    return this.layerCtx!;
  }

  hit(x: number, y: number): Item | null {
    const list = this.drawList;
    for (let i = list.length - 1; i >= 0; i--) {
      const it = list[i];
      if (it.rm || it.a < 0.4) continue;
      if (x >= it.x && x < it.x + it.w && y >= it.y && y < it.y + it.h) return it;
    }
    return null;
  }

  /** the tile next to `id` in an arrow-key direction, among the leaf tiles that `ok` accepts */
  neighbor(id: number | undefined, key: 'ArrowLeft' | 'ArrowRight' | 'ArrowUp' | 'ArrowDown', ok: (r: Rect) => boolean = () => true): Item | null {
    const [dx, dy] = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[key];
    const leaves = this.drawList.filter(it => !it.rm && it.r.kind !== Kind.Dir && it.r.kind !== Kind.Unscanned && ok(it.r) && it.w * it.h > 40);
    const c = id != null ? this.items.get(id) : undefined;
    if (!c || c.rm) return leaves.slice().sort((a, b) => b.w * b.h - a.w * a.h)[0] ?? null;
    let best: Item | null = null;
    let bs = Infinity;
    for (const it of leaves) {
      if (it === c) continue;
      let along: number;
      let perp: number;
      if (dx) {
        along = dx > 0 ? it.x - (c.x + c.w) : c.x - (it.x + it.w);
        perp = Math.max(0, Math.max(it.y, c.y) - Math.min(it.y + it.h, c.y + c.h));
      } else {
        along = dy > 0 ? it.y - (c.y + c.h) : c.y - (it.y + it.h);
        perp = Math.max(0, Math.max(it.x, c.x) - Math.min(it.x + it.w, c.x + c.w));
      }
      if (along < -1) continue;
      const cdist = dx ? Math.abs(it.y + it.h / 2 - (c.y + c.h / 2)) : Math.abs(it.x + it.w / 2 - (c.x + c.w / 2));
      const score = along + perp * 4 + cdist * 0.05;
      if (score < bs) {
        bs = score;
        best = it;
      }
    }
    return best;
  }

  destroy() {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    if (this.layer) this.layer.width = this.layer.height = 0;
    this.layer = null;
    this.layerCtx = null;
    if (this.gpuCanvas) {
      this.gpuCanvas.removeEventListener('webglcontextlost', this.onLost);
      this.gpuCanvas.removeEventListener('webglcontextrestored', this.onRestored);
    }
    this.batch?.dispose();
    this.batch = this.gpu = null;
  }
}

const boxOf = (it: Item): Box => ({ x: it.x, y: it.y, w: it.w, h: it.h });
const contains = (a: Box, b: Box) => b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w && b.y + b.h <= a.y + a.h;
function union(bs: Box[]): Box {
  let x0 = Infinity;
  let y0 = Infinity;
  let x1 = -Infinity;
  let y1 = -Infinity;
  for (const b of bs) {
    x0 = Math.min(x0, b.x);
    y0 = Math.min(y0, b.y);
    x1 = Math.max(x1, b.x + b.w);
    y1 = Math.max(y1, b.y + b.h);
  }
  return { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
}
const sameBox = (a: Box | null, b: Box | null) => a === b || (!!a && !!b && a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h);
