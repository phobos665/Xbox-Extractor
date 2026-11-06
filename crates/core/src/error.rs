use std::fmt;
use std::io::ErrorKind;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FTP: {0}")]
    Ftp(String),
    /// A failure a fresh session and another try may get past: a dropped connection, a reply
    /// that came out of order, or a `4xx`/`502` from a server that was busy. See
    /// [`Error::is_transient`].
    #[error("FTP: {0}")]
    Transient(String),
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
    #[error("not enough space on {place}: this needs {}, and {} is free", fmt_bytes(*.needed), fmt_bytes(*.free))]
    NoSpace { place: String, needed: u64, free: u64 },
}

impl Error {
    /// Whether reconnecting and trying again may succeed. Xbox FTP servers drop idle sessions,
    /// answer `502` or `4xx` when busy, and stall for seconds after a transfer is cut off; all
    /// of these clear up on a fresh session.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Transient(_) | Error::Timeout(_) | Error::Connect { .. } => true,
            Error::Io(e) => is_network_io(e),
            _ => false,
        }
    }

    /// Wraps a `suppaftp` error with what was being done, keeping whether it is transient.
    pub fn ftp(context: impl fmt::Display, e: suppaftp::FtpError) -> Error {
        let msg = format!("{context}: {e}");
        if ftp_error_is_transient(&e) { Error::Transient(msg) } else { Error::Ftp(msg) }
    }
}

fn is_network_io(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected
    )
}

fn ftp_error_is_transient(e: &suppaftp::FtpError) -> bool {
    use suppaftp::FtpError as F;
    match e {
        F::ConnectionError(_) | F::BadResponse | F::DataConnectionAlreadyOpen => true,
        F::UnexpectedResponse(r) => reply_is_transient(r.status.code()),
        _ => false,
    }
}

/// `4xx` replies are temporary by definition, except `452` (out of space), which another try
/// will not fix. `502` and `503` are not, but Xbox servers send them when a reply has come out
/// of step with its command, or when they are still recovering from an abandoned transfer.
pub(crate) fn reply_is_transient(code: u32) -> bool {
    ((400..500).contains(&code) && code != 452) || code == 502 || code == 503
}

/// Whether a reply means the drive is full.
pub(crate) fn reply_is_no_space(e: &suppaftp::FtpError) -> bool {
    matches!(e, suppaftp::FtpError::UnexpectedResponse(r) if matches!(r.status.code(), 452 | 552))
}

impl From<suppaftp::FtpError> for Error {
    fn from(e: suppaftp::FtpError) -> Self {
        if ftp_error_is_transient(&e) { Error::Transient(e.to_string()) } else { Error::Ftp(e.to_string()) }
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

/// Formats a byte count for messages, e.g. `4.2 GB`.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_replies() {
        assert!(reply_is_transient(421));
        assert!(reply_is_transient(425));
        assert!(reply_is_transient(426));
        assert!(reply_is_transient(502));
        assert!(!reply_is_transient(452));
        assert!(!reply_is_transient(550));
        assert!(!reply_is_transient(530));
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(3 << 30), "3.0 GB");
    }
}
