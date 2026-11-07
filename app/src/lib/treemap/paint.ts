/**
 * Fluent painter for the treemap canvas.
 *
 * Rules from the design review:
 * - hover and selection only change fill, stroke and brightness, never position,
 *   size, corner radius or padding;
 * - a tile that cannot hold a whole line of text gets no label (the tooltip has it);
 * - folder depth stays readable: header bands, nested translucent fills that build up
 *   with depth, and an outline whose contrast steps down with depth.
 *
 * The work is split in two so that a big window stays at 60 fps: `scene()` paints the map
 * itself (it changes only with the layout, the theme or the category filter, and the
 * engine keeps it in a cached layer), `overlay()` paints what changes from frame to frame
 * (hover, selection, the pointer light, the moving stripes of the unscanned tile). Between
 * layouts the engine restores just the region that changed from the cache and runs
 * `overlay()` clipped to it.
 *
 * With a GPU batch, `scene()` hands every folder, header band and tile to it (one draw call
 * on a WebGL canvas under this one) and paints only text and textures here.
 */
import { Flag, Kind } from '../backend/codec';
import { CAT_COUNT } from '../backend/categories';
import { fmtBytes } from '../format';
import { t as tr, tn } from '../i18n.svelte';
import { rgba } from '../theme';
import type { Box, Item, Painter, PaintState } from './engine';
import { NONE, type BoxBatch, type RGBA } from './gpu';

export const UI_FONT = '"Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI", "PingFang SC", "Noto Sans SC", system-ui, sans-serif';

const LABEL_MIN_W = 44;
const LINE = 16;
const RADIUS = 3;
const FOLDER_RADIUS = 4;
/** radius of the pointer light, CSS px */
const LIGHT_R = 90;

/** a rect snapped to device pixels, so edges stay crisp at any DPR */
function snapBox(it: Item, dpr: number): Box {
  const x0 = Math.round(it.x * dpr) / dpr;
  const y0 = Math.round(it.y * dpr) / dpr;
  return { x: x0, y: y0, w: Math.round((it.x + it.w) * dpr) / dpr - x0, h: Math.round((it.y + it.h) * dpr) / dpr - y0 };
}

class TextFit {
  private w = new Map<string, number>();
  measure(ctx: CanvasRenderingContext2D, s: string) {
    const k = ctx.font + '\u0001' + s;
    let v = this.w.get(k);
    if (v === undefined) {
      v = ctx.measureText(s).width;
      if (this.w.size > 40000) this.w.clear();
      this.w.set(k, v);
    }
    return v;
  }
  /** the longest prefix (with an ellipsis) that fits, or '' if not even 2 characters do */
  fit(ctx: CanvasRenderingContext2D, s: string, maxW: number) {
    if (maxW < 12 || !s) return '';
    const full = this.measure(ctx, s);
    if (full <= maxW) return s;
    let n = Math.min(s.length - 1, Math.floor((s.length * maxW) / full));
    while (n >= 2) {
      const t = s.slice(0, n) + '…';
      if (this.measure(ctx, t) <= maxW) return t;
      n--;
    }
    return '';
  }
  clear() {
    this.w.clear();
  }
}

export const textFit = new TextFit();

type Pat = { pat: CanvasPattern; size: number };

export class FluentPainter implements Painter {
  private readonly regularFont: string;
  private readonly semiboldFont: string;

  constructor(font = UI_FONT, readonly light = LIGHT_R) {
    this.regularFont = `400 12px ${font}`;
    this.semiboldFont = `600 12px ${font}`;
  }
  private pats = new Map<string, Pat>();
  private batches = new Map<string, Path2D>();
  /** folder outline per depth, per scheme */
  private strokes = new Map<string, string[]>();

  reset() {
    this.pats.clear();
    this.strokes.clear();
    textFit.clear();
  }

  private folderStroke(dark: boolean, depth: number) {
    const k = dark ? 'dark' : 'light';
    let a = this.strokes.get(k);
    if (!a) {
      a = [];
      for (let d = 0; d < 32; d++) a.push(dark ? `rgba(255,255,255,${Math.max(0.05, 0.13 - d * 0.012)})` : `rgba(0,0,0,${Math.max(0.055, 0.15 - d * 0.016)})`);
      this.strokes.set(k, a);
    }
    return a[Math.min(depth, 31)];
  }

  private pattern(ctx: CanvasRenderingContext2D, key: string, size: number, dpr: number, paint: (g: CanvasRenderingContext2D, s: number) => void): Pat {
    const k = key + '|' + dpr;
    let p = this.pats.get(k);
    if (p) return p;
    const c = document.createElement('canvas');
    c.width = c.height = Math.round(size * dpr);
    const g = c.getContext('2d')!;
    g.scale(dpr, dpr);
    paint(g, size);
    p = { pat: ctx.createPattern(c, 'repeat')!, size };
    this.pats.set(k, p);
    return p;
  }

  private usePattern(p: Pat, dpr: number, ox = 0, oy = 0) {
    p.pat.setTransform(new DOMMatrix().translate(ox, oy).scale(1 / dpr));
    return p.pat;
  }

  scene(ctx: CanvasRenderingContext2D, list: readonly Item[], S: PaintState, gpu: BoxBatch | null = null) {
    const th = S.theme;
    const t = th.tok;
    const dark = th.scheme === 'dark';
    const dpr = S.dpr;
    ctx.clearRect(0, 0, S.W, S.H);
    ctx.textBaseline = 'alphabetic';
    ctx.globalAlpha = 1;

    // a closed folder is washed with the category that fills it most, lighter than any
    // file, and keeps its outline and neutral ink so it never reads as a file
    const leafFill = (it: Item) => {
      const c = it.r.cat;
      if (it.r.kind === Kind.DirLeaf) return c < CAT_COUNT ? th.tint[c] : t['tm-leafdir'];
      return c < CAT_COUNT ? th.fill[c] : t['tm-leafdir'];
    };
    const leafInk = (it: Item) => {
      const c = it.r.cat;
      return c < CAT_COUNT && it.r.kind !== Kind.DirLeaf ? th.ink[c] : t['tm-leafdir-ink'];
    };
    const outlined = (it: Item) => it.r.kind === Kind.DirLeaf || it.r.cat >= CAT_COUNT;
    const box = (it: Item) => snapBox(it, dpr);

    /* ---------- pass 1: open folders (containers), shallow to deep ---------- */
    for (const it of list) {
      const r = it.r;
      if (r.kind !== Kind.Dir || it.va <= 0.004 || it.w < 2 || it.h < 2) continue;
      const b = box(it);
      ctx.globalAlpha = it.va;
      if (gpu) {
        // the outline's outer edge lies half a pixel outside the 2D path, so its radius is half a pixel more
        gpu.push(b.x * dpr, b.y * dpr, b.w * dpr, b.h * dpr, (FOLDER_RADIUS + 0.5) * dpr, gpu.color(t['tm-folder'], it.va), gpu.color(this.folderStroke(dark, r.depth), it.va), dpr);
      } else {
        ctx.beginPath();
        ctx.roundRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1, FOLDER_RADIUS);
        ctx.fillStyle = t['tm-folder'];
        ctx.fill();
        ctx.lineWidth = 1;
        ctx.strokeStyle = this.folderStroke(dark, r.depth);
        ctx.stroke();
      }
      if (r.header && b.h > r.header + 2) {
        if (gpu) {
          // rounded on top only: a box that runs on past its bottom corners, cut at the band's bottom
          gpu.push((b.x + 1) * dpr, (b.y + 1) * dpr, (b.w - 2) * dpr, (r.header - 1 + 2 * FOLDER_RADIUS) * dpr, (FOLDER_RADIUS - 1) * dpr, gpu.color(t['tm-folder-head'], it.va), null, 0, (b.y + r.header) * dpr);
        } else {
          ctx.beginPath();
          ctx.roundRect(b.x + 1, b.y + 1, b.w - 2, r.header - 1, [FOLDER_RADIUS - 1, FOLDER_RADIUS - 1, 0, 0]);
          ctx.fillStyle = t['tm-folder-head'];
          ctx.fill();
        }
        this.header(ctx, it, b, S);
      }
    }

    /* ---------- pass 2: tiles; steady ones batched by colour ---------- */
    const batches = this.batches;
    batches.clear();
    const outlines = new Path2D();
    let hasOutlines = false;
    const loose: Item[] = [];
    const overlays: Item[] = [];
    for (const it of list) {
      const r = it.r;
      if (r.kind === Kind.Dir || r.kind === Kind.Unscanned || it.va <= 0.004 || it.w < 0.6 || it.h < 0.6) continue;
      if (r.kind === Kind.Free || it.va < 0.995 || r.flags & Flag.Cloud) {
        loose.push(it);
        continue;
      }
      const b = box(it);
      const col = leafFill(it);
      if (gpu) {
        this.gpuTile(gpu, b, dpr, gpu.color(col), outlined(it) ? gpu.color(t['tm-leafdir-stroke']) : null);
        if (r.kind === Kind.Group || r.flags & (Flag.NoAccess | Flag.InUse)) overlays.push(it);
        continue;
      }
      let p = batches.get(col);
      if (!p) batches.set(col, (p = new Path2D()));
      if (b.w < 5 || b.h < 5) p.rect(b.x, b.y, b.w, b.h);
      else p.roundRect(b.x, b.y, b.w, b.h, RADIUS);
      if (outlined(it) && b.w > 3 && b.h > 3) {
        // a closed folder: outlined like the open ones, so it never reads as a file type
        outlines.roundRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1, Math.min(RADIUS, b.w / 2, b.h / 2));
        hasOutlines = true;
      }
      if (r.kind === Kind.Group || r.flags & (Flag.NoAccess | Flag.InUse)) overlays.push(it);
    }
    ctx.globalAlpha = 1;
    for (const [col, p] of batches) {
      ctx.fillStyle = col;
      ctx.fill(p);
    }
    if (hasOutlines) {
      ctx.lineWidth = 1;
      ctx.strokeStyle = t['tm-leafdir-stroke'];
      ctx.stroke(outlines);
    }
    for (const it of loose) {
      const r = it.r;
      const b = box(it);
      if (r.kind === Kind.Free) {
        this.special(ctx, it, b, S);
        continue;
      }
      ctx.globalAlpha = it.va * (r.flags & Flag.Cloud ? 0.55 : 1);
      if (gpu) {
        const a = ctx.globalAlpha;
        this.gpuTile(gpu, b, dpr, gpu.color(leafFill(it), a), outlined(it) ? gpu.color(t['tm-leafdir-stroke'], a) : null);
        if (r.kind === Kind.Group || r.flags & (Flag.NoAccess | Flag.InUse)) overlays.push(it);
        continue;
      }
      ctx.beginPath();
      if (b.w < 5 || b.h < 5) ctx.rect(b.x, b.y, b.w, b.h);
      else ctx.roundRect(b.x, b.y, b.w, b.h, RADIUS);
      ctx.fillStyle = leafFill(it);
      ctx.fill();
      if (outlined(it) && b.w > 3 && b.h > 3) {
        ctx.beginPath();
        ctx.roundRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1, Math.min(RADIUS, b.w / 2, b.h / 2));
        ctx.lineWidth = 1;
        ctx.strokeStyle = t['tm-leafdir-stroke'];
        ctx.stroke();
      }
      if (r.kind === Kind.Group || r.flags & (Flag.NoAccess | Flag.InUse)) overlays.push(it);
    }

    /* ---------- pass 3: texture overlays (merged items, locked files) ---------- */
    for (const it of overlays) {
      const r = it.r;
      const b = box(it);
      ctx.globalAlpha = it.va;
      ctx.beginPath();
      ctx.roundRect(b.x, b.y, b.w, b.h, Math.min(RADIUS, b.w / 2, b.h / 2));
      if (r.kind === Kind.Group) {
        const p = this.pattern(ctx, 'dots' + th.scheme, 6, dpr, g => {
          g.fillStyle = t['tm-dots'];
          g.fillRect(2, 2, 1.5, 1.5);
        });
        ctx.fillStyle = this.usePattern(p, dpr);
        ctx.fill();
      } else {
        const p = this.pattern(ctx, 'hatch' + th.scheme, 8, dpr, stripes(t['tm-hatch'], 8, 2));
        ctx.fillStyle = this.usePattern(p, dpr);
        ctx.fill();
      }
    }

    /* ---------- pass 4: labels ---------- */
    for (const it of list) {
      const r = it.r;
      if (r.kind === Kind.Dir || r.kind === Kind.Free || r.kind === Kind.Unscanned || it.va < 0.25) continue;
      if (it.w < LABEL_MIN_W || it.h < 20) continue;
      const name = r.kind === Kind.Group ? tn('smallItems', r.count) : r.name;
      if (!name) continue;
      const b = box(it);
      ctx.globalAlpha = it.va * (r.flags & Flag.Cloud ? 0.8 : 1);
      const ink = leafInk(it);
      this.label(ctx, name, fmtBytes(r.size), b, ink, ink);
    }

    ctx.globalAlpha = 1;
  }

  /** a tile on the GPU, as pass 2 paints it: the fill, then a closed folder's outline over it */
  private gpuTile(gpu: BoxBatch, b: Box, dpr: number, fill: RGBA, stroke: RGBA | null) {
    gpu.push(b.x * dpr, b.y * dpr, b.w * dpr, b.h * dpr, b.w < 5 || b.h < 5 ? 0 : RADIUS * dpr, fill);
    if (stroke && b.w > 3 && b.h > 3) gpu.push(b.x * dpr, b.y * dpr, b.w * dpr, b.h * dpr, (Math.min(RADIUS, b.w / 2, b.h / 2) + 0.5) * dpr, NONE, stroke, dpr);
  }

  inOverlay(it: Item) {
    return it.r.kind === Kind.Unscanned;
  }

  overlay(ctx: CanvasRenderingContext2D, list: readonly Item[], marked: readonly Item[], S: PaintState) {
    const t = S.theme.tok;
    const dark = S.theme.scheme === 'dark';
    const dpr = S.dpr;
    const box = (it: Item) => snapBox(it, dpr);

    /* ---------- the unscanned tile: its stripes move while the scan runs ---------- */
    for (const it of list) {
      if (it.r.kind === Kind.Unscanned && it.va > 0.004 && it.w >= 0.6 && it.h >= 0.6) this.special(ctx, it, box(it), S);
    }

    /* ---------- pass 5: hover and selection (paint only, geometry untouched) ---------- */
    for (const it of marked) {
      const r = it.r;
      const b = box(it);
      if (it.hv > 0.01) {
        ctx.globalAlpha = it.va * it.hv;
        ctx.beginPath();
        if (r.kind === Kind.Dir) {
          ctx.roundRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1, FOLDER_RADIUS);
          ctx.lineWidth = 1;
          ctx.strokeStyle = t['ctl-strong'];
          ctx.stroke();
        } else {
          ctx.roundRect(b.x, b.y, b.w, b.h, Math.min(RADIUS, b.w / 2, b.h / 2));
          ctx.fillStyle = t['tm-hover'];
          ctx.fill();
        }
      }
      if (S.sel.has(it.id) && b.w > 3 && b.h > 3) {
        const rad = r.kind === Kind.Dir ? FOLDER_RADIUS : RADIUS;
        ctx.globalAlpha = it.va;
        ctx.beginPath();
        ctx.roundRect(b.x + 1, b.y + 1, b.w - 2, b.h - 2, Math.max(0, Math.min(rad, b.w / 2 - 1, b.h / 2 - 1)));
        ctx.lineWidth = 2;
        ctx.strokeStyle = t.accent;
        ctx.stroke();
        if (b.w > 8 && b.h > 8) {
          ctx.beginPath();
          ctx.roundRect(b.x + 2.5, b.y + 2.5, b.w - 5, b.h - 5, Math.max(0, Math.min(rad - 1.5, b.w / 2 - 3, b.h / 2 - 3)));
          ctx.lineWidth = 1;
          ctx.strokeStyle = dark ? 'rgba(0,0,0,0.55)' : 'rgba(255,255,255,0.9)';
          ctx.stroke();
        }
      }
    }

    /* ---------- pass 6: pointer light on nearby tile edges ---------- */
    if (S.mouse.in && this.light > 0) {
      const mx = S.mouse.x;
      const my = S.mouse.y;
      const R = this.light;
      const g = ctx.createRadialGradient(mx, my, 0, mx, my, R);
      g.addColorStop(0, t['tm-reveal']);
      g.addColorStop(1, 'rgba(0,0,0,0)');
      ctx.globalAlpha = 1;
      ctx.beginPath();
      for (const it of list) {
        if (it.rm || it.r.kind === Kind.Dir || it.w < 4 || it.h < 4) continue;
        if (it.x > mx + R || it.x + it.w < mx - R || it.y > my + R || it.y + it.h < my - R) continue;
        const b = box(it);
        ctx.rect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1);
      }
      ctx.lineWidth = 1;
      ctx.strokeStyle = g;
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
  }

  private header(ctx: CanvasRenderingContext2D, it: Item, b: { x: number; y: number; w: number; h: number }, S: PaintState) {
    const r = it.r;
    const t = S.theme.tok;
    if (b.w < 30 || !r.name) return;
    let x = b.x + 8;
    const base = b.y + Math.round(r.header / 2 + 4.5);
    if (r.flags & (Flag.Protected | Flag.NoAccess) && b.w > 80) {
      lockGlyph(ctx, x, b.y + r.header / 2 - 5.5, t['text-3']);
      x += 15;
    }
    ctx.font = this.regularFont;
    const sz = fmtBytes(r.size);
    const sw = textFit.measure(ctx, sz);
    const showSize = b.w > sw + 96;
    ctx.font = this.semiboldFont;
    const nm = textFit.fit(ctx, r.name, b.x + b.w - x - 8 - (showSize ? sw + 10 : 0));
    if (nm) {
      ctx.fillStyle = t['tm-folder-text'];
      ctx.fillText(nm, x, base);
    }
    if (showSize) {
      ctx.font = this.regularFont;
      ctx.fillStyle = t['text-3'];
      ctx.fillText(sz, b.x + b.w - sw - 8, base);
    }
  }

  private label(ctx: CanvasRenderingContext2D, name: string, size: string, b: { x: number; y: number; w: number; h: number }, ink: string, ink2: string) {
    const px = 7;
    ctx.font = this.regularFont;
    const nm = textFit.fit(ctx, name, b.w - px * 2);
    if (!nm) return;
    ctx.fillStyle = ink;
    ctx.fillText(nm, b.x + px, b.y + 16);
    if (b.h >= 16 + LINE + 6) {
      ctx.font = this.semiboldFont;
      const s = textFit.fit(ctx, size, b.w - px * 2);
      if (s) {
        ctx.fillStyle = ink2;
        ctx.fillText(s, b.x + px, b.y + 16 + LINE);
      }
    }
  }

  private special(ctx: CanvasRenderingContext2D, it: Item, b: { x: number; y: number; w: number; h: number }, S: PaintState) {
    const t = S.theme.tok;
    const free = it.r.kind === Kind.Free;
    ctx.globalAlpha = it.va;
    ctx.beginPath();
    ctx.roundRect(b.x + 0.5, b.y + 0.5, b.w - 1, b.h - 1, FOLDER_RADIUS);
    if (free) {
      ctx.fillStyle = rgba(t.accent, S.theme.scheme === 'dark' ? 0.08 : 0.06);
      ctx.fill();
    } else {
      const p = this.pattern(ctx, 'unscanned' + S.theme.scheme, 12, S.dpr, stripes(t['tm-stripe'], 12, 4));
      ctx.fillStyle = this.usePattern(p, S.dpr, S.scanning ? (S.now / 40) % 12 : 0, 0);
      ctx.fill();
    }
    ctx.setLineDash([4, 3]);
    ctx.lineWidth = 1;
    ctx.strokeStyle = free ? t.accent : t['ctl-strong'];
    ctx.stroke();
    ctx.setLineDash([]);
    if (b.w >= 60 && b.h >= 22) {
      const name = free ? tr('free') : tr('unscanned');
      ctx.font = this.semiboldFont;
      const nm = textFit.fit(ctx, name, b.w - 16);
      if (!nm) return;
      ctx.fillStyle = free ? t.accent : t['text-2'];
      ctx.fillText(nm, b.x + 8, b.y + 17);
      if (b.h >= 40) {
        ctx.font = this.regularFont;
        ctx.fillStyle = t['text-3'];
        ctx.fillText(textFit.fit(ctx, fmtBytes(it.r.size), b.w - 16), b.x + 8, b.y + 17 + LINE);
      }
    }
  }
}

function stripes(color: string, size: number, width: number) {
  return (g: CanvasRenderingContext2D, s: number) => {
    g.strokeStyle = color;
    g.lineWidth = width;
    g.beginPath();
    for (let i = -s; i <= s * 2; i += size) {
      g.moveTo(i, s);
      g.lineTo(i + s, 0);
    }
    g.stroke();
  };
}

function lockGlyph(ctx: CanvasRenderingContext2D, x: number, y: number, color: string) {
  ctx.save();
  ctx.strokeStyle = color;
  ctx.fillStyle = color;
  ctx.lineWidth = 1.2;
  ctx.beginPath();
  ctx.roundRect(x + 1, y + 5, 9, 6.5, 1.2);
  ctx.fill();
  ctx.beginPath();
  ctx.arc(x + 5.5, y + 5, 2.8, Math.PI, 0);
  ctx.stroke();
  ctx.restore();
}
