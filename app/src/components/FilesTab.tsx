import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  api,
  formatBytes,
  joinRemote,
  onJobUpdate,
  parentRemote,
  type JobState,
  type JobUpdate,
  type QueuedJob,
  type RemoteEntry,
  type Settings,
} from "../api";
import { QueuePanel, type JobView } from "./QueuePanel";

type SortKey = "name" | "size";
type Edit = { kind: "new"; value: string } | { kind: "rename"; from: string; value: string };

/** The longest name a FATX drive stores. */
const FATX_MAX_NAME = 42;
const BAD_CHARS = /[/\\:*?"<>|\x00-\x1f]/;
const FINISHED: JobState[] = ["done", "failed", "cancelled", "skipped"];

function baseName(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

function nameProblem(name: string): string | null {
  if (!name || name === "." || name === "..") return "Enter a name";
  if (name.length > FATX_MAX_NAME) return `Names on the Xbox are limited to ${FATX_MAX_NAME} characters`;
  if (BAD_CHARS.test(name)) return 'Names on the Xbox cannot contain / \\ : * ? " < > |';
  return null;
}

function applyUpdate(j: JobView, u: JobUpdate): JobView {
  return {
    ...j,
    state: u.state,
    progress: u.progress ?? (u.state === "running" ? j.progress : u.progress),
    rate: u.rate ?? j.rate,
    message: u.message,
    outPath: u.outPath ?? j.outPath,
  };
}

function Icon({ kind }: { kind: "drive" | "folder" | "file" }) {
  const d = {
    drive: "M3 6h18v12H3z M7 14h.01 M11 14h6",
    folder: "M3 6h6l2 2h10v10H3z",
    file: "M6 3h8l4 4v14H6z M14 3v4h4",
  }[kind];
  return (
    <svg className={`ficon ficon-${kind}`} viewBox="0 0 24 24" width="18" height="18" aria-hidden>
      <path d={d} fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

interface Props {
  connected: boolean;
  /** This tab is showing; drops elsewhere in the app are not uploads. */
  active: boolean;
  settings: Settings;
}

export function FilesTab({ connected, active, settings }: Props) {
  const [path, setPath] = useState("/");
  const [entries, setEntries] = useState<RemoteEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [anchor, setAnchor] = useState<string | null>(null);
  const [edit, setEdit] = useState<Edit | null>(null);
  const [busy, setBusy] = useState(false);
  const [sort, setSort] = useState<SortKey>("name");
  const [jobs, setJobs] = useState<JobView[]>([]);
  const [dragging, setDragging] = useState(false);
  const [downloadDir, setDownloadDir] = useState("");

  const generation = useRef(0);
  // The drop and job listeners are registered once, so they read these through refs.
  const pathRef = useRef(path);
  pathRef.current = path;
  const entriesRef = useRef(entries);
  entriesRef.current = entries;
  const canDrop = useRef(false);
  canDrop.current = active && connected && path !== "/";
  /** Jobs this tab queued, by id. */
  const mine = useRef(new Map<number, JobView>());
  /** Updates that arrived before the enqueue call returned the job's id. */
  const early = useRef(new Map<number, JobUpdate>());

  const atRoot = path === "/";

  /** Lists `p` and shows it. A folder that will not open leaves the current one showing. */
  async function list(p: string) {
    const gen = ++generation.current;
    setLoading(true);
    setError(null);
    try {
      const found = await api.browse(p);
      if (generation.current !== gen) return;
      setPath(p);
      setEntries(found);
      setSelected(new Set());
      setAnchor(null);
      setEdit(null);
    } catch (e) {
      if (generation.current === gen) setError(String(e));
    } finally {
      if (generation.current === gen) setLoading(false);
    }
  }

  function uploadFinished(outPath: string) {
    if (parentRemote(outPath) === pathRef.current) list(pathRef.current);
  }

  useEffect(() => {
    if (connected) {
      list("/");
    } else {
      generation.current++;
      setPath("/");
      setEntries(null);
      setLoading(false);
      setError(null);
    }
  }, [connected]);

  useEffect(() => {
    const un = onJobUpdate((u) => {
      const j = mine.current.get(u.id);
      if (!j) {
        early.current.set(u.id, u);
        return;
      }
      setJobs((prev) => prev.map((x) => (x.id === u.id ? applyUpdate(x, u) : x)));
      if (j.mode === "upload" && FINISHED.includes(u.state)) uploadFinished(j.outPath);
    });
    const drop = getCurrentWebview().onDragDropEvent((e) => {
      if (!canDrop.current) {
        setDragging(false);
        return;
      }
      if (e.payload.type === "over" || e.payload.type === "enter") setDragging(true);
      else if (e.payload.type === "leave") setDragging(false);
      else if (e.payload.type === "drop") {
        setDragging(false);
        upload(e.payload.paths);
      }
    });
    return () => {
      un.then((f) => f());
      drop.then((f) => f());
    };
  }, []);

  function addJobs(queued: QueuedJob[], mode: "upload" | "download") {
    const added: JobView[] = queued.map((q) => {
      let j: JobView = {
        id: q.id,
        path: q.path,
        name: baseName(q.path),
        mode,
        outPath: q.outPath,
        state: "queued",
        progress: null,
        rate: null,
        message: null,
      };
      const u = early.current.get(q.id);
      if (u) {
        early.current.delete(q.id);
        j = applyUpdate(j, u);
      }
      mine.current.set(q.id, j);
      return j;
    });
    setJobs((prev) => [...prev, ...added]);
    const landed = added.find((j) => j.mode === "upload" && FINISHED.includes(j.state));
    if (landed) uploadFinished(landed.outPath);
  }

  const sorted = useMemo(() => {
    const list = [...(entries ?? [])];
    list.sort((a, b) => {
      if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
      if (sort === "size" && a.size !== b.size) return b.size - a.size;
      return a.name.localeCompare(b.name, undefined, { sensitivity: "base", numeric: true });
    });
    return list;
  }, [entries, sort]);

  const picked = sorted.filter((e) => selected.has(e.name));
  const pickedBytes = picked.reduce((n, e) => n + (e.isDir ? 0 : e.size), 0);

  function click(e: MouseEvent, name: string) {
    if (e.shiftKey && anchor) {
      const names = sorted.map((x) => x.name);
      const [a, b] = [names.indexOf(anchor), names.indexOf(name)].sort((x, y) => x - y);
      if (a >= 0) {
        setSelected(new Set(names.slice(a, b + 1)));
        return;
      }
    }
    if (e.metaKey || e.ctrlKey) toggle(name);
    else setSelected(new Set([name]));
    setAnchor(name);
  }

  function toggle(name: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
    setAnchor(name);
  }

  async function upload(paths: string[]) {
    const dir = pathRef.current;
    if (paths.length === 0 || dir === "/") return;
    const here = new Set((entriesRef.current ?? []).map((e) => e.name.toLowerCase()));
    const clashes = paths.map(baseName).filter((n) => here.has(n.toLowerCase()));
    if (clashes.length > 0) {
      const what = clashes.length === 1 ? `"${clashes[0]}" is` : `${clashes.length} of these are`;
      const ok = await confirm(
        `${what} already in ${dir}. Files with the same name will be replaced, and folders merged into.`,
        { title: "Replace on the Xbox?", kind: "warning", okLabel: "Replace", cancelLabel: "Cancel" },
      );
      if (!ok) return;
    }
    try {
      const queued = await api.enqueueTransfers(
        paths.map((p) => ({ kind: "upload", local: p, remoteDir: dir })),
        true,
      );
      addJobs(queued, "upload");
    } catch (e) {
      setError(String(e));
    }
  }

  async function chooseUpload(directory: boolean) {
    const p = await open({ multiple: true, directory, title: `Upload to ${path}` });
    if (Array.isArray(p)) upload(p);
    else if (typeof p === "string") upload([p]);
  }

  async function download() {
    if (picked.length === 0) return;
    const dir = await open({
      directory: true,
      defaultPath: downloadDir || settings.outputDir || undefined,
      title: "Download to",
    });
    if (typeof dir !== "string") return;
    setDownloadDir(dir);
    try {
      const queued = await api.enqueueTransfers(
        picked.map((e) => ({
          kind: "download",
          remote: joinRemote(path, e.name),
          isDir: e.isDir,
          size: e.size,
          localDir: dir,
        })),
        settings.overwrite,
      );
      addJobs(queued, "download");
    } catch (e) {
      setError(String(e));
    }
  }

  async function remove() {
    if (picked.length === 0) return;
    const what = picked.length === 1 ? `"${picked[0].name}"` : `${picked.length} items`;
    const parts = [`Delete ${what} from ${path}? This cannot be undone.`];
    if (picked.some((e) => e.isDir)) parts.push("Folders are deleted with everything in them.");
    if (/^\/C(:|\/|$)/i.test(path)) parts.push("C: holds the dashboard: deleting the wrong file there can stop the Xbox booting.");
    const ok = await confirm(parts.join("\n\n"), {
      title: "Delete from the Xbox?",
      kind: "warning",
      okLabel: "Delete",
      cancelLabel: "Cancel",
    });
    if (!ok) return;
    setBusy(true);
    let failure: string | null = null;
    try {
      await api.remove(picked.map((e) => ({ path: joinRemote(path, e.name), isDir: e.isDir })));
    } catch (e) {
      failure = String(e);
    }
    setBusy(false);
    await list(path);
    if (failure) setError(failure);
  }

  async function commitEdit() {
    if (!edit) return;
    const name = edit.value.trim();
    if (edit.kind === "rename" && name === edit.from) {
      setEdit(null);
      return;
    }
    const problem = nameProblem(name);
    if (problem) {
      setError(problem);
      return;
    }
    setBusy(true);
    let failure: string | null = null;
    try {
      if (edit.kind === "new") await api.makeDir(joinRemote(path, name));
      else await api.rename(joinRemote(path, edit.from), name);
    } catch (e) {
      failure = String(e);
    }
    setBusy(false);
    if (failure) {
      setError(failure);
      return;
    }
    await list(path);
    setSelected(new Set([name]));
    setAnchor(name);
  }

  function editKeys(e: KeyboardEvent) {
    if (e.key === "Enter") commitEdit();
    if (e.key === "Escape") {
      setEdit(null);
      setError(null);
    }
  }

  const crumbs = path.split("/").filter(Boolean);
  const editInput = (value: string) => (
    <input
      className="rename"
      autoFocus
      value={value}
      maxLength={FATX_MAX_NAME}
      disabled={busy}
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      onChange={(e) => setEdit((x) => (x ? { ...x, value: e.target.value } : x))}
      onKeyDown={editKeys}
      onFocus={(e) => {
        // Select the name without its extension, as Finder and Explorer do.
        const dot = e.target.value.lastIndexOf(".");
        e.target.setSelectionRange(0, dot > 0 ? dot : e.target.value.length);
      }}
    />
  );

  return (
    <>
      <main className={`library files${dragging ? " dragging" : ""}`}>
        <div className="toolbar">
          <button
            className="btn btn-ghost"
            disabled={!connected || atRoot || loading}
            onClick={() => list(parentRemote(path))}
            title="Up one folder"
          >
            ↑
          </button>
          <nav className="crumbs">
            <button disabled={!connected} onClick={() => list("/")}>
              Xbox
            </button>
            {crumbs.map((c, i) => (
              <span key={i}>
                <span className="muted">/</span>
                <button onClick={() => list("/" + crumbs.slice(0, i + 1).join("/"))}>{c}</button>
              </span>
            ))}
          </nav>
          {loading && <span className="spinner" />}
          <div className="spacer" />
          <button className="btn btn-ghost" disabled={!connected || loading} onClick={() => list(path)}>
            Refresh
          </button>
          <select value={sort} onChange={(e) => setSort(e.target.value as SortKey)} title="Sort">
            <option value="name">Name</option>
            <option value="size">Size</option>
          </select>
          <button
            className="btn"
            disabled={!connected || atRoot || busy || edit !== null}
            onClick={() => {
              setSelected(new Set());
              setEdit({ kind: "new", value: "New Folder" });
            }}
          >
            New folder
          </button>
          <button
            className="btn"
            disabled={!connected || atRoot || busy || picked.length !== 1 || edit !== null}
            onClick={() => setEdit({ kind: "rename", from: picked[0].name, value: picked[0].name })}
          >
            Rename
          </button>
          <button className="btn" disabled={!connected || atRoot || busy || picked.length === 0} onClick={remove}>
            Delete
          </button>
        </div>

        <div className="scroll">
          {error && (
            <div className="banner" onClick={() => setError(null)} title="Dismiss">
              {error}
            </div>
          )}
          {!connected && (
            <div className="empty">
              <h1>Connect to your Xbox</h1>
              <p className="muted">Once connected you can browse its drives, and send files to it or copy them off.</p>
            </div>
          )}
          {connected && entries !== null && (
            <table className="list files-list">
              <thead>
                <tr>
                  <th />
                  <th />
                  <th>Name</th>
                  <th className="num">Size</th>
                </tr>
              </thead>
              <tbody>
                {edit?.kind === "new" && (
                  <tr className="selected">
                    <td />
                    <td className="ficon-cell">
                      <Icon kind="folder" />
                    </td>
                    <td>{editInput(edit.value)}</td>
                    <td />
                  </tr>
                )}
                {sorted.map((e) => {
                  const on = selected.has(e.name);
                  const renaming = edit?.kind === "rename" && edit.from === e.name;
                  return (
                    <tr
                      key={e.name}
                      className={on ? "selected" : ""}
                      onClick={(ev) => click(ev, e.name)}
                      onDoubleClick={() => e.isDir && !renaming && list(joinRemote(path, e.name))}
                    >
                      <td
                        onClick={(ev) => {
                          ev.stopPropagation();
                          if (!atRoot) toggle(e.name);
                        }}
                      >
                        {!atRoot && <input type="checkbox" checked={on} readOnly />}
                      </td>
                      <td className="ficon-cell">
                        <Icon kind={atRoot ? "drive" : e.isDir ? "folder" : "file"} />
                      </td>
                      <td>
                        {renaming ? (
                          editInput(edit.value)
                        ) : (
                          <span className="row-title">{atRoot ? `${e.name.replace(/:$/, "")}:` : e.name}</span>
                        )}
                      </td>
                      <td className="num muted">{e.isDir ? "" : formatBytes(e.size)}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
          {connected && entries !== null && sorted.length === 0 && edit === null && (
            <div className="empty">
              <h1>This folder is empty</h1>
              <p className="muted">
                Drop files or folders here, or press <b>Upload files…</b>
              </p>
            </div>
          )}
        </div>

        <footer className="actionbar">
          <span className="muted small ellipsis">
            {!connected
              ? ""
              : atRoot
                ? "Open a drive. Games usually live on F: or E:\\Games; C: holds the dashboard."
                : picked.length > 0
                  ? `${picked.length} of ${sorted.length} selected${pickedBytes > 0 ? ` · ${formatBytes(pickedBytes)}` : ""}`
                  : `${sorted.length} item${sorted.length === 1 ? "" : "s"} · drop files here to upload`}
          </span>
          <div className="spacer" />
          <button className="btn" disabled={!connected || atRoot} onClick={() => chooseUpload(false)}>
            Upload files…
          </button>
          <button className="btn" disabled={!connected || atRoot} onClick={() => chooseUpload(true)}>
            Upload folder…
          </button>
          <button className="btn btn-primary" disabled={!connected || atRoot || picked.length === 0} onClick={download}>
            Download{picked.length > 0 ? ` ${picked.length}` : ""}
          </button>
        </footer>
      </main>

      <QueuePanel
        jobs={jobs}
        title="Transfers"
        emptyText="Uploads and downloads appear here. They share one line with ISO builds and run one at a time."
        onClearFinished={() => {
          const keep = jobs.filter((j) => j.state === "queued" || j.state === "running");
          for (const j of jobs) if (!keep.includes(j)) mine.current.delete(j.id);
          setJobs(keep);
        }}
      />
    </>
  );
}
