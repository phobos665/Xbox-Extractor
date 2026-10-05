import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api, formatBytes, onExtractUpdate, type JobState, type Settings } from "../api";

interface ExtractJob {
  id: number;
  iso: string;
  outDir: string;
  state: JobState;
  totalBytes: number | null;
  doneBytes: number | null;
  totalFiles: number | null;
  current: string | null;
  message: string | null;
}

const STATE_LABEL: Record<JobState, string> = {
  queued: "Queued",
  running: "Extracting",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
  skipped: "Skipped",
};

function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

function isIso(p: string): boolean {
  return /\.(x?iso)$/i.test(p);
}

interface Props {
  settings: Settings;
  onSettings: (s: Settings) => void;
}

export function ExtractTab({ settings, onSettings }: Props) {
  const [pending, setPending] = useState<string[]>([]);
  const [jobs, setJobs] = useState<ExtractJob[]>([]);
  const [dragging, setDragging] = useState(false);
  const [version, setVersion] = useState<string>("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api.extractXisoVersion().then(setVersion, (e) => setVersion(`extract-xiso unavailable: ${e}`));
    const un = onExtractUpdate((u) =>
      setJobs((prev) =>
        prev.map((j) =>
          j.id === u.id
            ? {
                ...j,
                state: u.state,
                totalBytes: u.totalBytes ?? j.totalBytes,
                doneBytes: u.doneBytes ?? j.doneBytes,
                totalFiles: u.totalFiles ?? j.totalFiles,
                current: u.current ?? j.current,
                message: u.message,
              }
            : j,
        ),
      ),
    );
    const drop = getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "over" || e.payload.type === "enter") setDragging(true);
      else if (e.payload.type === "leave") setDragging(false);
      else if (e.payload.type === "drop") {
        setDragging(false);
        add(e.payload.paths.filter(isIso));
      }
    });
    return () => {
      un.then((f) => f());
      drop.then((f) => f());
    };
  }, []);

  function add(paths: string[]) {
    setPending((prev) => [...prev, ...paths.filter((p) => !prev.includes(p))]);
  }

  async function chooseIsos() {
    const picked = await open({
      multiple: true,
      title: "Choose Xbox ISOs",
      filters: [{ name: "Xbox ISO", extensions: ["iso", "xiso"] }],
    });
    if (Array.isArray(picked)) add(picked);
    else if (typeof picked === "string") add([picked]);
  }

  async function chooseDest() {
    const dir = await open({ directory: true, defaultPath: settings.extractDir || undefined, title: "Extract into" });
    if (typeof dir === "string") onSettings({ ...settings, extractDir: dir });
  }

  async function start() {
    setError(null);
    try {
      const queued = await api.enqueueExtracts(pending, settings.extractDir, settings.skipSystemUpdate, settings.overwrite);
      setJobs((prev) => [
        ...prev,
        ...queued.map((q) => ({
          id: q.id,
          iso: q.iso,
          outDir: q.outDir,
          state: "queued" as const,
          totalBytes: null,
          doneBytes: null,
          totalFiles: null,
          current: null,
          message: null,
        })),
      ]);
      setPending([]);
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <div className={`extract${dragging ? " dragging" : ""}`}>
      <div className="extract-main">
        <section className="panel">
          <div className="panel-head">
            <h2>ISOs to extract</h2>
            <div className="spacer" />
            <button className="btn" onClick={chooseIsos}>
              Add ISOs…
            </button>
            {pending.length > 0 && (
              <button className="btn btn-ghost" onClick={() => setPending([])}>
                Clear
              </button>
            )}
          </div>
          {pending.length === 0 ? (
            <div className="dropzone muted">
              Drop .iso files here, or press <b>Add ISOs…</b>
            </div>
          ) : (
            <ul className="pending">
              {pending.map((p) => (
                <li key={p}>
                  <span className="ellipsis" title={p}>
                    {baseName(p)}
                  </span>
                  <span className="spacer" />
                  <button className="btn btn-small btn-ghost" onClick={() => setPending((x) => x.filter((y) => y !== p))}>
                    Remove
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="extract-options">
            <div className="outdir" title={settings.extractDir}>
              <span className="muted small">Extract into</span>
              <span className="mono small ellipsis">{settings.extractDir}</span>
              <button className="btn btn-small btn-ghost" onClick={chooseDest}>
                Change…
              </button>
            </div>
            <label className="check-row small">
              <input
                type="checkbox"
                checked={settings.skipSystemUpdate}
                onChange={(e) => onSettings({ ...settings, skipSystemUpdate: e.target.checked })}
              />
              Skip <code>$SystemUpdate</code>
            </label>
            <div className="spacer" />
            <button className="btn btn-primary" disabled={pending.length === 0} onClick={start}>
              Extract {pending.length || ""}
            </button>
          </div>
          {error && <div className="small" style={{ color: "var(--red)" }}>{error}</div>}
          <div className="muted small">
            Each ISO goes into its own folder named after the file. Uses {version || "extract-xiso"}.
          </div>
        </section>

        <section className="panel">
          <div className="panel-head">
            <h2>Extractions</h2>
            <div className="spacer" />
            {jobs.some((j) => !["queued", "running"].includes(j.state)) && (
              <button
                className="btn btn-small btn-ghost"
                onClick={() => setJobs((prev) => prev.filter((j) => j.state === "queued" || j.state === "running"))}
              >
                Clear finished
              </button>
            )}
          </div>
          {jobs.length === 0 && <div className="muted small">Nothing extracted yet.</div>}
          <ul className="extract-jobs">
            {jobs.map((j) => {
              const frac = j.state === "done" ? 1 : j.totalBytes ? (j.doneBytes ?? 0) / j.totalBytes : 0;
              return (
                <li key={j.id} className={`job job-${j.state}`}>
                  <div className="job-top">
                    <span className="job-name" title={j.iso}>
                      {baseName(j.iso)}
                    </span>
                    <span className={`tag state-${j.state}`}>{STATE_LABEL[j.state]}</span>
                  </div>
                  {(j.state === "running" || j.state === "done") && (
                    <div className="bar">
                      <div style={{ width: `${(frac * 100).toFixed(1)}%` }} />
                    </div>
                  )}
                  <div className="job-bottom">
                    <span className="small muted ellipsis" title={j.message ?? j.current ?? ""}>
                      {j.state === "running" && j.totalBytes != null
                        ? `${formatBytes(j.doneBytes ?? 0)} of ${formatBytes(j.totalBytes)}${j.current ? ` · ${j.current}` : ""}`
                        : j.state === "done"
                          ? `${j.totalFiles ?? ""} files, ${formatBytes(j.totalBytes)}${j.message ? ` · ${j.message}` : ""}`
                          : (j.message ?? "")}
                    </span>
                    <span className="spacer" />
                    {(j.state === "queued" || j.state === "running") && (
                      <button className="btn btn-small btn-ghost" onClick={() => api.cancelExtract(j.id)}>
                        Cancel
                      </button>
                    )}
                    {(j.state === "done" || j.state === "skipped") && (
                      <button className="btn btn-small btn-ghost" onClick={() => revealItemInDir(j.outDir)}>
                        Show
                      </button>
                    )}
                  </div>
                </li>
              );
            })}
          </ul>
        </section>
      </div>
      <footer className="credits muted small">
        Extraction is done by extract-xiso, bundled unmodified. This product includes software developed by in
        &lt;in@fishtank.com&gt;.
      </footer>
    </div>
  );
}
