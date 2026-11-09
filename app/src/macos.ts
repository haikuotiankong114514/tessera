/** Native-shell adapter. Rendering, animation, hit testing, batching, caches and
 * the binary codec are the upstream implementation, not a parallel renderer. */
import { TreemapEngine, type EngineHost, type MotionKind } from './lib/treemap/engine';
import { FluentPainter } from './lib/treemap/paint';
import { fluentMotion, fluentLayout } from './lib/treemap/motion';
import { makeTheme } from './lib/theme';
import { decodeLayout, isNodeId, Kind } from './lib/backend/codec';
import { setLang, t, tn } from './lib/i18n.svelte';

type State = { generation: number; view: number; logical: boolean; scanning: boolean; ready: boolean; rev: number;
  scheme: 'light' | 'dark'; accent: string; selection: number[]; reduced: boolean; lang: string; active: boolean; filter?: number };
type HostWindow = Window & { webkit?: { messageHandlers: { tessera: { postMessage(value: unknown): void } } }; tessera?: unknown };
const win = window as HostWindow;
const canvas = document.querySelector<HTMLCanvasElement>('#map')!;
const gpu = document.querySelector<HTMLCanvasElement>('#gpu')!;
const message = document.querySelector<HTMLDivElement>('#message')!;
const tip = document.querySelector<HTMLDivElement>('#tip')!;
let state: State | null = null;
const host: EngineHost = { sel: new Set(), hoverId: null, filter: null, scanning: false, reduced: false, theme: makeTheme('light', null) };
const engine = new TreemapEngine(canvas, host, new FluentPainter('-apple-system, system-ui, sans-serif', 0), fluentMotion, fluentLayout.outer, gpu);
let inFlight = false;
let pending: MotionKind | null = null;
let disposed = false;
let layoutSerial = 0;
let appliedSerial = -1;
let lastSelected: number | null = null;
const metrics = { requests: 0, layoutBytes: 0, decodedMs: [] as number[], roundTripMs: [] as number[], errors: [] as string[] };
function post(action: string, extra: object = {}) { win.webkit?.messageHandlers.tessera.postMessage({ action, generation: state?.generation ?? 0, ...extra }); }
function request(kind: MotionKind = 'update') { pending = kind; if (!inFlight) void pump(); }
async function pump() {
  if (inFlight || !state || !pending || disposed || !state.active || !state.ready || !engine.W || !engine.H) return;
  const kind = state.scanning ? 'instant' : pending; pending = null; inFlight = true;
  const capture = state; const serial = layoutSerial;
  try {
    const started = performance.now();
    const query = new URLSearchParams({ generation: String(capture.generation), view: String(capture.view), width: String(engine.W), height: String(engine.H), logical: String(capture.logical), force: String(appliedSerial !== serial) });
    const response = await fetch(`tessera://app/layout?${query}`);
    if(response.status===204)return;
    if (!response.ok) throw new Error(`layout ${response.status}`);
    const bytes = await response.arrayBuffer();
    if (serial !== layoutSerial || state.generation !== capture.generation || state.view !== capture.view || state.logical !== capture.logical) return;
    const t = performance.now(); const layout = decodeLayout(bytes);
    metrics.requests++; metrics.layoutBytes += bytes.byteLength;
    metrics.decodedMs.push(performance.now() - t); metrics.roundTripMs.push(t - started);
    if (metrics.decodedMs.length > 1000) { metrics.decodedMs.shift(); metrics.roundTripMs.shift(); }
    engine.apply(layout, kind);
    appliedSerial = serial;
    syncHover();
    message.textContent = layout.rects.length ? '' : capture.scanning ? (capture.lang === 'zh-CN' ? '正在发现文件…' : 'Discovering files…') : (capture.lang === 'zh-CN' ? '这里没有可显示的文件' : 'No files to display');
  } catch (error) { if (serial !== layoutSerial || state.generation !== capture.generation) return; const text = String(error); metrics.errors.push(text); post('error', { message: text }); }
  finally { inFlight = false; if (pending) void pump(); }
}
function resize() {
  if (engine.resize(innerWidth, innerHeight, devicePixelRatio)) {
    layoutSerial++;
    request('instant');
  }
}
const observer = new ResizeObserver(resize);
observer.observe(document.documentElement);
function watchScale() {
  matchMedia(`(resolution: ${devicePixelRatio}dppx)`).addEventListener('change', () => { resize(); watchScale(); }, { once: true });
}
watchScale(); resize();
function select(ids: number[]) { host.sel = new Set(ids); engine.kick(); post('select', { ids }); }
let hoverRequest: AbortController | null = null;
let hoverTimer: number | undefined;
let tipWidth = 0;
let tipHeight = 0;

function positionTip() {
  if (tip.hidden) return;
  const { x, y } = engine.mouse;
  tip.style.left = Math.max(0, Math.min(x + 12, innerWidth - tipWidth - 8)) + 'px';
  tip.style.top = Math.max(0, Math.min(y + 16, innerHeight - tipHeight - 8)) + 'px';
}

function showTip(name: string) {
  tip.textContent = name;
  tip.hidden = !name;
  if (name) {
    tipWidth = tip.offsetWidth;
    tipHeight = tip.offsetHeight;
    positionTip();
  }
}

function clearHover() {
  window.clearTimeout(hoverTimer);
  hoverRequest?.abort();
  hoverRequest = null;
  host.hoverId = null;
  showTip('');
}

function syncHover() {
  const hit = engine.mouse.in && appliedSerial === layoutSerial
    ? engine.hit(engine.mouse.x, engine.mouse.y) : null;
  const id = hit?.id ?? null;
  if (id === host.hoverId) {
    positionTip();
    return;
  }
  clearHover();
  host.hoverId = id;
  engine.kick();
  if (!hit || !state) return;
  const rect = hit.r;
  if (rect.kind === Kind.Group) showTip(tn('smallItems', rect.count));
  else if (rect.kind === Kind.Free) showTip(t('free'));
  else if (rect.kind === Kind.Unscanned) showTip(t('unscanned'));
  else if (rect.name) showTip(rect.name);
  else if (isNodeId(rect.id)) {
    const generation = state.generation;
    const request = new AbortController();
    hoverRequest = request;
    hoverTimer = window.setTimeout(async () => {
      try {
        const query = new URLSearchParams({ generation: String(generation), id: String(rect.id) });
        const response = await fetch(`tessera://app/tile-name?${query}`, { signal: request.signal });
        if (!response.ok) return;
        const name = await response.text();
        if (!request.signal.aborted && state?.generation === generation && host.hoverId === rect.id) {
          showTip(name);
        }
      } catch {
        // Hover is best effort and aborted when the pointer moves to another tile.
      }
    }, 60);
  }
}

canvas.addEventListener('pointermove', event => {
  engine.mouse.x = event.offsetX;
  engine.mouse.y = event.offsetY;
  engine.mouse.in = true;
  syncHover();
});
canvas.addEventListener('pointerleave', () => {
  engine.mouse.in = false;
  clearHover();
  engine.kick();
});
canvas.addEventListener('pointerdown', event=>{
  if (event.button !== 0 || appliedSerial !== layoutSerial) return; canvas.focus(); const hit=engine.hit(event.offsetX,event.offsetY);
  if(!hit || !isNodeId(hit.id)) {select([]);return;}
  const ids=new Set(host.sel);
  if(event.shiftKey && lastSelected!==null){
    const siblings=engine.drawList.filter(it=>it.r.parent===hit.r.parent && isNodeId(it.id));
    const a=siblings.findIndex(it=>it.id===lastSelected),b=siblings.findIndex(it=>it.id===hit.id);
    if(!event.metaKey)ids.clear();
    if(a>=0 && b>=0)for(const it of siblings.slice(Math.min(a,b),Math.max(a,b)+1))ids.add(it.id);else ids.add(hit.id);
  }else if(event.metaKey){if(ids.has(hit.id))ids.delete(hit.id);else ids.add(hit.id);}else {ids.clear();ids.add(hit.id);}
  lastSelected=hit.id;select([...ids]);
});
canvas.addEventListener('dblclick',e=>{if (appliedSerial !== layoutSerial) return; const hit=engine.hit(e.offsetX,e.offsetY);if(hit && hit.r.kind!==Kind.File)post('navigate',{id:hit.r.target});});
canvas.addEventListener('contextmenu',e=>{e.preventDefault();if (appliedSerial !== layoutSerial) return; const hit=engine.hit(e.offsetX,e.offsetY);if(hit && isNodeId(hit.id) && !host.sel.has(hit.id))select([hit.id]);post('context');});
canvas.addEventListener('keydown',e=>{
  if (e.metaKey || e.ctrlKey || e.altKey || appliedSerial !== layoutSerial) return;
  if(e.key==='Enter'){ const id=[...host.sel][0];const it=engine.items.get(id);if(it && it.r.kind!==Kind.File){post('navigate',{id:it.r.target});e.preventDefault();} }
  else if(e.key.startsWith('Arrow')){
    const next = engine.neighbor([...host.sel].at(-1), e.key as 'ArrowLeft', r => isNodeId(r.id));
    if (next) select([next.id]); e.preventDefault();
  }
  else if(e.key==='Escape')select([]);
});
const appearance=()=>{ if(!state)return;host.theme=makeTheme(state.scheme,{light:state.accent,dark:state.accent});host.reduced=state.reduced;document.documentElement.style.colorScheme=state.scheme;engine.painter.reset();engine.invalidate(); };
win.tessera={
  update(next: State){const old=state;state=next;host.sel=new Set(next.selection);host.filter=next.filter ?? null;
    if (old?.lang !== next.lang) setLang(next.lang);
    const changed=!old||old.generation!==next.generation||old.view!==next.view||old.logical!==next.logical;
    if (changed) {
      layoutSerial++;
      clearHover();
      lastSelected = null;
    }
    if (!old || old.generation !== next.generation) {
      engine.apply({ view: next.view, rev: next.rev, bytesPerPx: 0, rects: [] }, 'instant');
      message.textContent = next.scanning ? (next.lang === 'zh-CN' ? '正在发现文件…' : 'Discovering files…') : '';
    }
    if(!old || old.scheme!==next.scheme || old.accent!==next.accent || old.reduced!==next.reduced)appearance();
    if(changed || !old || old.rev!==next.rev || old.scanning!==next.scanning || (!old.ready && next.ready) || (!old.active&&next.active))request('instant');else engine.kick();
  },
  measure(){return { ...metrics, stats: {...engine.stats}, webgl:engine.stats.gpu, width:innerWidth,height:innerHeight,dpr:devicePixelRatio,userAgent:navigator.userAgent };},
  dispose(){disposed=true;layoutSerial++;clearHover();observer.disconnect();engine.destroy();},
};
engine.onStats = stats => post('render-status', { stats: { ...stats } });
post('ready');
