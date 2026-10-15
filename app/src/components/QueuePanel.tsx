import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { api, formatBytes, formatDuration, type IsoProgress, type JobState, type OutputMode } from "../api";

/** What a job does: build or copy a title, or move something for the Files tab. */
export type JobKind = OutputMode | "upload" | "download";

export interface JobView {
  id: number;
  path: string;
  name: string;
  mode: JobKind;
  /** A local path, or for an upload the path it lands at on the Xbox. */
  outPath: string;
  state: JobState;
  progress: IsoProgress | null;
  rate: number | null;
  message: string | null;
}

const KIND_LABEL: Record<JobKind, string> = {
  iso: "ISO",
  files: "Files",
  upload: "Upload",
  download: "Download",
};

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
  title?: string;
  emptyText: string;
  onClearFinished: () => void;
}

export function QueuePanel({ jobs, title = "Queue", emptyText, onClearFinished }: Props) {
  const activeJobs = jobs.filter((j) => j.state === "queued" || j.state === "running");
  const active = activeJobs.length;
  const finished = jobs.length - active;

  return (
    <aside className="queue">
      <div className="queue-head">
        <h2>{title}</h2>
        <div className="spacer" />
        {active > 0 && (
          <button className="btn btn-small" onClick={() => activeJobs.forEach((j) => api.cancelJob(j.id))}>
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
        <div className="empty small muted">{emptyText}</div>
      )}
      <ul>
        {jobs.map((j) => (
          <li key={j.id} className={`job job-${j.state}`}>
            <div className="job-top">
              <span className="job-name" title={j.outPath}>
                {j.name}
              </span>
              <span className="tag">{KIND_LABEL[j.mode]}</span>
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
              {(j.state === "done" || j.state === "skipped") && j.mode !== "upload" && (
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
