import { formatBytes, type Title } from "../api";

export type ViewMode = "grid" | "list";

interface Props {
  titles: Title[];
  covers: Record<string, string | null>;
  sizes: Record<string, { bytes: number; files: number }>;
  selected: Set<string>;
  queued: Set<string>;
  view: ViewMode;
  onToggle: (path: string, additive: boolean) => void;
}

function initials(name: string): string {
  const words = name.replace(/[^\p{L}\p{N} ]/gu, " ").split(/\s+/).filter(Boolean);
  return (words.slice(0, 2).map((w) => w[0]).join("") || "?").toUpperCase();
}

function Cover({ title, src }: { title: Title; src: string | null | undefined }) {
  if (src) return <img className="cover" src={src} alt="" draggable={false} />;
  return (
    <div className={`cover cover-empty${src === undefined ? " loading" : ""}`}>
      {initials(title.titleName)}
    </div>
  );
}

function Badges({ t }: { t: Title }) {
  return (
    <div className="badges">
      {t.titleCode && <span className="tag mono">{t.titleCode}</span>}
      {t.region && <span className="tag">{t.region}</span>}
      {t.discNumber > 1 && <span className="tag">Disc {t.discNumber}</span>}
      {t.error && (
        <span className="tag tag-red" title={t.error}>
          XBE unreadable
        </span>
      )}
    </div>
  );
}

export function Library({ titles, covers, sizes, selected, queued, view, onToggle }: Props) {
  if (view === "list") {
    return (
      <table className="list">
        <thead>
          <tr>
            <th />
            <th />
            <th>Title</th>
            <th>ID</th>
            <th>Region</th>
            <th className="num">Size</th>
            <th>Folder on Xbox</th>
          </tr>
        </thead>
        <tbody>
          {titles.map((t) => {
            const on = selected.has(t.path);
            return (
              <tr
                key={t.path}
                className={on ? "selected" : ""}
                onClick={(e) => onToggle(t.path, e.shiftKey || e.metaKey || e.ctrlKey)}
              >
                <td>
                  <input type="checkbox" checked={on} readOnly />
                </td>
                <td className="thumb">
                  <Cover title={t} src={covers[t.path]} />
                </td>
                <td>
                  <div className="row-title">
                    {t.titleName}
                    {queued.has(t.path) && <span className="tag tag-green">queued</span>}
                  </div>
                  <div className="muted small">{t.isoName}</div>
                </td>
                <td className="mono">{t.titleCode ?? t.titleId}</td>
                <td>{t.region}</td>
                <td className="num">{sizes[t.path] ? formatBytes(sizes[t.path].bytes) : "…"}</td>
                <td className="mono small muted">{t.path}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    );
  }

  return (
    <div className="grid">
      {titles.map((t) => {
        const on = selected.has(t.path);
        return (
          <button
            key={t.path}
            className={`card${on ? " selected" : ""}`}
            onClick={(e) => onToggle(t.path, e.shiftKey || e.metaKey || e.ctrlKey)}
            title={t.path}
          >
            <span className="check" aria-hidden>
              {on ? "✓" : ""}
            </span>
            {queued.has(t.path) && <span className="queued-flag">queued</span>}
            <Cover title={t} src={covers[t.path]} />
            <div className="card-body">
              <div className="card-title">{t.titleName}</div>
              <Badges t={t} />
              <div className="muted small">{sizes[t.path] ? formatBytes(sizes[t.path].bytes) : "Measuring…"}</div>
            </div>
          </button>
        );
      })}
    </div>
  );
}
