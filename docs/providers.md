# Providers

What each provider offers, why it is tiered the way it is, and what it costs to
get back. This is the same information the tool shows at runtime — recorded here
so the reasoning is auditable without running anything.

Tiers: **safe** = regenerates automatically · **review** = costs a re-download or
a rebuild · **caution** = may be irreplaceable, always confirmed separately.

## Node.js — `node.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| `~/.npm` | safe | next `npm install` | |
| pnpm store | review | npm registry | **Hardlinked into every `node_modules` on the machine.** Withheld by default (`providers.node.keep_pnpm_store`); enabling it frees far less than its apparent size and breaks existing installs. |
| Yarn v1 / Berry caches | safe | next `yarn install` | |
| Bun install cache | safe | next `bun install` | |
| `~/.node-gyp` | safe | nodejs.org | Native-addon build headers. |
| Playwright / Puppeteer / Cypress / Electron | review | vendor CDN | Large downloads; automatic but slow on a poor link. |
| `node_modules` | safe *or* **caution** | npm registry *or* never | Safe when the project has a lockfile. **Caution with no lockfile** — a reinstall may resolve different versions. |
| `.next`, `.nuxt`, `.turbo`, `.parcel-cache`, `.svelte-kit`, `.angular` | safe | next build | |

## Python — `python.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| pip / uv / poetry / pipenv / pre-commit caches | safe | PyPI | |
| conda `pkgs` | review | conda channels | Hardlinked into every environment, same shape as the pnpm store. |
| Poetry virtualenvs | review | ~3 min rebuild | Recreated by `poetry install`. |
| `.venv` / `venv` | review *or* **caution** | rebuild *or* never | Only claimed when `pyvenv.cfg` is present, so a source directory named `env/` is never mistaken for one. **Caution when the project records no dependencies** — the installed packages exist nowhere else. |
| `.tox`, `.nox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `__pycache__` | safe | next run | |

## Rust — `rust.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| `registry/src` | safe | next `cargo build` | Re-extracted from the local `.crate` archives; needs no network. |
| `registry/cache` | review | crates.io | The archives themselves. Offline builds break until re-fetched. |
| `~/.cargo/git` | review | upstream repos | |
| sccache, rustup downloads/tmp | safe | next build | |
| Extra rustup toolchains | review | static.rust-lang.org | Only offered when more than one is installed, so the machine is never left unable to build. Warns that a project pinning it in `rust-toolchain.toml` will fail. |
| `target/` | safe | ~4 min rebuild | Usually the largest directory in a Rust project. |

## JVM — `jvm.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| `~/.gradle/{caches,daemon,native,wrapper/dists}` | safe | Maven Central / gradle.org | The reliable big win on a JVM machine. |
| `~/.m2/repository` | review *or* **caution** | Maven Central *or* never | A bounded scan looks for jars with no `_remote.repositories` marker. Those came from `mvn install` and **are not re-downloadable**; their presence promotes the whole repository to caution. |
| Ivy, sbt boot, Coursier, Kotlin/Native | review | configured repositories | |
| `build/` | safe | ~3 min rebuild | Only claimed for Gradle projects. |
| `target/` | safe | ~3 min rebuild | Only claimed for Maven projects — the name is used by too many other tools to claim on sight. |

## Go — `go.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| build cache | safe | next `go build` | |
| module cache | review | Go module proxy | Written **read-only** by the toolchain, so a plain `rm -rf` fails partway. Reclaimed via `go clean -modcache` when `go` is on PATH. |

## Apple / Xcode — `apple.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| DerivedData | safe | ~6 min rebuild | |
| SwiftPM / Xcode / Simulator caches | safe | next build | |
| CocoaPods cache | safe | CocoaPods CDN | |
| iOS/watchOS/tvOS DeviceSupport | review | ~10 min | Regenerated only when a device running that OS version is next **physically connected**. |
| **Xcode Archives** | **caution** | **never** | Contains the dSYMs needed to symbolicate crash reports from builds you have already shipped. Cannot be regenerated from source. |
| Simulator devices (all) | **caution** | ~5 min | Deletes app data, logins and databases inside every simulator. |
| Unavailable simulator devices | safe | — | `simctl delete unavailable` only removes devices whose runtime is already gone. |
| `Pods/` | safe *or* **caution** | CDN *or* never | Caution without `Podfile.lock`. |
| `Carthage/Build`, `.build`, project `DerivedData` | safe | rebuild | |

## Android — `android.*`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| System images | review | SDK Manager | One candidate per API level. Any AVD using it stops starting until reinstalled. |
| NDK versions | review | SDK Manager | Only when more than one is installed. |
| `.android/{cache,build-cache,tmp}` | safe | next build | |
| **AVDs** | **caution** | ~5 min | The emulator's full disk image: installed apps, accounts and app data are lost. The `.avd` directory and its `.ini` are always removed together, since leaving the `.ini` produces a broken AVD Manager entry. |

## Containers — `containers.*`

Everything here reclaims **through the daemon**, never by deleting paths: Docker's
storage is a single opaque disk image on macOS and removing files under it
corrupts the installation.

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| Build cache | safe | ~8 min | `docker builder prune`. Images and containers untouched. |
| Dangling images | safe | registry | Untagged layers only. |
| All unused images | review | registry | Includes tagged images. Anything built locally and never pushed is gone. |
| Volumes | **caution** | **never** | Off by default (`providers.containers.include_volumes`). Volumes hold database contents. |
| Docker Desktop disk image | **caution** | **never** | One file containing every image, container and volume — equivalent to a factory reset. |
| Colima / Lima / Podman machine data | **caution** | never | Same shape. |

## Others

| Provider | Items | Notes |
|---|---|---|
| `dotnet.*` | NuGet cache, `bin/`, `obj/` | |
| `ruby.*` | RubyGems cache, Bundler cache, `vendor/bundle` | Only the `.gem` archives, never the unpacked installation. Caution without `Gemfile.lock`. |
| `php.*` | Composer cache, `vendor/` | Caution without `composer.lock`. |
| `dart.*` | pub cache, Flutter engine cache, `.dart_tool`, `build/` | The engine cache is a multi-gigabyte re-download via `flutter precache`. |
| `buildtools.*` | ccache, Bazel disk cache, Bazelisk, Zig cache, CMake `build/` | `build/` is only claimed when it contains `CMakeCache.txt` — the name is far too common to claim on sight. |
| `editors.*` | VS Code / Cursor / VSCodium / Windsurf caches and logs, JetBrains caches | Workspace storage is **review**: it holds per-project state such as open tabs and local history. |
| `system.*` | `brew cleanup -s`, Nix GC, `~/Library/Logs` | Nix `-d` also deletes old profile generations, so rollback stops being possible. |
| `ml.*` | Hugging Face, PyTorch hub and LM Studio model caches; **one candidate per Ollama model**; ComfyUI, SillyTavern and Automatic1111 model/output/cache directories | **Disabled by default** — model weights are deliberate, very large downloads. Enable by removing `"ml"` from `providers.disabled`. ComfyUI, SillyTavern and Automatic1111 are found the same way project artifacts are — via the project walk, refined by a marker file unique to that tool (`.comfy_environment`, `public/scripts/world-info.js`, `webui.py` + `modules/paths_internal.py`). When a tool's default port answers right now, its candidates carry an extra warning that files may be in active use (`providers.ai.check_running`). |
| `ml.ollama` | One candidate per installed Ollama model, not the whole `~/.ollama/models` directory | Ollama's storage is content-addressed — models can share layer files — so each model's size counts only the blobs no other installed model needs, and removal goes through `ollama rm <model>` rather than deleting paths (a model sharing a layer with another gets a warning naming the shared amount instead). Staleness comes from the manifest's own mtime, or the blobs' atime when the filesystem records a more recent read — whichever is more recent, the same signal `ollama list`'s "modified" column can't give you. When `providers.ai.check_running` is on, a model currently loaded in Ollama's memory (`ollama ps`) carries its own warning, separate from the general "Ollama is running" one every model gets. |

## Large files — `files.large`

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| Any regular file ≥ `providers.files.min_size` (500 MB) under `~` | **caution** | **never** | Labelled with a guessed type — `Video · talk.mov`, `Disk image · Xcode.xip`, `Archive · backup.zip`. Found by a separate walk of the home directory that runs alongside the project walk; it skips dot-directories, `scan.exclude`, every directory `reclaim scan` already understands, and **macOS packages** (`.app`, `.photoslibrary`, `.sparsebundle`, …), whose inner files must never be removed one by one. A file inside another candidate's directory is offered only by that candidate. Removal is always to the **Trash**, even under `--purge`, after a fresh `check_user_file` guard check. Disable with `disabled = ["files"]`. |

## Unused apps — `apps.unused` (macOS)

| Item | Tier | Comes back | Notes |
|---|---|---|---|
| An `.app` bundle in `/Applications` or `~/Applications` (or one folder below) not opened for `providers.apps.unused_days` (90) | **caution** | **re-download** from the App Store or the developer | "Last opened" is the later of Spotlight's `kMDItemLastUsedDate` (read with one batched `mdls` call) and the access time of the program files in `Contents/MacOS`. Reads within five minutes of the file being written are ignored, because that is the installer or Gatekeeper, not a launch. With neither, the app is aged from Spotlight's date added or the bundle's mtime, and labelled "never opened". The age shown and used for ranking is this date, never the bundle's file times: an auto-update rewrites those. Symlinked bundles (Safari, which lives on the sealed system volume) are skipped, as are web-app shortcuts made by Chrome-family browsers (their `Info.plist` carries `CrAppModeShortcutID`). Removal is always to the **Trash**, even under `--purge`, after a fresh `check_app` guard check, and is refused while the app is running. Settings in `~/Library` are left alone. Apps owned by root (App Store or `.pkg` installs) carry a note that macOS will ask for a password. Disable with `disabled = ["apps"]`. |

## Adding a provider

One file in `crates/reclaim-providers/src/`, implementing `Provider`:

```rust
fn discover(&self, ctx: &ScanContext) -> Vec<Candidate>
```

It must be cheap — path existence checks and matching against `ctx.projects`, the
project list from the single stage-1 walk. It must never walk the filesystem,
measure sizes, or delete anything; the pipeline does all three.

Register it in `reclaim_providers::all()`, and give every caution-tier candidate a
warning explaining *why* — a `Caution` tier with no explanation is exactly the
failure this tool exists to prevent, and there is a test that enforces it.
