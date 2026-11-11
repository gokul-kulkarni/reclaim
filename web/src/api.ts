// Typed client for the local reclaim API.
//
// The token comes from the URL the CLI printed. It is stripped from the address
// bar immediately after being read, so it does not end up in browser history or
// get pasted along with the URL.

export type Tier = "safe" | "review" | "caution";
export type Severity = "info" | "caution" | "danger";

export interface Warning {
  severity: Severity;
  message: string;
}

export interface Candidate {
  id: string;
  provider: string;
  group: string;
  group_title: string;
  label: string;
  detail: string;
  paths: string[];
  tier: Tier;
  kind: string;
  on_disk: number;
  shared: number;
  files: number;
  partial: boolean;
  last_used_days: number | null;
  last_used_human: string;
  active_now: boolean;
  score: number;
  regen: string;
  warnings: Warning[];
}

export interface GroupTotal {
  group: string;
  title: string;
  on_disk: number;
}

export interface ScanResponse {
  total_reclaimable: number;
  projects_scanned: number;
  elapsed_ms: number;
  unreadable: number;
  hidden_count: number;
  hidden_bytes: number;
  groups: GroupTotal[];
  candidates: Candidate[];
}

export type SectionKey = "failed" | "removed" | "command" | "trashed" | "dry_run" | "gone";

export interface CleanItem {
  id: string;
  label: string;
  disposition: string;
  section: SectionKey;
  freed_bytes: number;
  expected_bytes: number;
  /** What was freed, or what was expected when nothing was. */
  shown_bytes: number;
  /** Paths with the home directory shortened to `~`. */
  paths: string[];
  /** The command that reclaimed it, for items cleaned by a tool. */
  command: string | null;
  error: string | null;
}

export interface CleanSection {
  key: SectionKey;
  heading: string;
  explanation: string;
  bytes: number;
  /** Item ids in display order. */
  ids: string[];
}

export interface CleanResponse {
  dry_run: boolean;
  bytes_freed: number;
  bytes_trashed: number;
  succeeded: boolean;
  summary: string;
  skipped_caution: number;
  items: CleanItem[];
  sections: CleanSection[];
}

/** One line of `POST /api/clean?stream=true`. */
export type CleanEvent =
  | { type: "started"; total: number; bytes: number }
  | { type: "item_started"; id: string; label: string }
  | { type: "item_finished"; item: CleanItem }
  | { type: "finished"; result: CleanResponse };

export interface GroupStats {
  group: string;
  title: string;
  freed: number;
  trashed: number;
  items: number;
}

export interface TriggerStats {
  label: string;
  runs: number;
  freed: number;
}

export interface TimelinePoint {
  started_at_ms: number;
  when: string;
  freed: number;
  trashed: number;
  cumulative_freed: number;
}

export interface TopItem {
  label: string;
  group_title: string;
  tier: Tier;
  bytes: number;
  when: string;
}

export interface FailureEntry {
  when: string;
  label: string;
  provider: string;
  error: string;
}

export interface RunSummary {
  id: string;
  when: string;
  trigger: string;
  dry_run: boolean;
  candidates_found: number;
  freed: number;
  trashed: number;
  items: number;
  failures: number;
  succeeded: boolean;
}

export interface HistoryReport {
  generated_at_ms: number;
  runs: number;
  real_runs: number;
  dry_runs: number;
  lifetime_freed: number;
  lifetime_trashed: number;
  lifetime_candidates_found: number;
  failed_items: number;
  by_group: GroupStats[];
  by_trigger: TriggerStats[];
  timeline: TimelinePoint[];
  top_items: TopItem[];
  failures: FailureEntry[];
  runs_detail: RunSummary[];
}

function readToken(): string {
  const params = new URLSearchParams(window.location.search);
  const token = params.get("t") ?? "";
  if (token) {
    // Keep it out of history and out of anything the user copies.
    const clean = window.location.pathname + window.location.hash;
    window.history.replaceState({}, "", clean);
  }
  return token;
}

const TOKEN = readToken();

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api${path}`, {
    ...init,
    headers: {
      "content-type": "application/json",
      "x-reclaim-token": TOKEN,
      ...(init?.headers ?? {}),
    },
  });

  if (!response.ok) throw await failure(response);
  return (await response.json()) as T;
}

async function failure(response: Response): Promise<Error> {
  let message = `${response.status} ${response.statusText}`;
  try {
    const body = await response.json();
    if (body?.error) message = body.error;
  } catch {
    // Keep the status line if the body is not JSON.
  }
  return new Error(message);
}

/**
 * Run a clean, calling `onEvent` for each progress line as it arrives, and
 * resolve with the final result.
 */
async function cleanStream(
  ids: string[],
  dryRun: boolean,
  confirmCaution: boolean,
  onEvent: (event: CleanEvent) => void
): Promise<CleanResponse> {
  const response = await fetch("/api/clean?stream=true", {
    method: "POST",
    headers: { "content-type": "application/json", "x-reclaim-token": TOKEN },
    body: JSON.stringify({ ids, dry_run: dryRun, confirm_caution: confirmCaution }),
  });
  if (!response.ok) throw await failure(response);
  if (!response.body) throw new Error("the server sent no progress stream");

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  // Held in an object: TypeScript does not track assignments made in a closure.
  const final: { result: CleanResponse | null } = { result: null };

  const handle = (line: string) => {
    if (!line.trim()) return;
    const event = JSON.parse(line) as CleanEvent | { type: "error"; message: string };
    if (event.type === "error") throw new Error(event.message);
    if (event.type === "finished") final.result = event.result;
    onEvent(event);
  };

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    pending += decoder.decode(value, { stream: true });
    const lines = pending.split("\n");
    pending = lines.pop() ?? "";
    lines.forEach(handle);
  }
  handle(pending + decoder.decode());

  if (!final.result) {
    throw new Error("the run ended without a result; rescan to see what is left");
  }
  return final.result;
}

export const api = {
  hasToken: () => TOKEN.length > 0,

  scan: (all: boolean) => request<ScanResponse>(`/scan${all ? "?all=true" : ""}`),

  clean: (ids: string[], dryRun: boolean, confirmCaution: boolean) =>
    request<CleanResponse>("/clean", {
      method: "POST",
      body: JSON.stringify({ ids, dry_run: dryRun, confirm_caution: confirmCaution }),
    }),

  cleanStream,

  history: () => request<HistoryReport>("/history"),
};

/** Binary units with the short spelling, matching the CLI exactly. */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  if (value >= 100) return `${value.toFixed(0)} ${units[unit]}`;
  if (value >= 10) return `${value.toFixed(1)} ${units[unit]}`;
  return `${value.toFixed(2)} ${units[unit]}`;
}
