//! Typed errors for the core engine.
//!
//! Every variant carries the path or value that caused it: an error message a
//! user sees while a tool is about to delete their files must say exactly which
//! file it is talking about.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("refusing to touch {path}: {reason}")]
    Refused { path: PathBuf, reason: String },

    #[error("config error in {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config error: {0}")]
    Config(String),

    #[error("could not determine the home directory")]
    NoHome,

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to move {path} to the trash: {source}")]
    Trash {
        path: PathBuf,
        #[source]
        source: trash::Error,
    },

    #[error("`{program}` failed with status {status}: {stderr}")]
    Command {
        program: String,
        status: String,
        stderr: String,
    },

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn refused(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Refused {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// True when the failure is the filesystem telling us the path is simply gone,
    /// which is a no-op for a deletion tool rather than a real failure.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_message_names_the_path() {
        let err = Error::refused("/etc", "outside the allowed roots");
        let msg = err.to_string();
        assert!(msg.contains("/etc"), "{msg}");
        assert!(msg.contains("outside the allowed roots"), "{msg}");
    }

    #[test]
    fn missing_paths_are_recognised_as_benign() {
        let missing = Error::io("/gone", std::io::Error::from(std::io::ErrorKind::NotFound));
        let denied = Error::io(
            "/root",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(missing.is_not_found());
        assert!(!denied.is_not_found());
    }
}
