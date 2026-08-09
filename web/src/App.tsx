import { useCallback, useEffect, useMemo, useState } from "react";
import { api, formatBytes } from "./api";
import type { Candidate, CleanResponse, ScanResponse } from "./api";
import { Treemap } from "./Treemap";
import { Scatter } from "./Scatter";

type View = "treemap" | "scatter" | "table";
type SortKey = "score" | "size" | "age" | "name";

export default function App() {
  const [scan, setScan] = useState<ScanResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [view, setView] = useState<View>("treemap");
  const [sort, setSort] = useState<SortKey>("score");
  const [query, setQuery] = useState("");
  const [inspecting, setInspecting] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [result, setResult] = useState<CleanResponse | null>(null);

  const runScan = useCallback(async (all = false) => {
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const response = await api.scan(all);
      setScan(response);
      // A rescan invalidates ids, so a stale selection could target the wrong thing.
      setSelected(new Set());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    if (!api.hasToken()) {
      setError(
        "No access token. Open the link printed by `reclaim ui` rather than typing the address."
      );
      return;
    }
    void runScan();
  }, [runScan]);

  const rows = useMemo(() => {
    if (!scan) return [];
    const needle = query.trim().toLowerCase();
    const filtered = scan.candidates.filter(
      (c) =>
        !needle ||
        c.label.toLowerCase().includes(needle) ||
        c.provider.toLowerCase().includes(needle) ||
        c.group_title.toLowerCase().includes(needle)
    );

    const sorted = [...filtered];
    switch (sort) {
      case "size":
        sorted.sort((a, b) => b.on_disk - a.on_disk);
        break;
      case "age":
        sorted.sort((a, b) => (b.last_used_days ?? 0) - (a.last_used_days ?? 0));
        break;
      case "name":
        sorted.sort((a, b) => a.label.localeCompare(b.label));
        break;
      default:
        sorted.sort((a, b) => b.score - a.score);
    }
    return sorted;
  }, [scan, query, sort]);

  const chosen = useMemo(
    () => (scan?.candidates ?? []).filter((c) => selected.has(c.id)),
    [scan, selected]
  );
  const chosenBytes = chosen.reduce((sum, c) => sum + c.on_disk, 0);
  const cautionChosen = chosen.filter((c) => c.tier === "caution");

  const toggle = (id: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  // Bulk selection never includes caution-tier items, matching the TUI: one
  // click must not be able to sweep up something irreplaceable.
  const selectAllSafe = () =>
    setSelected(new Set(rows.filter((c) => c.tier !== "caution").map((c) => c.id)));

  const doClean = async (dryRun: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const response = await api.clean([...selected], dryRun, cautionChosen.length > 0);
      setResult(response);
      setConfirming(false);
      if (!dryRun) await runScan();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const inspected = scan?.candidates.find((c) => c.id === inspecting) ?? null;

  return (
    <div className="app">
      <header>
        <div className="brand">
          <h1>reclaim</h1>
          {scan && (
            <span className="totals">
              <strong>{formatBytes(scan.total_reclaimable)}</strong> reclaimable ·{" "}
              {scan.candidates.length} items · {scan.projects_scanned} projects ·{" "}
              {(scan.elapsed_ms / 1000).toFixed(1)}s
            </span>
          )}
        </div>
        <div className="actions">
          <button onClick={() => void runScan()} disabled={busy}>
            {busy ? "Scanning…" : "Rescan"}
          </button>
          <button onClick={() => void runScan(true)} disabled={busy}>
            Show everything
          </button>
        </div>
      </header>

      {error && (
        <div className="banner error">
          <strong>Error.</strong> {error}
        </div>
      )}

      {scan?.unreadable ? (
        <div className="banner warn">
          {scan.unreadable} director{scan.unreadable === 1 ? "y" : "ies"} could not be read, so
          these totals are a lower bound.
        </div>
      ) : null}

      {result && (
        <div className={`banner ${result.succeeded ? "ok" : "error"}`}>
          <strong>{result.dry_run ? "Dry run." : "Done."}</strong> {result.summary}
          {result.bytes_trashed > 0 && (
            <> {formatBytes(result.bytes_trashed)} is in the Trash and still uses disk until you empty it.</>
          )}
          {result.skipped_caution > 0 && (
            <> {result.skipped_caution} caution-tier item(s) were skipped.</>
          )}
        </div>
      )}

      <nav className="views">
        {(["treemap", "scatter", "table"] as View[]).map((v) => (
          <button key={v} className={view === v ? "tab active" : "tab"} onClick={() => setView(v)}>
            {v === "treemap" ? "Disk map" : v === "scatter" ? "Size vs staleness" : "List"}
          </button>
        ))}
        <input
          className="search"
          placeholder="Filter…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        {view === "table" && (
          <select value={sort} onChange={(e) => setSort(e.target.value as SortKey)}>
            <option value="score">Sort: best first</option>
            <option value="size">Sort: largest</option>
            <option value="age">Sort: stalest</option>
            <option value="name">Sort: name</option>
          </select>
        )}
        <button onClick={selectAllSafe}>Select all (not caution)</button>
        <button onClick={() => setSelected(new Set())}>Clear</button>
      </nav>

      <main className={busy && scan ? "scanning" : undefined}>
        {busy && !scan ? (
          <div className="scan-loading">
            <span className="spinner" aria-hidden="true" />
            <p>Scanning your disk…</p>
            <p className="dim">
              Large project roots can take a while the first time — sizes will fill in as they're
              found.
            </p>
          </div>
        ) : (
          <>
            {view === "treemap" && (
              <Treemap
                candidates={rows}
                selected={selected}
                onSelect={toggle}
                onInspect={setInspecting}
                width={960}
                height={520}
              />
            )}
            {view === "scatter" && (
              <Scatter
                candidates={rows}
                selected={selected}
                onSelect={toggle}
                width={960}
                height={520}
              />
            )}
            {view === "table" && (
              <Table rows={rows} selected={selected} onToggle={toggle} onInspect={setInspecting} />
            )}
          </>
        )}
        {busy && scan && (
          <div className="scan-overlay">
            <span className="spinner" aria-hidden="true" />
            Rescanning…
          </div>
        )}
      </main>

      {scan && scan.hidden_count > 0 && (
        <p className="note">
          {scan.hidden_count} item(s) totalling {formatBytes(scan.hidden_bytes)} are below the size
          threshold. Use “Show everything” to include them.
        </p>
      )}

      <footer className={selected.size ? "bar active" : "bar"}>
        <span>
          {selected.size} selected · <strong>{formatBytes(chosenBytes)}</strong>
          {cautionChosen.length > 0 && (
            <em className="caution-note"> · includes {cautionChosen.length} caution-tier</em>
          )}
        </span>
        <span className="spacer" />
        <button disabled={!selected.size || busy} onClick={() => void doClean(true)}>
          Preview
        </button>
        <button
          className="danger"
          disabled={!selected.size || busy}
          onClick={() => setConfirming(true)}
        >
          Reclaim {formatBytes(chosenBytes)}
        </button>
      </footer>

      {inspected && <Detail candidate={inspected} onClose={() => setInspecting(null)} />}

      {confirming && (
        <Confirm
          candidates={chosen}
          bytes={chosenBytes}
          onCancel={() => setConfirming(false)}
          onConfirm={() => void doClean(false)}
        />
      )}
    </div>
  );
}

function Table({
  rows,
  selected,
  onToggle,
  onInspect,
}: {
  rows: Candidate[];
  selected: Set<string>;
  onToggle: (id: string) => void;
  onInspect: (id: string) => void;
}) {
  if (!rows.length) return <p className="empty">Nothing matches.</p>;

  return (
    <table className="grid">
      <thead>
        <tr>
          <th />
          <th>Size</th>
          <th>Risk</th>
          <th>Last used</th>
          <th>What</th>
          <th>Ecosystem</th>
          <th>Comes back</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((c) => (
          <tr key={c.id} className={selected.has(c.id) ? "row selected" : "row"}>
            <td>
              <input
                type="checkbox"
                checked={selected.has(c.id)}
                onChange={() => onToggle(c.id)}
                aria-label={`Select ${c.label}`}
              />
            </td>
            <td className="num">
              {formatBytes(c.on_disk)}
              {c.shared > 0 && <span className="shared"> +{formatBytes(c.shared)} shared</span>}
            </td>
            <td>
              <span className={`pill ${c.tier}`}>{c.tier}</span>
            </td>
            <td>
              {c.last_used_human}
              {c.active_now && <span className="pill active">in use</span>}
            </td>
            <td>
              <button className="link" onClick={() => onInspect(c.id)}>
                {c.label}
              </button>
              {c.warnings.length > 0 && (
                <span
                  className={`warn-dot ${c.warnings.some((w) => w.severity === "danger") ? "danger" : ""}`}
                  title={c.warnings.map((w) => w.message).join("\n")}
                >
                  !
                </span>
              )}
            </td>
            <td>{c.group_title}</td>
            <td className="dim">{c.regen}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Detail({ candidate, onClose }: { candidate: Candidate; onClose: () => void }) {
  return (
    <div className="overlay" onClick={onClose}>
      <div className="panel" onClick={(e) => e.stopPropagation()}>
        <h2>{candidate.label}</h2>
        <p className="dim">{candidate.detail}</p>

        <dl>
          <dt>Size on disk</dt>
          <dd>{formatBytes(candidate.on_disk)}</dd>
          {candidate.shared > 0 && (
            <>
              <dt>Shared</dt>
              <dd className="warn-text">
                {formatBytes(candidate.shared)} is hardlinked elsewhere and will not be freed
              </dd>
            </>
          )}
          <dt>Files</dt>
          <dd>{candidate.files.toLocaleString()}</dd>
          <dt>Last used</dt>
          <dd>
            {candidate.last_used_human}
            {candidate.active_now && " — touched in the last 24h, a build may be using it"}
          </dd>
          <dt>Risk</dt>
          <dd>
            <span className={`pill ${candidate.tier}`}>{candidate.tier}</span>
          </dd>
          <dt>Comes back</dt>
          <dd>{candidate.regen}</dd>
          <dt>Paths</dt>
          <dd>
            <ul className="paths">
              {candidate.paths.map((p) => (
                <li key={p}>
                  <code>{p}</code>
                </li>
              ))}
            </ul>
          </dd>
        </dl>

        {candidate.warnings.length > 0 && (
          <>
            <h3>Before you delete this</h3>
            <ul className="warnings">
              {candidate.warnings.map((w, i) => (
                <li key={i} className={w.severity}>
                  {w.message}
                </li>
              ))}
            </ul>
          </>
        )}

        <button onClick={onClose}>Close</button>
      </div>
    </div>
  );
}

function Confirm({
  candidates,
  bytes,
  onCancel,
  onConfirm,
}: {
  candidates: Candidate[];
  bytes: number;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const caution = candidates.filter((c) => c.tier === "caution");

  return (
    <div className="overlay" onClick={onCancel}>
      <div className="panel" onClick={(e) => e.stopPropagation()}>
        <h2>Reclaim {formatBytes(bytes)}?</h2>
        <p>
          {candidates.length} item{candidates.length === 1 ? "" : "s"} will be removed. Safe items
          are deleted outright so the space is freed immediately; anything riskier goes to the
          Trash.
        </p>

        <ul className="confirm-list">
          {candidates.slice(0, 12).map((c) => (
            <li key={c.id}>
              <span className={`pill ${c.tier}`}>{c.tier}</span> {formatBytes(c.on_disk)} —{" "}
              {c.label}
            </li>
          ))}
          {candidates.length > 12 && <li className="dim">…and {candidates.length - 12} more</li>}
        </ul>

        {caution.length > 0 && (
          <div className="banner error">
            <strong>{caution.length} item(s) may be irreplaceable:</strong>
            <ul>
              {caution.flatMap((c) =>
                c.warnings.map((w, i) => <li key={`${c.id}-${i}`}>{w.message}</li>)
              )}
            </ul>
          </div>
        )}

        <div className="panel-actions">
          <button onClick={onCancel}>Cancel</button>
          <button className="danger" onClick={onConfirm}>
            Yes, reclaim {formatBytes(bytes)}
          </button>
        </div>
      </div>
    </div>
  );
}
