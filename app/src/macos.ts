/** Native-shell adapter. Rendering, animation, hit testing, batching, caches and
 * the binary codec are the upstream implementation, not a parallel renderer. */
import { TreemapEngine, type EngineHost, type MotionKind } from './lib/treemap/engine';
import { FluentPainter } from './lib/treemap/paint';
import { fluentMotion, fluentLayout } from './lib/treemap/motion';
import { makeTheme } from './lib/theme';
import { decodeLayout, isNodeId, Kind } from './lib/backend/codec';
import { setLang } from './lib/i18n.svelte';

type State = { generation: number; view: number; logical: boolean; scanning: boolean; rev: number;
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
let lastSelected: number | null = null;
const metrics = { requests: 0, layoutBytes: 0, decodedMs: [] as number[], roundTripMs: [] as number[], errors: [] as string[] };
function post(action: string, extra: object = {}) { win.webkit?.messageHandlers.tessera.postMessage({ action, generation: state?.generation ?? 0, ...extra }); }
function request(kind: MotionKind = 'update') { pending = kind; if (!inFlight) void pump(); }
async function pump() {
  if (inFlight || !state || !pending || disposed || !state.active) return;
  const kind = pending; pending = null; inFlight = true;
  const capture = state; const serial = layoutSerial;
  try {
    const started = performance.now();
    const query = new URLSearchParams({ generation: String(capture.generation), view: String(capture.view), width: String(engine.W), height: String(engine.H), logical: String(capture.logical), force: String(metrics.requests === 0) });
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
    message.textContent = layout.rects.length ? '' : capture.scanning ? (capture.lang === 'zh-CN' ? '正在发现文件…' : 'Discovering files…') : (capture.lang === 'zh-CN' ? '这里没有可显示的文件' : 'No files to display');
  } catch (error) { if (serial !== layoutSerial || state.generation !== capture.generation) return; const text = String(error); metrics.errors.push(text); post('error', { message: text }); }
  finally { inFlight = false; if (pending) void pump(); }
}
function resize() { if (engine.resize(innerWidth, innerHeight, devicePixelRatio)) request('update'); }
const observer = new ResizeObserver(resize);
observer.observe(document.documentElement);
function watchScale() {
  matchMedia(`(resolution: ${devicePixelRatio}dppx)`).addEventListener('change', () => { resize(); watchScale(); }, { once: true });
}
watchScale(); resize();
function select(ids: number[]) { host.sel = new Set(ids); engine.kick(); post('select', { ids }); }
canvas.addEventListener('pointermove', event => {
  const { offsetX: x, offsetY: y } = event; const hit = engine.hit(x,y);
  engine.mouse.x=x; engine.mouse.y=y; engine.mouse.in=true; engine.mouse.moved=true;
  const id = hit?.id ?? null;
  if (host.hoverId !== id) { host.hoverId = id; tip.textContent = hit?.r.name ?? ''; }
  if (hit?.r.name) { tip.style.display='block'; tip.style.left=Math.min(x+12,innerWidth-200)+'px'; tip.style.top=Math.min(y+16,innerHeight-32)+'px'; } else tip.style.display='none';
  engine.kick();
});
canvas.addEventListener('pointerleave',()=>{ host.hoverId=null; engine.mouse.in=false; tip.style.display='none'; engine.kick(); });
canvas.addEventListener('pointerdown', event=>{
  if(event.button!==0)return; canvas.focus(); const hit=engine.hit(event.offsetX,event.offsetY);
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
canvas.addEventListener('dblclick',e=>{const hit=engine.hit(e.offsetX,e.offsetY);if(hit && hit.r.kind!==Kind.File)post('navigate',{id:hit.r.target});});
canvas.addEventListener('contextmenu',e=>{e.preventDefault();const hit=engine.hit(e.offsetX,e.offsetY);if(hit && isNodeId(hit.id) && !host.sel.has(hit.id))select([hit.id]);post('context');});
canvas.addEventListener('keydown',e=>{
  if(e.metaKey || e.ctrlKey || e.altKey)return;
  if(e.key==='Enter'){ const id=[...host.sel][0];const it=engine.items.get(id);if(it && it.r.kind!==Kind.File){post('navigate',{id:it.r.target});e.preventDefault();} }
  else if(e.key.startsWith('Arrow')){
    const next = engine.neighbor([...host.sel].at(-1), e.key as 'ArrowLeft', r => isNodeId(r.id));
    if (next) select([next.id]); e.preventDefault();
  }
  else if(e.key==='Escape')select([]);
});
const appearance=()=>{ if(!state)return;host.theme=makeTheme(state.scheme,{light:state.accent,dark:state.accent});host.reduced=state.reduced;document.documentElement.style.colorScheme=state.scheme;engine.painter.reset();engine.invalidate(); };
win.tessera={
  update(next: State){const old=state;state=next;host.scanning=next.scanning && next.active;host.sel=new Set(next.selection);host.filter=next.filter ?? null;
    setLang(next.lang);const changed=!old||old.generation!==next.generation||old.view!==next.view||old.logical!==next.logical;
    if(changed)layoutSerial++;
    if(!old || old.scheme!==next.scheme || old.accent!==next.accent || old.reduced!==next.reduced)appearance();
    if(changed || !old || old.rev!==next.rev || old.scanning!==next.scanning || (!old.active&&next.active))request(changed?'instant':'live');else engine.kick();
  },
  measure(){return { ...metrics, stats: {...engine.stats}, webgl:engine.stats.gpu, width:innerWidth,height:innerHeight,dpr:devicePixelRatio,userAgent:navigator.userAgent };},
  dispose(){disposed=true;layoutSerial++;observer.disconnect();engine.destroy();},
};
post('ready');
