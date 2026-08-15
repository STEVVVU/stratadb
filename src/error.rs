use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the engine.
///
/// `Display` text is pinned by the `#[error(...)]` attributes below and is
/// identical to the previous hand-written implementation, because the CLI
/// prints these strings verbatim.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corruption in {path} at byte {offset}: {message}", path = path.display())]
    Corruption {
        path: PathBuf,
        offset: u64,
        message: String,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("database is already open: {}", .0.display())]
    Locked(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::Error;
    use std::path::PathBuf;

    /// The CLI prints `Display` output directly, so these strings are part of
    /// the observable interface. This test pins them across the move to
    /// `thiserror`.
    #[test]
    fn display_text_is_unchanged() {
        assert_eq!(
            Error::InvalidInput("bad key".to_owned()).to_string(),
            "invalid input: bad key"
        );
        assert_eq!(
            Error::Locked(PathBuf::from("/db")).to_string(),
            "database is already open: /db"
        );
        assert_eq!(
            Error::Corruption {
                path: PathBuf::from("/db/segment-1.wal"),
                offset: 42,
                message: "checksum mismatch".to_owned(),
            }
            .to_string(),
            "corruption in /db/segment-1.wal at byte 42: checksum mismatch"
        );
        let io = Error::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert_eq!(io.to_string(), "I/O error: denied");
    }

    /// `source()` previously returned the inner error for `Io` and `None` for
    /// every other variant. `#[from]` preserves that.
    #[test]
    fn source_is_exposed_only_for_io() {
        use std::error::Error as _;

        let io = Error::from(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(io.source().is_some());
        assert!(Error::InvalidInput("x".to_owned()).source().is_none());
        assert!(Error::Locked(PathBuf::from("/db")).source().is_none());
        assert!(Error::Corruption {
            path: PathBuf::from("/db"),
            offset: 0,
            message: String::new(),
        }
        .source()
        .is_none());
    }
}
