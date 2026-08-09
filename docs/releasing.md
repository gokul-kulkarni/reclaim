# Releasing

## Testing packaging locally

Before any of this touches the internet, the whole install path can be exercised
on your own machine.

```sh
./scripts/package.sh
```

Builds `dist/reclaim-<target>.tar.gz` and its `.sha256`, in exactly the layout
the release workflow produces and the Homebrew formula expects. Pass
`--skip-web` to reuse an existing `web/dist` and skip the npm build.

```sh
./scripts/brew-test.sh
```

The one that matters. It:

1. builds the tarball,
2. creates a throwaway local tap (`reclaim-local/test`),
3. writes a formula pointing at the tarball on disk with its real checksum —
   lifting the `install` and `test` blocks verbatim from
   `packaging/homebrew/reclaim.rb`, so the two cannot drift,
4. runs `brew install`, `brew test` and `brew audit --strict`,
5. smoke-tests the installed binary against a sandbox home,
6. checks the bash, zsh and fish completions actually landed,
7. uninstalls and untaps.

Pass `--keep` to leave it installed. It is safe to run repeatedly.

This is a genuine test, not a simulation: the only difference from a real
`brew install` is that the tarball comes from `file://` rather than a GitHub
release. It has already caught one bug that unit tests could not — Homebrew's
`shell_parameter_format: :arg` invokes `reclaim completions --shell=bash`, which
the CLI rejects because it takes the shell as a positional argument.

## Cutting a release

Builds are split across two machines, deliberately. CI only builds the two
Linux targets. GitHub's macOS runner capacity — especially the Intel
(`x86_64-apple-darwin`, `macos-13`) image — is small, and a build has been
observed queueing indefinitely with zero progress rather than failing or
completing. Both macOS targets cross-compile natively from a single Apple
Silicon machine via Xcode's own toolchain (no Docker, no second Mac, no
queue), so they are built and attached locally instead. If GitHub's macOS
runners become reliable again, `release.yml`'s `build` matrix is the only
place that would need to change back.

1. Bump `version` in the workspace `Cargo.toml`. Every crate uses
   `version.workspace = true`, so this is the only place it lives.
   `packaging/homebrew/reclaim.rb`'s version is rewritten by
   `update-tap.sh` below and does not need editing by hand.
2. `cargo test --workspace && ./scripts/brew-test.sh`
3. Commit, tag, push:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

   This triggers `release.yml`: it builds `x86_64-unknown-linux-gnu` and
   `aarch64-unknown-linux-musl`, and publishes them as a GitHub Release along
   with `install.sh`. Watch it with `gh run watch` or the Actions tab.

4. Once that `publish` job has finished (the release exists, with two
   tarballs), attach the macOS builds from a Mac:

   ```sh
   git checkout v0.1.0        # if not already on it
   ./scripts/release-macos.sh v0.1.0
   ```

   This builds `aarch64-apple-darwin` and `x86_64-apple-darwin` and uploads
   them to the same release via `gh release upload`.

5. Once all four tarballs are attached (`gh release view v0.1.0` to check),
   render and push the Homebrew formula:

   ```sh
   ./scripts/update-tap.sh 0.1.0
   ```

   This reads the four `.sha256` files already on the release — it does not
   care whether a given tarball came from CI or from step 4 — renders
   `packaging/homebrew/reclaim.rb` with the real version and checksums, and
   pushes it to `homebrew-tap`. It refuses to run if any of the four
   checksums are missing, so a partial release can't produce a formula that
   404s for one platform.

## First-time setup for the Homebrew tap

The tap does not exist yet. To create it:

1. Create a public repo named **`homebrew-tap`** under your account. The
   `homebrew-` prefix is what makes `brew tap gokul-kulkarni/tap` work.
2. Add a `Formula/` directory. `update-tap.sh` writes `Formula/reclaim.rb`
   into it.

`update-tap.sh` pushes using your own local `gh` authentication, the same
credential `brew-test.sh` already uses for its throwaway test tap — no
separate token needed for this path. (A `TAP_GITHUB_TOKEN` repo secret was set
up earlier for a CI-driven version of this step; it isn't used by anything
currently, since the tap update now happens locally. Harmless to leave in
place if you want to move this back into CI later.)

After the first release, verify it end to end from a clean shell:

```sh
brew tap gokul-kulkarni/tap
brew install reclaim
brew test reclaim
reclaim --version
```

## Publishing to crates.io

`reclaim` is taken, so the crate is `reclaim-cli` with `[[bin]] name = "reclaim"`.
Publish bottom-up, since the crates depend on each other:

```sh
cargo publish -p reclaim-core
cargo publish -p reclaim-providers
cargo publish -p reclaim-web
cargo publish -p reclaim-cli
```

`cargo install reclaim-cli` builds from source and therefore ships the
"frontend not built" fallback page for `reclaim ui` unless `web/dist` is
committed or vendored. The CLI and TUI are unaffected.
