// Typed wrappers over the Tauri commands and events in src-tauri/src/lib.rs.
// The shapes mirror the serde output of the Rust types they name.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ConnectionInfo {
  host: string;
  port: number;
  user: string;
  password: string;
}

/** What a job makes from a game folder: an XISO, or the folder's files as they are. */
export type OutputMode = "iso" | "files";

export interface Settings {
  connection: ConnectionInfo;
  recentHosts: string[];
  outputDir: string;
  outputMode: OutputMode;
  roots: string[];
  depth: number;
  overwrite: boolean;
  extractDir: string;
  skipSystemUpdate: boolean;
}

/** xib_core::scan::Title */
export interface Title {
  path: string;
  folder: string;
  xbeFile: string;
  xbeSize: number;
  titleName: string;
  titleId: string;
  titleCode: string | null;
  region: string;
  discNumber: number;
  version: number;
  imageSection: [number, number] | null;
  isoName: string;
  error: string | null;
  sizeBytes: number | null;
  fileCount: number | null;
  /** Reused from the last scan because the XBE had not changed size. */
  cached: boolean;
}

export interface Found {
  host: string;
  port: number;
  banner: string;
  looksLikeXbox: boolean;
}

export interface Connected {
  server: string | null;
  drives: string[];
}

export type ScanEvent =
  | { kind: "visiting"; path: string }
  | { kind: "found"; title: Title }
  | { kind: "rootMissing"; root: string }
  | { kind: "skipped"; path: string; reason: string };

export type IsoProgress =
  | { kind: "listing"; dirs: number; files: number; bytes: number }
  | { kind: "copying"; file: string; done: number; total: number }
  | { kind: "finishing" }
  | { kind: "done"; bytes: number };

export type JobState = "queued" | "running" | "done" | "failed" | "cancelled" | "skipped";

export interface JobUpdate {
  id: number;
  state: JobState;
  progress: IsoProgress | null;
  rate: number | null;
  message: string | null;
  outPath: string | null;
}

export interface ExtractUpdate {
  id: number;
  state: JobState;
  totalBytes: number | null;
  doneBytes: number | null;
  totalFiles: number | null;
  current: string | null;
  message: string | null;
  outDir: string | null;
}

export interface QueuedExtract {
  id: number;
  iso: string;
  outDir: string;
}

export interface QueuedJob {
  id: number;
  path: string;
  outPath: string;
}

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  discover: () => invoke<Found[]>("discover_xboxes"),
  connect: (info: ConnectionInfo) => invoke<Connected>("connect", { info }),
  disconnect: () => invoke<void>("disconnect"),
  scan: (roots: string[], depth: number, full = false) =>
    invoke<Title[]>("scan_titles", { request: { roots, depth, full } }),
  titleImage: (path: string) => invoke<string | null>("title_image", { path }),
  titleSize: (path: string) => invoke<{ bytes: number; files: number }>("title_size", { path }),
  enqueue: (requests: { path: string; isoName: string }[], outputDir: string, mode: OutputMode, overwrite: boolean) =>
    invoke<QueuedJob[]>("enqueue_jobs", { requests, outputDir, mode, overwrite }),
  cancelJob: (id: number) => invoke<void>("cancel_job", { id }),
  cancelAll: () => invoke<void>("cancel_all_jobs"),
  enqueueExtracts: (isos: string[], destDir: string, skipSystemUpdate: boolean, overwrite: boolean) =>
    invoke<QueuedExtract[]>("enqueue_extracts", { isos, destDir, skipSystemUpdate, overwrite }),
  cancelExtract: (id: number) => invoke<void>("cancel_extract", { id }),
  extractXisoVersion: () => invoke<string>("sidecar_version"),
};

export function onExtractUpdate(f: (u: ExtractUpdate) => void): Promise<UnlistenFn> {
  return listen<ExtractUpdate>("extract://update", (e) => f(e.payload));
}

export function onScanEvent(f: (e: ScanEvent) => void): Promise<UnlistenFn> {
  return listen<ScanEvent>("scan://event", (e) => f(e.payload));
}

export function onJobUpdate(f: (u: JobUpdate) => void): Promise<UnlistenFn> {
  return listen<JobUpdate>("job://update", (e) => f(e.payload));
}

export function onDiscovered(f: (x: Found) => void): Promise<UnlistenFn> {
  return listen<Found>("discover://found", (e) => f(e.payload));
}

export function formatBytes(n: number | undefined | null): string {
  if (n == null) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`;
}

export function formatDuration(seconds: number): string {
  if (!isFinite(seconds) || seconds < 0) return "";
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${r.toString().padStart(2, "0")}s`;
  return `${r}s`;
}
