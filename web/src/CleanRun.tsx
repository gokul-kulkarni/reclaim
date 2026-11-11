import { useEffect, useRef, useState } from "react";
import { formatBytes } from "./api";
import type { CleanEvent, CleanItem, CleanResponse, SectionKey } from "./api";

/** What a clean in flight has done so far, folded from its stream. */
export interface RunProgress {
  dryRun: boolean;
  total: number;
  bytes: number;
  done: number;
  freed: number;
  trashed: number;
  wouldFree: number;
  failed: number;
  /** Labels started and not yet finished, oldest first. */
  inFlight: string[];
  finished: CleanItem[];
}

export function startProgress(dryRun: boolean, total: number, bytes: number): RunProgress {
  return {
    dryRun,
    total,
    bytes,
    done: 0,
    freed: 0,
    trashed: 0,
    wouldFree: 0,
    failed: 0,
    inFlight: [],
    finished: [],
  };
}

/** Fold one stream event into the progress, returning a new object. */
export function advance(progress: RunProgress, event: CleanEvent): RunProgress {
  switch (event.type) {
    case "started":
      return { ...progress, total: event.total, bytes: event.bytes };
    case "item_started":
      return { ...progress, inFlight: [...progress.inFlight, event.label] };
    case "item_finished": {
      const { item } = event;
      const at = progress.inFlight.indexOf(item.label);
      const inFlight =
        at < 0 ? progress.inFlight : progress.inFlight.filter((_, i) => i !== at);
      return {
        ...progress,
        inFlight,
        done: progress.done + 1,
        freed:
          progress.freed +
          (item.section === "removed" || item.section === "command" ? item.freed_bytes : 0),
        trashed: progress.trashed + (item.section === "trashed" ? item.freed_bytes : 0),
        wouldFree: progress.wouldFree + (item.section === "dry_run" ? item.expected_bytes : 0),
        failed: progress.failed + (item.section === "failed" ? 1 : 0),
        finished: [...progress.finished, item],
      };
    }
    case "finished":
      return { ...progress, inFlight: [] };
  }
}

const VERB: Record<SectionKey, string> = {
  failed: "failed",
  removed: "deleted",
  command: "cleaned",
  trashed: "trashed",
  dry_run: "would free",
  gone: "already gone",
};

/** Non-dismissable panel shown while items are removed. */
export function ProgressPanel({ progress }: { progress: RunProgress }) {
  const ratio = progress.total ? Math.min(progress.done / progress.total, 1) : 0;
  const listRef = useRef<HTMLOListElement>(null);

  // Keep the newest result in view as the list grows.
  useEffect(() => {
    const list = listRef.current;
    if (list) list.scrollTop = list.scrollHeight;
  }, [progress.finished.length]);

  return (
    <div className="overlay">
      <div className="panel run-panel" role="status" aria-live="polite">
        <h2>
          <span className="spinner" aria-hidden="true" />{" "}
          {progress.dryRun ? "Checking" : "Reclaiming"} {formatBytes(progress.bytes)} from{" "}
          {progress.total} item{progress.total === 1 ? "" : "s"}
        </h2>

        <div
          className="meter"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={progress.total}
          aria-valuenow={progress.done}
        >
          <div className="meter-fill" style={{ width: `${ratio * 100}%` }} />
        </div>

        <p className="run-totals">
          <strong>
            {progress.done} of {progress.total}
          </strong>
          {progress.dryRun ? (
            <> · {formatBytes(progress.wouldFree)} would be freed</>
          ) : (
            <>
              {" "}
              · <span className="ok-text">{formatBytes(progress.freed)} freed</span>
              {progress.trashed > 0 && (
                <>
                  {" "}
                  · <span className="warn-text">{formatBytes(progress.trashed)} to Trash</span>
                </>
              )}
            </>
          )}
          {progress.failed > 0 && (
            <>
              {" "}
              · <span className="bad-text">{progress.failed} failed</span>
            </>
          )}
        </p>
        <p className="dim run-now">
          {progress.inFlight.length > 0 ? <>Now: {progress.inFlight[0]}</> : " "}
        </p>

        <ol className="run-log" ref={listRef}>
          {progress.finished.map((item) => (
            <li key={item.id}>
              <span className={`verb ${item.section}`}>{VERB[item.section]}</span>
              <span className="num">{formatBytes(item.shown_bytes)}</span>
              <span className="run-label">{item.label}</span>
            </li>
          ))}
        </ol>
        <p className="dim small">Results appear when every item is done.</p>
      </div>
    </div>
  );
}

/** What the run did, grouped by outcome. Stays up until the user closes it. */
export function ResultsPanel({
  result,
  onClose,
}: {
  result: CleanResponse;
  onClose: () => void;
}) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const byId = new Map(result.items.map((item) => [item.id, item]));
  const failed = result.items.filter((i) => i.section === "failed").length;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  return (
    <div className="overlay" onClick={onClose}>
      <div
        className="panel run-panel"
        role="dialog"
        aria-label="Clean results"
        onClick={(e) => e.stopPropagation()}
      >
        <h2>{result.dry_run ? "Dry run results" : "Results"}</h2>

        <div className="stats run-stats">
          {result.dry_run ? (
            <div className="card">
              <div className="card-value">
                {formatBytes(result.items.reduce((sum, i) => sum + i.expected_bytes, 0))}
              </div>
              <div className="dim small">would be reclaimed</div>
            </div>
          ) : (
            <>
              <div className="card safe">
                <div className="card-value">{formatBytes(result.bytes_freed)}</div>
                <div className="dim small">freed now</div>
              </div>
              <div className="card review">
                <div className="card-value">{formatBytes(result.bytes_trashed)}</div>
                <div className="dim small">in the Trash</div>
              </div>
            </>
          )}
          <div className={failed ? "card caution" : "card"}>
            <div className="card-value">{failed}</div>
            <div className="dim small">failed</div>
          </div>
        </div>

        {result.bytes_trashed > 0 && (
          <p className="warn-text small">
            {formatBytes(result.bytes_trashed)} is in the Trash and still uses disk until you empty
            it.
          </p>
        )}
        {result.skipped_caution > 0 && (
          <p className="warn-text small">
            {result.skipped_caution} caution-tier item(s) were skipped: they need explicit
            confirmation.
          </p>
        )}

        {result.sections.map((section) => (
          <section key={section.key} className={`run-section ${section.key}`}>
            <h3>
              {section.heading}{" "}
              <span className="dim">
                · {section.ids.length} item{section.ids.length === 1 ? "" : "s"} ·{" "}
                {formatBytes(section.bytes)}
              </span>
            </h3>
            <p className="dim small">{section.explanation}</p>
            <ul className="run-items">
              {section.ids.map((id) => {
                const item = byId.get(id);
                return item ? (
                  <ResultRow
                    key={id}
                    item={item}
                    expanded={expanded.has(id)}
                    onToggle={() => toggle(id)}
                  />
                ) : null;
              })}
            </ul>
          </section>
        ))}

        <div className="panel-actions">
          <button onClick={onClose} autoFocus>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

function ResultRow({
  item,
  expanded,
  onToggle,
}: {
  item: CleanItem;
  expanded: boolean;
  onToggle: () => void;
}) {
  const [first, ...rest] = item.paths;
  return (
    <li>
      <div className="run-row">
        <span className="num">{formatBytes(item.shown_bytes)}</span>
        <span className="run-label">{item.label}</span>
      </div>
      <div className="run-where dim">
        {item.command ? (
          <>
            ran <code>{item.command}</code>
          </>
        ) : first ? (
          <>
            <code>{first}</code>
            {rest.length > 0 && (
              <button className="link" onClick={onToggle}>
                {expanded ? "fewer" : `+${rest.length} more`}
              </button>
            )}
          </>
        ) : null}
      </div>
      {expanded && rest.length > 0 && (
        <ul className="paths">
          {rest.map((p) => (
            <li key={p}>
              <code>{p}</code>
            </li>
          ))}
        </ul>
      )}
      {item.error && <div className="bad-text small">{item.error}</div>}
    </li>
  );
}
