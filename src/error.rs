use std::fmt::{Display, Formatter};
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Corruption {
        path: PathBuf,
        offset: u64,
        message: String,
    },
    InvalidInput(String),
    Locked(PathBuf),
}

impl Display for Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Corruption {
                path,
                offset,
                message,
            } => write!(
                formatter,
                "corruption in {} at byte {offset}: {message}",
                path.display()
            ),
            Self::InvalidInput(message) => write!(formatter, "invalid input: {message}"),
            Self::Locked(path) => {
                write!(formatter, "database is already open: {}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corruption { .. } | Self::InvalidInput(_) | Self::Locked(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
