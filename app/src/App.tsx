import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, formatBytes, onJobUpdate, onScanEvent, type ConnectionInfo, type Settings, type Title } from "./api";
import { ConnectionBar, type ConnStatus } from "./components/ConnectionBar";
import { Library, type ViewMode } from "./components/Library";
import { QueuePanel, type JobView } from "./components/QueuePanel";
import { SettingsDialog } from "./components/SettingsDialog";
import { ExtractTab } from "./components/ExtractTab";
import "./App.css";

type SortKey = "name" | "size" | "path";
type Tab = "xbox" | "extract";

export default function App() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [conn, setConn] = useState<ConnectionInfo>({ host: "", port: 21, user: "xbox", password: "xbox" });
  const [status, setStatus] = useState<ConnStatus>({ kind: "idle" });
  const [titles, setTitles] = useState<Title[]>([]);
  const [covers, setCovers] = useState<Record<string, string | null>>({});
  const [sizes, setSizes] = useState<Record<string, { bytes: number; files: number }>>({});
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [scanning, setScanning] = useState(false);
  const [scanNote, setScanNote] = useState("");
  const [scanError, setScanError] = useState<string | null>(null);
  const [jobs, setJobs] = useState<JobView[]>([]);
  const [filter, setFilter] = useState("");
  const [sort, setSort] = useState<SortKey>("name");
  const [view, setView] = useState<ViewMode>("grid");
  const [showSettings, setShowSettings] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("xbox");
  const generation = useRef(0);

  useEffect(() => {
    api.getSettings().then((s) => {
      setSettings(s);
      setConn(s.connection);
    });
  }, []);

  useEffect(() => {
    const un = onJobUpdate((u) =>
      setJobs((prev) =>
        prev.map((j) =>
          j.id === u.id
            ? {
                ...j,
                state: u.state,
                progress: u.progress ?? (u.state === "running" ? j.progress : u.progress),
                rate: u.rate ?? j.rate,
                message: u.message,
                outPath: u.outPath ?? j.outPath,
              }
            : j,
        ),
      ),
    );
    return () => {
      un.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const un = onScanEvent((e) => {
      if (e.kind === "visiting") setScanNote(`Looking in ${e.path}`);
      if (e.kind === "found") setTitles((prev) => (prev.some((t) => t.path === e.title.path) ? prev : [...prev, e.title]));
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  const persist = useCallback(
    (s: Settings) => {
      setSettings(s);
      api.saveSettings(s).catch((e) => setNotice(String(e)));
    },
    [setSettings],
  );

  /** Fetches covers, then folder sizes, one at a time behind the scan. A rescan abandons it. */
  async function fillDetails(list: Title[], gen: number) {
    for (const t of list) {
      if (generation.current !== gen) return;
      const img = await api.titleImage(t.path).catch(() => null);
      setCovers((c) => ({ ...c, [t.path]: img }));
    }
    for (const t of list) {
      if (generation.current !== gen) return;
      if (t.sizeBytes != null) continue;
      try {
        const s = await api.titleSize(t.path);
        setSizes((prev) => ({ ...prev, [t.path]: s }));
      } catch {
        /* a folder that will not list stays "Measuring" */
      }
    }
  }

  /** A quick scan reuses titles whose XBE has not changed size since the last scan of this
   * console; a full one reads every XBE again. */
  async function scan(s: Settings | null = settings, full = false) {
    if (!s) return;
    const gen = ++generation.current;
    setScanning(true);
    setScanError(null);
    setTitles([]);
    setCovers({});
    setSizes({});
    setSelected(new Set());
    try {
      const found = await api.scan(s.roots, s.depth, full);
      if (generation.current !== gen) return;
      setTitles(found);
      const known: Record<string, { bytes: number; files: number }> = {};
      for (const t of found) if (t.sizeBytes != null) known[t.path] = { bytes: t.sizeBytes, files: t.fileCount ?? 0 };
      setSizes(known);
      const reused = found.filter((t) => t.cached).length;
      setScanNote(
        `${found.length} title${found.length === 1 ? "" : "s"}` +
          (reused > 0 ? ` · ${reused} unchanged since the last scan` : ""),
      );
      fillDetails(found, gen);
    } catch (e) {
      setScanError(String(e));
      setScanNote("");
    } finally {
      setScanning(false);
    }
  }

  async function connect() {
    setStatus({ kind: "connecting" });
    try {
      const r = await api.connect(conn);
      setStatus({ kind: "connected", server: r.server });
      const s = await api.getSettings();
      setSettings(s);
      scan(s);
    } catch (e) {
      setStatus({ kind: "error", message: String(e) });
    }
  }

  async function disconnect() {
    generation.current++;
    await api.disconnect();
    setStatus({ kind: "idle" });
  }

  async function chooseOutput() {
    if (!settings) return;
    const dir = await open({ directory: true, defaultPath: settings.outputDir || undefined, title: "Save ISOs to" });
    if (typeof dir === "string") persist({ ...settings, outputDir: dir });
  }

  async function createIsos() {
    if (!settings) return;
    const picked = titles.filter((t) => selected.has(t.path));
    if (picked.length === 0) return;
    try {
      const mode = settings.outputMode;
      const queued = await api.enqueue(
        picked.map((t) => ({ path: t.path, isoName: t.isoName })),
        settings.outputDir,
        mode,
        settings.overwrite,
      );
      const byPath = new Map(picked.map((t) => [t.path, t]));
      setJobs((prev) => [
        ...prev,
        ...queued.map((q) => ({
          id: q.id,
          path: q.path,
          name: byPath.get(q.path)?.titleName ?? q.path,
          mode,
          outPath: q.outPath,
          state: "queued" as const,
          progress: null,
          rate: null,
          message: null,
        })),
      ]);
      setSelected(new Set());
    } catch (e) {
      setNotice(String(e));
    }
  }

  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const list = q
      ? titles.filter(
          (t) =>
            t.titleName.toLowerCase().includes(q) ||
            t.path.toLowerCase().includes(q) ||
            (t.titleCode ?? "").toLowerCase().includes(q) ||
            t.titleId.toLowerCase().includes(q),
        )
      : [...titles];
    list.sort((a, b) => {
      if (sort === "size") return (sizes[b.path]?.bytes ?? -1) - (sizes[a.path]?.bytes ?? -1);
      if (sort === "path") return a.path.localeCompare(b.path);
      return a.titleName.localeCompare(b.titleName, undefined, { sensitivity: "base" });
    });
    return list;
  }, [titles, filter, sort, sizes]);

  const queuedPaths = useMemo(
    () => new Set(jobs.filter((j) => j.state === "queued" || j.state === "running").map((j) => j.path)),
    [jobs],
  );

  const selectedBytes = useMemo(() => {
    let total = 0;
    let known = true;
    for (const p of selected) {
      if (sizes[p]) total += sizes[p].bytes;
      else known = false;
    }
    return { total, known };
  }, [selected, sizes]);

  function toggle(path: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  const connected = status.kind === "connected";

  return (
    <div className="app">
      <ConnectionBar
        info={conn}
        recentHosts={settings?.recentHosts ?? []}
        status={status}
        onChange={setConn}
        onConnect={connect}
        onDisconnect={disconnect}
        onOpenSettings={() => setShowSettings(true)}
      />

      <nav className="tabs">
        <button className={tab === "xbox" ? "on" : ""} onClick={() => setTab("xbox")}>
          Xbox to ISO
        </button>
        <button className={tab === "extract" ? "on" : ""} onClick={() => setTab("extract")}>
          Extract ISOs
        </button>
      </nav>

      {tab === "extract" && settings && <ExtractTab settings={settings} onSettings={persist} />}

      <div className="body" hidden={tab !== "xbox"}>
        <main className="library">
          <div className="toolbar">
            <button
              className="btn"
              disabled={!connected || scanning}
              onClick={() => scan()}
              title="Re-list the drives; titles whose XBE is unchanged are not read again"
            >
              {scanning ? <span className="spinner" /> : null}
              {scanning ? "Scanning" : "Rescan"}
            </button>
            <button
              className="btn btn-ghost"
              disabled={!connected || scanning}
              onClick={() => scan(settings, true)}
              title="Read every title's XBE again"
            >
              Full rescan
            </button>
            <input
              className="search"
              placeholder="Filter titles"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
            <select value={sort} onChange={(e) => setSort(e.target.value as SortKey)} title="Sort">
              <option value="name">Name</option>
              <option value="size">Size</option>
              <option value="path">Folder</option>
            </select>
            <div className="seg">
              <button className={view === "grid" ? "on" : ""} onClick={() => setView("grid")}>
                Covers
              </button>
              <button className={view === "list" ? "on" : ""} onClick={() => setView("list")}>
                List
              </button>
            </div>
            <div className="spacer" />
            <button
              className="btn btn-ghost"
              disabled={visible.length === 0}
              onClick={() =>
                setSelected((prev) =>
                  visible.every((t) => prev.has(t.path)) ? new Set() : new Set(visible.map((t) => t.path)),
                )
              }
            >
              {visible.length > 0 && visible.every((t) => selected.has(t.path)) ? "Select none" : "Select all"}
            </button>
          </div>

          <div className="scroll">
            {!connected && titles.length === 0 && (
              <div className="empty">
                <h1>Connect to your Xbox</h1>
                <p className="muted">
                  Enter its IP address, or press <b>Find Xbox</b> to search this network. The dashboard's FTP server
                  must be running (UnleashX, EvolutionX, Avalaunch and XBMC all have one).
                </p>
              </div>
            )}
            {connected && titles.length === 0 && !scanning && !scanError && (
              <div className="empty">
                <h1>No titles found</h1>
                <p className="muted">Check where the scan looks in Settings, then rescan.</p>
              </div>
            )}
            {scanError && (
              <div className="empty">
                <h1>The scan stopped</h1>
                <p className="muted">{scanError}</p>
              </div>
            )}
            <Library
              titles={visible}
              covers={covers}
              sizes={sizes}
              selected={selected}
              queued={queuedPaths}
              view={view}
              onToggle={toggle}
            />
          </div>

          <footer className="actionbar">
            <span className="muted small ellipsis">{scanNote}</span>
            <div className="spacer" />
            <div
              className="seg"
              title="ISO builds one XISO image per game. Files copies the game folder as it is on the Xbox, which is what xboxrecomp needs."
            >
              <button
                className={settings?.outputMode !== "files" ? "on" : ""}
                onClick={() => settings && persist({ ...settings, outputMode: "iso" })}
              >
                ISO
              </button>
              <button
                className={settings?.outputMode === "files" ? "on" : ""}
                onClick={() => settings && persist({ ...settings, outputMode: "files" })}
              >
                Files
              </button>
            </div>
            <div className="outdir" title={settings?.outputDir}>
              <span className="muted small">Save to</span>
              <span className="mono small ellipsis">{settings?.outputDir}</span>
              <button className="btn btn-small btn-ghost" onClick={chooseOutput}>
                Change…
              </button>
            </div>
            <button className="btn btn-primary" disabled={selected.size === 0 || !connected} onClick={createIsos}>
              {settings?.outputMode === "files"
                ? `Copy ${selected.size || ""} game${selected.size === 1 ? "" : "s"}`
                : `Create ${selected.size || ""} ISO${selected.size === 1 ? "" : "s"}`}
              {selected.size > 0 && selectedBytes.total > 0 && (
                <span className="btn-sub">
                  {selectedBytes.known ? "" : "≥ "}
                  {formatBytes(selectedBytes.total)}
                </span>
              )}
            </button>
          </footer>
        </main>

        <QueuePanel
          jobs={jobs}
          onClearFinished={() => setJobs((prev) => prev.filter((j) => j.state === "queued" || j.state === "running"))}
        />
      </div>

      {notice && (
        <div className="toast" onClick={() => setNotice(null)}>
          {notice}
        </div>
      )}

      {showSettings && settings && (
        <SettingsDialog
          settings={settings}
          onClose={() => setShowSettings(false)}
          onSave={(s) => {
            persist(s);
            setShowSettings(false);
          }}
        />
      )}
    </div>
  );
}
