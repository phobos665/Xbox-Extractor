import { useEffect, useRef, useState } from "react";
import { api, onDiscovered, type ConnectionInfo, type Found } from "../api";

export type ConnStatus =
  | { kind: "idle" }
  | { kind: "connecting" }
  | { kind: "connected"; server: string | null }
  | { kind: "error"; message: string };

interface Props {
  info: ConnectionInfo;
  recentHosts: string[];
  status: ConnStatus;
  onChange: (info: ConnectionInfo) => void;
  onConnect: () => void;
  onDisconnect: () => void;
  onOpenSettings: () => void;
}

export function ConnectionBar({ info, recentHosts, status, onChange, onConnect, onDisconnect, onOpenSettings }: Props) {
  const [found, setFound] = useState<Found[]>([]);
  const [searching, setSearching] = useState(false);
  const [showFound, setShowFound] = useState(false);
  const [showLogin, setShowLogin] = useState(false);
  const popRef = useRef<HTMLDivElement>(null);
  const connected = status.kind === "connected";
  const busy = status.kind === "connecting";

  useEffect(() => {
    const close = (e: MouseEvent) => {
      if (popRef.current && !popRef.current.contains(e.target as Node)) {
        setShowFound(false);
        setShowLogin(false);
      }
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, []);

  async function search() {
    setSearching(true);
    setShowFound(true);
    setFound([]);
    const un = await onDiscovered((f) =>
      setFound((prev) => (prev.some((p) => p.host === f.host) ? prev : [...prev, f])),
    );
    try {
      const all = await api.discover();
      setFound(all);
    } finally {
      un();
      setSearching(false);
    }
  }

  return (
    <header className="topbar" ref={popRef}>
      <div className="brand">
        <span className="brand-mark" aria-hidden>
          X
        </span>
        <span>Xbox ISO Batcher</span>
      </div>

      <form
        className="conn"
        onSubmit={(e) => {
          e.preventDefault();
          if (!connected && info.host.trim()) onConnect();
        }}
      >
        <label className="field host">
          <span>Xbox address</span>
          <input
            list="recent-hosts"
            placeholder="192.168.1.20"
            value={info.host}
            disabled={connected || busy}
            onChange={(e) => onChange({ ...info, host: e.target.value.trim() })}
          />
          <datalist id="recent-hosts">
            {recentHosts.map((h) => (
              <option key={h} value={h} />
            ))}
          </datalist>
        </label>

        <div className="pop-anchor">
          <button type="button" className="btn" disabled={connected || searching} onClick={search}>
            {searching ? <span className="spinner" /> : null}
            Find Xbox
          </button>
          {showFound && (
            <div className="popover">
              <div className="popover-title">FTP servers on this network</div>
              {found.length === 0 && (
                <div className="muted small">{searching ? "Searching…" : "Nothing answered on port 21."}</div>
              )}
              {found.map((f) => (
                <button
                  type="button"
                  key={f.host}
                  className="found"
                  onClick={() => {
                    onChange({ ...info, host: f.host, port: f.port });
                    setShowFound(false);
                  }}
                >
                  <span className={f.looksLikeXbox ? "tag tag-green" : "tag"}>{f.looksLikeXbox ? "Xbox" : "FTP"}</span>
                  <span className="mono">{f.host}</span>
                  <span className="muted small ellipsis">{f.banner.replace(/^220[- ]?/, "")}</span>
                </button>
              ))}
            </div>
          )}
        </div>

        <div className="pop-anchor">
          <button type="button" className="btn btn-ghost" disabled={connected || busy} onClick={() => setShowLogin((v) => !v)}>
            Login: {info.user}
          </button>
          {showLogin && (
            <div className="popover login">
              <label className="field">
                <span>Port</span>
                <input
                  type="number"
                  value={info.port}
                  onChange={(e) => onChange({ ...info, port: Number(e.target.value) || 21 })}
                />
              </label>
              <label className="field">
                <span>User</span>
                <input value={info.user} onChange={(e) => onChange({ ...info, user: e.target.value })} />
              </label>
              <label className="field">
                <span>Password</span>
                <input
                  type="password"
                  value={info.password}
                  onChange={(e) => onChange({ ...info, password: e.target.value })}
                />
              </label>
              <div className="muted small">Most consoles use port 21 and xbox / xbox.</div>
            </div>
          )}
        </div>

        {connected ? (
          <button type="button" className="btn" onClick={onDisconnect}>
            Disconnect
          </button>
        ) : (
          <button type="submit" className="btn btn-primary" disabled={busy || !info.host.trim()}>
            {busy ? <span className="spinner" /> : null}
            Connect
          </button>
        )}
      </form>

      <div className="status">
        {status.kind === "connected" && (
          <span className="pill pill-green" title={status.server ?? ""}>
            Connected{status.server ? ` · ${status.server}` : ""}
          </span>
        )}
        {status.kind === "error" && <span className="pill pill-red">{status.message}</span>}
      </div>

      <button className="btn btn-ghost icon" title="Settings" onClick={onOpenSettings}>
        ⚙
      </button>
    </header>
  );
}
