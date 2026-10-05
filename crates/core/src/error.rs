use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FTP: {0}")]
    Ftp(String),
    #[error("could not reach {host}:{port}: {reason}")]
    Connect { host: String, port: u16, reason: String },
    #[error("the Xbox refused the login for user \"{0}\"")]
    Login(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{path}: {source}")]
    Xbe { path: String, source: xbe::XbeError },
    #[error("{path}: expected {expected} bytes, the Xbox sent {got}")]
    ShortRead { path: String, expected: u64, got: u64 },
    #[error("building the image failed: {0}")]
    Image(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timed out: {0}")]
    Timeout(String),
}

impl From<suppaftp::FtpError> for Error {
    fn from(e: suppaftp::FtpError) -> Self {
        Error::Ftp(e.to_string())
    }
}

impl<E: fmt::Debug + fmt::Display + Into<Error>> From<xdvdfs::util::Error<E>> for Error {
    fn from(e: xdvdfs::util::Error<E>) -> Self {
        match e {
            xdvdfs::util::Error::IOError(inner) => inner.into(),
            other => Error::Image(other.to_string()),
        }
    }
}
