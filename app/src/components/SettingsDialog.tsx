import { useState } from "react";
import type { Settings } from "../api";

interface Props {
  settings: Settings;
  onSave: (s: Settings) => void;
  onClose: () => void;
}

export function SettingsDialog({ settings, onSave, onClose }: Props) {
  const [roots, setRoots] = useState(settings.roots.join("\n"));
  const [depth, setDepth] = useState(settings.depth);
  const [overwrite, setOverwrite] = useState(settings.overwrite);

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal" onMouseDown={(e) => e.stopPropagation()}>
        <h2>Settings</h2>

        <label className="field">
          <span>Where to look for games</span>
          <textarea rows={4} value={roots} onChange={(e) => setRoots(e.target.value)} spellCheck={false} />
          <span className="muted small">
            One per line: a drive (<code>F:</code>) or a folder (<code>E:/Games</code>). Drives the Xbox does not have
            are skipped.
          </span>
        </label>

        <label className="field">
          <span>Folder levels to search below each</span>
          <input
            type="number"
            min={0}
            max={5}
            value={depth}
            onChange={(e) => setDepth(Math.max(0, Math.min(5, Number(e.target.value) || 0)))}
          />
          <span className="muted small">
            2 finds both <code>F:/Halo</code> and <code>F:/Games/Halo</code>.
          </span>
        </label>

        <label className="check-row">
          <input type="checkbox" checked={overwrite} onChange={(e) => setOverwrite(e.target.checked)} />
          <span>Replace ISOs that already exist (otherwise they are skipped)</span>
        </label>

        <div className="modal-actions">
          <button className="btn btn-ghost" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={() =>
              onSave({
                ...settings,
                roots: roots
                  .split(/\r?\n|,/)
                  .map((r) => r.trim())
                  .filter(Boolean),
                depth,
                overwrite,
              })
            }
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
