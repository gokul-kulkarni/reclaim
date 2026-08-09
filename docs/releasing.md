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

1. Bump `version` in the workspace `Cargo.toml`, and in
   `packaging/homebrew/reclaim.rb` if you are not relying on the workflow to
   rewrite it.
2. `cargo test --workspace && ./scripts/brew-test.sh`
3. Commit, tag, push:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

The release workflow then builds four targets
(`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-musl`), publishes tarballs and checksums to a GitHub
Release alongside `install.sh`, and updates the tap.

## First-time setup for the Homebrew tap

The tap does not exist yet. To create it:

1. Create a public repo named **`homebrew-tap`** under your account. The
   `homebrew-` prefix is what makes `brew tap gokul-kulkarni/tap` work.
2. Add a `Formula/` directory. The release workflow writes
   `Formula/reclaim.rb` into it.
3. Create a fine-grained personal access token with **Contents: read and write**
   on that repo, and add it to the `reclaim` repo as the secret
   **`TAP_GITHUB_TOKEN`**.

Without that secret the `homebrew` job logs a warning and skips; the release is
still published and installable via `install.sh` and `cargo install`.

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
