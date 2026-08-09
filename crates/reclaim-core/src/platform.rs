//! Platform-specific path resolution.
//!
//! macOS and Linux put developer caches in genuinely different places
//! (`~/Library/Caches/foo` vs `~/.cache/foo`), so every provider asks this module
//! rather than hardcoding one layout. Windows compiles and resolves what it can;
//! providers that have no Windows equivalent simply find no paths and emit nothing.
//!
//! **Every base directory is resolved once, at construction.** [`Paths::from_env`]
//! reads `$HOME`, `$XDG_*`, `$CARGO_HOME`, `$GOPATH` and friends;
//! [`Paths::with_home`] reads nothing and derives everything from the home it is
//! given. That split is what makes a sandboxed home actually hermetic — otherwise
//! a provider consulting `$CARGO_HOME` at call time escapes the sandbox and, under
//! `cargo test` (which always sets `CARGO_HOME`), reaches the developer's real
//! cache directory.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Resolved base directories for the current user and platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
    cache_dir: PathBuf,
    config_dir: PathBuf,
    state_dir: PathBuf,
    cargo_home: PathBuf,
    gopath: PathBuf,
    go_mod_cache: PathBuf,
    go_build_cache: PathBuf,
    android_sdk: Option<PathBuf>,
    android_avd: PathBuf,
    tmpdir: Option<PathBuf>,
    sandboxed: bool,
}

impl Paths {
    /// Resolve from the environment, honouring the standard override variables.
    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
            .ok_or(Error::NoHome)?;

        let base = Self::with_home(&home);
        let env = |key: &str| std::env::var_os(key).map(PathBuf::from);

        let cache_dir = env("XDG_CACHE_HOME").unwrap_or(base.cache_dir);
        let gopath = env("GOPATH").unwrap_or(base.gopath);

        Ok(Self {
            cache_dir: cache_dir.clone(),
            config_dir: env("XDG_CONFIG_HOME").unwrap_or(base.config_dir),
            state_dir: env("XDG_STATE_HOME").unwrap_or(base.state_dir),
            cargo_home: env("CARGO_HOME").unwrap_or(base.cargo_home),
            go_mod_cache: env("GOMODCACHE").unwrap_or_else(|| gopath.join("pkg/mod")),
            go_build_cache: env("GOCACHE").unwrap_or_else(|| cache_dir.join("go-build")),
            gopath,
            android_sdk: env("ANDROID_HOME")
                .or_else(|| env("ANDROID_SDK_ROOT"))
                .or(base.android_sdk),
            android_avd: env("ANDROID_AVD_HOME").unwrap_or(base.android_avd),
            tmpdir: env("TMPDIR"),
            sandboxed: false,
            home,
        })
    }

    /// Derive every directory from an explicit home, consulting no environment
    /// variables at all. Used by every test fixture, and by `--root`.
    ///
    /// The result is marked [`sandboxed`](Self::is_sandboxed): the caller has said
    /// "operate on this home instead of the real one", so anything that acts on
    /// machine-global state rather than on paths must be suppressed.
    pub fn with_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref().to_path_buf();

        let cache_dir = if cfg!(target_os = "macos") {
            home.join("Library/Caches")
        } else {
            home.join(".cache")
        };
        let gopath = home.join("go");

        let android_sdk = if cfg!(target_os = "macos") {
            Some(home.join("Library/Android/sdk"))
        } else if cfg!(target_os = "linux") {
            Some(home.join("Android/Sdk"))
        } else {
            None
        };

        Self {
            // `~/.config` on macOS too: developer tools overwhelmingly use it there,
            // and a dotfile-managed config is far more useful to this audience than
            // `~/Library/Application Support`.
            config_dir: home.join(".config"),
            state_dir: home.join(".local/state"),
            cargo_home: home.join(".cargo"),
            go_mod_cache: gopath.join("pkg/mod"),
            go_build_cache: cache_dir.join("go-build"),
            android_avd: home.join(".android/avd"),
            android_sdk,
            gopath,
            cache_dir,
            tmpdir: None,
            sandboxed: true,
            home,
        }
    }

    /// True when these paths were pointed at an explicit home rather than the
    /// real one (`--root`, or a test fixture).
    ///
    /// Providers that reclaim by shelling out — `brew cleanup`, `docker prune`,
    /// `simctl delete` — act on machine-global state that no sandbox can
    /// represent. Running them here would mutate the real machine while the user
    /// believes they are working against a scratch directory, so the pipeline
    /// drops those candidates entirely when this is set.
    pub fn is_sandboxed(&self) -> bool {
        self.sandboxed
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `~/<rest>`
    pub fn home_join(&self, rest: impl AsRef<Path>) -> PathBuf {
        self.home.join(rest)
    }

    /// `~/Library/Caches` on macOS, `$XDG_CACHE_HOME` or `~/.cache` elsewhere.
    pub fn cache_dir(&self) -> PathBuf {
        self.cache_dir.clone()
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_dir.clone()
    }

    /// Mutable state we own: the run journal.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }

    /// Our own config file, `~/.config/reclaim/config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("reclaim/config.toml")
    }

    /// Our own journal directory, `~/.local/state/reclaim/history`.
    pub fn journal_dir(&self) -> PathBuf {
        self.state_dir.join("reclaim/history")
    }

    /// macOS `~/Library/Developer`. `None` on other platforms.
    pub fn xcode_dir(&self) -> Option<PathBuf> {
        cfg!(target_os = "macos").then(|| self.home.join("Library/Developer"))
    }

    pub fn android_sdk(&self) -> Option<PathBuf> {
        self.android_sdk.clone()
    }

    pub fn android_avd(&self) -> PathBuf {
        self.android_avd.clone()
    }

    pub fn cargo_home(&self) -> PathBuf {
        self.cargo_home.clone()
    }

    pub fn gopath(&self) -> PathBuf {
        self.gopath.clone()
    }

    pub fn go_mod_cache(&self) -> PathBuf {
        self.go_mod_cache.clone()
    }

    pub fn go_build_cache(&self) -> PathBuf {
        self.go_build_cache.clone()
    }

    pub fn tmpdir(&self) -> Option<&Path> {
        self.tmpdir.as_deref()
    }

    /// Override a base directory. Lets tests point one toolchain at the sandbox
    /// without disturbing the process environment, which parallel tests share.
    pub fn with_override(mut self, which: Base, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        match which {
            Base::CargoHome => self.cargo_home = path,
            Base::GoPath => {
                self.go_mod_cache = path.join("pkg/mod");
                self.gopath = path;
            }
            Base::GoBuildCache => self.go_build_cache = path,
            Base::AndroidSdk => self.android_sdk = Some(path),
            Base::AndroidAvd => self.android_avd = path,
            Base::CacheDir => self.cache_dir = path,
        }
        self
    }

    /// Expand a leading `~` and any `$VAR` references in a configured path.
    pub fn expand(&self, raw: &str) -> PathBuf {
        let expanded = expand_env_vars(raw);
        if expanded == "~" {
            return self.home.clone();
        }
        if let Some(rest) = expanded.strip_prefix("~/") {
            return self.home.join(rest);
        }
        PathBuf::from(expanded)
    }

    /// Inverse of [`Self::expand`], for display: `/Users/x/.npm` -> `~/.npm`.
    pub fn contract(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

/// Which base directory an override targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    CargoHome,
    GoPath,
    GoBuildCache,
    AndroidSdk,
    AndroidAvd,
    CacheDir,
}

/// Substitute `$VAR` and `${VAR}`. Unknown variables expand to empty, matching shell behaviour.
fn expand_env_vars(input: &str) -> String {
    if !input.contains('$') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            if braced && next == '}' {
                chars.next();
                break;
            }
            if !(next.is_ascii_alphanumeric() || next == '_') {
                break;
            }
            name.push(next);
            chars.next();
        }
        if name.is_empty() {
            out.push('$');
        } else if let Some(value) = std::env::var_os(&name) {
            out.push_str(&value.to_string_lossy());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::with_home("/home/tester")
    }

    #[test]
    fn with_home_reads_no_environment_variables() {
        // Regression: `cargo test` always sets CARGO_HOME, so a Paths that consulted
        // it at call time reached the developer's real ~/.cargo from inside a
        // sandboxed test home.
        std::env::set_var("CARGO_HOME", "/somewhere/else/cargo");
        std::env::set_var("XDG_CACHE_HOME", "/somewhere/else/cache");
        std::env::set_var("GOPATH", "/somewhere/else/go");

        let p = Paths::with_home("/home/tester");
        assert_eq!(p.cargo_home(), PathBuf::from("/home/tester/.cargo"));
        assert_eq!(p.gopath(), PathBuf::from("/home/tester/go"));
        assert!(p.cache_dir().starts_with("/home/tester"));

        std::env::remove_var("CARGO_HOME");
        std::env::remove_var("XDG_CACHE_HOME");
        std::env::remove_var("GOPATH");
    }

    #[test]
    fn every_derived_path_stays_under_the_given_home() {
        let p = Paths::with_home("/home/tester");
        for path in [
            p.cache_dir(),
            p.config_dir(),
            p.state_dir(),
            p.config_file(),
            p.journal_dir(),
            p.cargo_home(),
            p.gopath(),
            p.go_mod_cache(),
            p.go_build_cache(),
            p.android_avd(),
        ] {
            assert!(
                path.starts_with("/home/tester"),
                "{} escaped the home",
                path.display()
            );
        }
    }

    #[test]
    fn overrides_redirect_a_single_base_without_touching_the_environment() {
        let p = Paths::with_home("/home/tester").with_override(Base::CargoHome, "/sandbox/cargo");
        assert_eq!(p.cargo_home(), PathBuf::from("/sandbox/cargo"));
        assert_eq!(
            p.gopath(),
            PathBuf::from("/home/tester/go"),
            "other bases are unaffected"
        );
    }

    #[test]
    fn overriding_gopath_also_moves_the_module_cache() {
        let p = Paths::with_home("/home/tester").with_override(Base::GoPath, "/sandbox/go");
        assert_eq!(p.go_mod_cache(), PathBuf::from("/sandbox/go/pkg/mod"));
    }

    #[test]
    fn expand_resolves_tilde() {
        assert_eq!(paths().expand("~/dev"), PathBuf::from("/home/tester/dev"));
        assert_eq!(paths().expand("~"), PathBuf::from("/home/tester"));
    }

    #[test]
    fn expand_leaves_absolute_paths_alone() {
        assert_eq!(paths().expand("/opt/thing"), PathBuf::from("/opt/thing"));
    }

    #[test]
    fn expand_does_not_treat_a_bare_tilde_prefix_as_home() {
        // `~foo` is another user's home in shell, which we deliberately do not
        // support; treating it as a subdirectory of our home would be badly wrong.
        assert_eq!(paths().expand("~foo"), PathBuf::from("~foo"));
    }

    #[test]
    fn contract_is_the_inverse_of_expand() {
        let p = paths();
        assert_eq!(p.contract(&p.expand("~/.npm")), "~/.npm");
        assert_eq!(p.contract(&p.expand("~")), "~");
        assert_eq!(p.contract(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn env_vars_expand_in_both_spellings() {
        std::env::set_var("RECLAIM_TEST_VAR", "xyz");
        assert_eq!(expand_env_vars("a/$RECLAIM_TEST_VAR/b"), "a/xyz/b");
        assert_eq!(expand_env_vars("a/${RECLAIM_TEST_VAR}/b"), "a/xyz/b");
        std::env::remove_var("RECLAIM_TEST_VAR");
    }

    #[test]
    fn unknown_env_vars_expand_to_empty_like_a_shell() {
        assert_eq!(expand_env_vars("a/$RECLAIM_DEFINITELY_UNSET_VAR/b"), "a//b");
    }

    #[test]
    fn a_lone_dollar_sign_survives() {
        assert_eq!(expand_env_vars("cost: $"), "cost: $");
    }

    #[test]
    fn cache_dir_follows_the_platform_convention() {
        let p = Paths::with_home("/home/tester");
        if cfg!(target_os = "macos") {
            assert!(p.cache_dir().ends_with("Library/Caches"));
        } else {
            assert!(p.cache_dir().ends_with(".cache"));
        }
    }
}
