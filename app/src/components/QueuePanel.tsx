import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { api, formatBytes, formatDuration, type IsoProgress, type JobState, type OutputMode } from "../api";

export interface JobView {
  id: number;
  path: string;
  name: string;
  mode: OutputMode;
  outPath: string;
  state: JobState;
  progress: IsoProgress | null;
  rate: number | null;
  message: string | null;
}

const STATE_LABEL: Record<JobState, string> = {
  queued: "Queued",
  running: "Copying",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
  skipped: "Skipped",
};

function detail(j: JobView): string {
  const p = j.progress;
  if (j.state === "running" && p) {
    switch (p.kind) {
      case "listing":
        return `Reading folders… ${p.files} files, ${formatBytes(p.bytes)}`;
      case "copying": {
        const left = j.rate && j.rate > 0 ? (p.total - p.done) / j.rate : NaN;
        const parts = [`${formatBytes(p.done)} of ${formatBytes(p.total)}`];
        if (j.rate) parts.push(`${formatBytes(j.rate)}/s`);
        if (isFinite(left)) parts.push(`${formatDuration(left)} left`);
        return parts.join(" · ");
      }
      case "finishing":
        return "Finishing…";
      default:
        return "";
    }
  }
  if (j.state === "done" && p?.kind === "done") return formatBytes(p.bytes);
  return j.message ?? "";
}

function fraction(j: JobView): number {
  const p = j.progress;
  if (j.state === "done") return 1;
  if (p?.kind === "copying" && p.total > 0) return p.done / p.total;
  if (p?.kind === "finishing") return 1;
  return 0;
}

interface Props {
  jobs: JobView[];
  onClearFinished: () => void;
}

export function QueuePanel({ jobs, onClearFinished }: Props) {
  const active = jobs.filter((j) => j.state === "queued" || j.state === "running").length;
  const finished = jobs.length - active;

  return (
    <aside className="queue">
      <div className="queue-head">
        <h2>Queue</h2>
        <div className="spacer" />
        {active > 0 && (
          <button className="btn btn-small" onClick={() => api.cancelAll()}>
            Cancel all
          </button>
        )}
        {finished > 0 && (
          <button className="btn btn-small btn-ghost" onClick={onClearFinished}>
            Clear finished
          </button>
        )}
      </div>
      {jobs.length === 0 && (
        <div className="empty small muted">Select titles and press Create ISOs (or Copy games, in Files mode). They run one at a time.</div>
      )}
      <ul>
        {jobs.map((j) => (
          <li key={j.id} className={`job job-${j.state}`}>
            <div className="job-top">
              <span className="job-name" title={j.outPath}>
                {j.name}
              </span>
              <span className="tag">{j.mode === "iso" ? "ISO" : "Files"}</span>
              <span className={`tag state-${j.state}`}>{STATE_LABEL[j.state]}</span>
            </div>
            {(j.state === "running" || j.state === "done") && (
              <div className="bar">
                <div style={{ width: `${(fraction(j) * 100).toFixed(1)}%` }} />
              </div>
            )}
            <div className="job-bottom">
              <span className="small muted ellipsis" title={j.message ?? ""}>
                {detail(j)}
              </span>
              <span className="spacer" />
              {(j.state === "queued" || j.state === "running") && (
                <button className="btn btn-small btn-ghost" onClick={() => api.cancelJob(j.id)}>
                  Cancel
                </button>
              )}
              {(j.state === "done" || j.state === "skipped") && (
                <button className="btn btn-small btn-ghost" onClick={() => revealItemInDir(j.outPath)}>
                  Show
                </button>
              )}
            </div>
          </li>
        ))}
      </ul>
    </aside>
  );
}
