# Configuration

`~/.config/reclaim/config.toml` (also on macOS — developer tools overwhelmingly
use `~/.config`, and a dotfile-managed config is more useful than
`~/Library/Application Support`).

```sh
reclaim config init       # write a fully commented default
reclaim config path
reclaim config show       # effective config after env and defaults
reclaim config validate
```

Precedence, lowest first: **built-in defaults → the file → `RECLAIM_*` env vars →
command-line flags.** Every field has a working default, so the tool runs
correctly with no config file at all. An unknown key is a hard error rather than
being silently ignored — a typo in a tool that deletes files should not be quiet.

## `[scan]`

| Key | Default | Meaning |
|---|---|---|
| `project_roots` | `[]` | Directories crawled to find projects with build artifacts. **The setting most worth changing.** Empty means well-known global caches only, which is fast but misses `node_modules` and `target/`. |
| `exclude` | see below | Glob patterns pruned during the project walk. |
| `max_depth` | `8` | How deep to descend below each root. |
| `concurrency` | `0` | Worker threads. 0 = auto: `min(cpus × 2, 16)`. Capped because this workload is metadata-bound — beyond that, threads queue on the filesystem. |
| `follow_symlinks` | `false` | |
| `cross_device` | `false` | Whether the walk may cross onto another filesystem. Leave false unless you want network shares and external disks included. |

Default excludes: `**/Library/**`, `**/.Trash/**`, `**/.git/**`,
`**/node_modules/**`, `**/.venv/**`, `**/Applications/**`.

## `[thresholds]`

| Key | Default | Meaning |
|---|---|---|
| `min_size` | `"50MB"` | Items smaller than this are hidden from the list — but still counted, and reported as "N items hidden". Never silently dropped. |
| `stale_after_days` | `60` | Baseline for the staleness factor in the score. Untouched for this long scores 1.0; longer scales to a cap of 3.0. |
| `per_tier.safe` | `14` | Default age threshold for `--tier safe`. |
| `per_tier.review` | `60` | |
| `per_tier.caution` | `180` | Riskier items wait longer before being suggested. |

Sizes accept `1024`, `50MB`, `1.5GiB`, `2g`. Durations accept `30`, `30d`, `6w`,
`3mo`, `1y`; sub-day units are rejected rather than silently rounded.

## `[delete]`

| Key | Default | Meaning |
|---|---|---|
| `mode` | `"tiered"` | `tiered` purges safe items (space freed immediately) and Trashes the rest. `trash` = everything recoverable, but nothing freed until you empty it. `purge` = everything permanent. |
| `confirm_caution` | `true` | Prompt for caution-tier items even under `--yes`. In a non-interactive context they are **skipped**, never silently removed. |
| `protected_paths` | `["~/Documents", "~/Desktop", "~/.ssh"]` | **Added to** the built-in protected list. Nothing can remove entries from that list: `/`, `/etc`, `$HOME` itself, `~/.ssh`, `~/.gnupg`, any path containing a `.git` component, and anything outside the allowed roots. |

## `[providers]`

```toml
enabled  = ["*"]
disabled = ["ml"]           # full id ("apple.archives") or group prefix ("node")
```

| Key | Default | Meaning |
|---|---|---|
| `apple.keep_latest_simulator_runtimes` | `2` | |
| `apple.offer_non_ios_runtimes` | `true` | tvOS/watchOS/visionOS. |
| `node.keep_pnpm_store` | `true` | The store is hardlinked into every install; deleting it breaks them for a small real gain. |
| `node.offer_node_modules` | `true` | |
| `containers.include_volumes` | `false` | Volumes hold databases and other state that exists nowhere else. |

`reclaim providers` lists every provider and whether it is enabled.

## `[ui]`

| Key | Default | Meaning |
|---|---|---|
| `port` | `0` | 0 picks a free port. |
| `open_browser` | `true` | |
| `theme` | `"auto"` | `auto` \| `light` \| `dark`. |

The server always binds `127.0.0.1` and always requires a per-process token.
Neither is configurable.

## `[schedule]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Managed by `reclaim schedule install`. |
| `cadence` | `"weekly"` | `daily` \| `weekly` \| `biweekly` \| `monthly`. Runs at 03:00. |
| `tiers` | `["safe"]` | **`caution` is rejected at load time.** A job running while nobody is watching never removes anything irreplaceable. |
| `older_than_days` | `60` | |
| `dry_run_first` | `true` | The first scheduled run only reports. Inspect it with `reclaim history`, then set this to false to arm it. |
| `notify` | `true` | |
| `max_runtime_minutes` | `30` | Enforced by the systemd unit on Linux. |

## Environment variables

| Variable | Overrides |
|---|---|
| `RECLAIM_CONCURRENCY` | `scan.concurrency` |
| `RECLAIM_MIN_SIZE` | `thresholds.min_size` |
| `RECLAIM_DELETE_MODE` | `delete.mode` |
| `RECLAIM_PROJECT_ROOTS` | `scan.project_roots` (colon-separated) |
| `RECLAIM_LOG` | Log filter, e.g. `reclaim=debug` |

`HOME`, `XDG_*`, `CARGO_HOME`, `GOPATH`, `GOMODCACHE`, `GOCACHE`, `ANDROID_HOME`,
`ANDROID_SDK_ROOT` and `ANDROID_AVD_HOME` are honoured when locating caches.
