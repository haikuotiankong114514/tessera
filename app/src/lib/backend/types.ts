/**
 * The frontend's view of the Rust core. Mirrors docs/ipc.md; the mock backend and
 * the Tauri backend both implement `Backend`.
 */

export type SizeMode = 'disk' | 'logical';

export interface PlatformInfo {
  /** true when the process runs elevated (MFT path available) */
  elevated: boolean;
  /** the window backdrop that Rust managed to apply */
  backdrop: 'mica' | 'none';
  /** system accent colour, tuned per theme as Windows does (AccentDark1 / AccentLight2) */
  accent: { light: string; dark: string } | null;
  version: string;
  /** true when tessera-core serves the commands; false means the mock tree is used */
  core: boolean;
  /** the UI runs inside the Tauri window (not a plain browser preview) */
  native: boolean;
  /** UI language, picked from the Windows preferred languages (see lib/i18n.svelte.ts) */
  lang: string;
  /** BCP 47 tag for dates and numbers, e.g. en-GB */
  locale: string;
  /** Optional macOS comparison-build scan root. */
  benchmarkRoot?: string | null;
}

export interface Volume {
  root: string;
  label: string;
  fs: string;
  capacity: number;
  free: number;
}

export interface LayoutStyle {
  outer: number;
  pad: number;
  gap: number;
  header: number;
  minHeaderW: number;
  minArea: number;
  recurse: number;
  openShare: number;
  maxDepth: number;
  maxPerDir: number;
  labelMinW: number;
  labelMinH: number;
}

export interface LayoutRequest {
  view: number;
  width: number;
  height: number;
  sizeMode: SizeMode;
  style: LayoutStyle;
}

export type DetailsKind = 'file' | 'dir' | 'group' | 'free' | 'unscanned';

export interface Details {
  id: number;
  kind: DetailsKind;
  name: string;
  path: string;
  size: number;
  disk: number;
  logical: number;
  share: number;
  files: number;
  dirs: number;
  modified: number | null;
  cat: number;
  flags: number;
  lockReason: string | null;
  hint: string | null;
}

export interface Crumb {
  id: number;
  name: string;
}

export interface NavInfo {
  crumbs: Crumb[];
  up: number | null;
}

export interface CatStat {
  cat: number;
  bytes: number;
}

export interface DeleteResult {
  ok: number[];
  failed: { id: number; reason: string }[];
  bytes: number;
  undo: number | null;
  rev: number;
}

export interface ScanStarted {
  scanId: number;
  rootId: number;
}

export interface ScanProgress {
  scanId: number;
  files: number;
  dirs: number;
  /** bytes found so far */
  scanned: number;
  /** the volume's used bytes, known before the scan starts */
  used: number;
  elapsedMs: number;
  done: boolean;
  rev: number;
  errors: number;
}

export interface TreeChanged {
  rev: number;
  bytesDelta: number;
}

export interface BackendEvents {
  'scan-progress': ScanProgress;
  'tree-changed': TreeChanged;
}

export type Unlisten = () => void;

export interface Backend {
  readonly platform: PlatformInfo;
  listVolumes(): Promise<Volume[]>;
  scanStart(root: string): Promise<ScanStarted>;
  scanCancel(): Promise<void>;
  /** raw layout buffer, decode with `decodeLayout` */
  layout(req: LayoutRequest): Promise<ArrayBuffer>;
  details(id: number, view: number, sizeMode: SizeMode): Promise<Details | null>;
  nav(id: number): Promise<NavInfo>;
  catStats(view: number, sizeMode: SizeMode): Promise<CatStat[]>;
  recycle(ids: number[]): Promise<DeleteResult>;
  restore(token: number): Promise<DeleteResult>;
  purge(ids: number[]): Promise<DeleteResult>;
  emptyRecycleBin(root: string): Promise<DeleteResult>;
  reveal(id: number): Promise<void>;
  relaunchElevated(): Promise<void>;
  on<K extends keyof BackendEvents>(event: K, cb: (payload: BackendEvents[K]) => void): Promise<Unlisten>;
  /** preview-only helpers (live-update demo); absent on the real backend */
  demo?: { startDownload(): boolean };
}
