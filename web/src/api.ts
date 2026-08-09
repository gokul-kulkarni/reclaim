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

export interface CleanItem {
  id: string;
  label: string;
  disposition: string;
  freed_bytes: number;
  error: string | null;
}

export interface CleanResponse {
  dry_run: boolean;
  bytes_freed: number;
  bytes_trashed: number;
  succeeded: boolean;
  summary: string;
  skipped_caution: number;
  items: CleanItem[];
}

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

  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body?.error) message = body.error;
    } catch {
      // Keep the status line if the body is not JSON.
    }
    throw new Error(message);
  }

  return (await response.json()) as T;
}

export const api = {
  hasToken: () => TOKEN.length > 0,

  scan: (all: boolean) => request<ScanResponse>(`/scan${all ? "?all=true" : ""}`),

  clean: (ids: string[], dryRun: boolean, confirmCaution: boolean) =>
    request<CleanResponse>("/clean", {
      method: "POST",
      body: JSON.stringify({ ids, dry_run: dryRun, confirm_caution: confirmCaution }),
    }),

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
