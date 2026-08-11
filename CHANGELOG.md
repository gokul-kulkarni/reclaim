# Changelog

All notable changes to `reclaim`. Versions follow [semver](https://semver.org);
while the project is pre-1.0, breaking changes may land in a minor bump.

## 0.1.4 — 2026-08-11

### Fixed

- **`--concurrency 1` silently reported `0 B`.** A single-threaded scan measured
  nothing and told you there was nothing to reclaim — a confidently wrong answer,
  which is the failure mode this tool exists to avoid. The measurement pool was
  built with one thread, and each walk then asked `jwalk` for Rayon-backed
  parallelism; that single worker was already blocked waiting on the walk it had
  just started, so the nested request could never be scheduled. It burned the full
  5-second busy timeout and returned zeroes. Single-threaded requests now map to a
  serial walk. Regression tests at both the walk and pipeline level assert that
  concurrency 1 measures exactly the same bytes as the default.
- Partial measurements no longer blame permissions for every failure. The `partial`
  flag is set by unreadable directories, metadata failures and device-boundary
  crossings alike, so the note now states the fact without inventing a cause.

### Documentation

- Added real demo recordings of the terminal UI and the web UI.
- Removed the example output at the top of the README: it had been written
  illustratively during development rather than captured from a real scan.
  Replaced with a genuine `reclaim scan` excerpt.
- Removed the "~260 seconds single-threaded" benchmark claim. It was measured
  against the concurrency bug above, so the figure was meaningless. The parallel
  figure (~11 seconds for a full home-directory scan) is real and re-measured.

## 0.1.3 — 2026-08-09

### Fixed

- `reclaim history report --out` help text claimed a timestamped default filename;
  the actual default is a fixed `report.html` that is overwritten each run.

### Internal

- Stopped tracking `web/tsconfig.tsbuildinfo`, TypeScript's incremental build
  cache, which produced a spurious diff on every local build.

## 0.1.2 — 2026-08-09

### Added

- **`reclaim history report`** — renders a detailed HTML report of every recorded
  run: lifetime totals, a freed-over-time chart, breakdowns by ecosystem and
  trigger, the biggest reclaims to date, and any failures. Self-contained, with
  hand-built SVG charts and no external assets or JavaScript. `--open` launches it
  in a browser.
- **History tab in the web UI**, rendering the same aggregate data live.
  `GET /api/history` now returns the full report rather than a raw run dump.

Both surfaces share one aggregation (`reclaim_core::report::HistoryReport`), so
they cannot disagree about a number.

## 0.1.1 — 2026-08-09

### Added

- Progress feedback while a scan runs. `reclaim scan` and `reclaim clean` print a
  self-erasing status line on a terminal (never on stdout, so `--json` stays
  parseable); the TUI shows an animated spinner and a placeholder panel instead of
  an empty table during project discovery.
- The web UI shows a loading state on first scan and a dimmed "Rescanning…" badge
  on subsequent ones.

### Fixed

- Selected blocks in the web UI's disk map were hard to see. Selection is now an
  accent tint over the whole cell plus a checkmark, legible regardless of the tier
  colour underneath or how small the cell is.

## 0.1.0 — 2026-08-09

First release. Finds and safely reclaims disk space left by developer toolchains
across Node, Python, Rust, JVM, Go, Apple/Xcode, Android, containers, .NET, Ruby,
PHP, Dart, build tools, editors, system caches and ML model stores.

- Parallel scanning with hardlink deduplication and `st_blocks`-based sizing, so
  totals reflect space actually recoverable rather than apparent size.
- Staleness and recovery-cost scoring derived from the owning project's activity,
  not just the artifact's own mtime.
- Tiered deletion: regenerable caches are removed outright, anything riskier goes
  to the Trash. Every path is re-checked against the safety guard immediately
  before deletion.
- Terminal UI, an optional localhost-only web UI with a treemap and
  size-against-staleness plot, a journal of every run, and scheduled background
  cleanup via launchd or systemd user timers.
- macOS and Linux. Windows compiles but is not a supported target.
