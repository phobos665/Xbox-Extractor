//! An FTP session with an Xbox dashboard's FTP server.
//!
//! Xbox FTP servers (UnleashX, EvolutionX, Avalaunch, XBMC) are small and old. This keeps to
//! the commands they all answer: `CWD`, `LIST`, `RETR`, `REST` and `ABOR`, in passive mode with
//! binary transfers. It changes directory before listing or fetching rather than passing a path,
//! because not every server accepts a path argument to `LIST`.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use suppaftp::Mode;
use tokio::io::AsyncReadExt;

use crate::listparse;
use crate::{Error, Result};

pub type Session = suppaftp::tokio::ImplAsyncFtpStream<suppaftp::tokio::AsyncNoTlsStream>;
pub type TransferStream = suppaftp::tokio::TransferStream<suppaftp::tokio::AsyncNoTlsStream>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

impl Default for ConnectionInfo {
    /// The credentials almost every softmodded or chipped Xbox ships with.
    fn default() -> Self {
        Self { host: String::new(), port: 21, user: "xbox".into(), password: "xbox".into() }
    }
}

impl ConnectionInfo {
    pub fn new(host: impl Into<String>) -> Self {
        Self { host: host.into(), ..Self::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Joins remote path components with `/`, keeping a single leading slash.
pub fn join(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    if name.is_empty() {
        if base.is_empty() { "/".into() } else { base.into() }
    } else {
        format!("{}/{}", base, name.trim_start_matches('/'))
    }
}

/// Picks the line of a (possibly multi-line) `220` greeting that names the server: the final
/// `220 ` line, which ends a multi-line reply, else the first non-empty line.
pub fn server_name(welcome: &str) -> String {
    let lines: Vec<&str> = welcome.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let last = lines.iter().rev().find(|l| l.starts_with("220 ")).or(lines.first());
    last.map(|l| l.trim_start_matches("220").trim_start_matches(['-', ' ']).trim().to_string())
        .unwrap_or_default()
}

/// Splits a remote path into its directory and final name.
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path),
    }
}

/// How much of a file [`XboxFtp::read_prefix`] will read and discard to end a transfer
/// cleanly rather than cut it off. At the Xbox's ~8 MB/s this is about a second, against two to
/// three seconds for UnleashX to recover from an abandoned transfer.
pub const DRAIN_LIMIT: u64 = 8 << 20;

pub struct XboxFtp {
    info: ConnectionInfo,
    stream: Session,
    welcome: Option<String>,
    timeout: Duration,
    /// The server answers `REST` but sends from the start of the file anyway (UnleashX does).
    /// Learnt by [`XboxFtp::read_at`] and kept across reconnects.
    rest_ignored: bool,
}

impl XboxFtp {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

    pub async fn connect(info: &ConnectionInfo) -> Result<Self> {
        Self::connect_with_timeout(info, Self::DEFAULT_TIMEOUT).await
    }

    pub async fn connect_with_timeout(info: &ConnectionInfo, timeout: Duration) -> Result<Self> {
        let (stream, welcome) = open(info, timeout).await?;
        Ok(Self { info: info.clone(), stream, welcome, timeout, rest_ignored: false })
    }

    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    /// The server's whole greeting. UnleashX's runs to a dozen lines of drive statistics.
    pub fn welcome(&self) -> Option<&str> {
        self.welcome.as_deref()
    }

    /// The line of the greeting that names the server, e.g. `UnleashX FTP Server ready.`
    pub fn server_name(&self) -> Option<String> {
        self.welcome.as_deref().map(server_name)
    }

    /// Drops the session and logs in again. Used after a transfer the server would not abort
    /// cleanly, which leaves the control connection in an unknown state.
    pub async fn reconnect(&mut self) -> Result<()> {
        let (stream, welcome) = open(&self.info, self.timeout).await?;
        self.stream = stream;
        self.welcome = welcome;
        Ok(())
    }

    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        let p = if path.is_empty() { "/" } else { path };
        let t = self.timeout;
        match tokio::time::timeout(t, self.stream.cwd(p)).await {
            Ok(r) => r.map_err(|e| Error::Ftp(format!("cannot open {p}: {e}"))),
            Err(_) => Err(Error::Timeout(format!("CWD {p}"))),
        }
    }

    /// Lists a directory. Lines the parser cannot read are logged and skipped.
    pub async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        self.cwd(path).await?;
        let t = self.timeout;
        let lines = match tokio::time::timeout(t, self.stream.list(None)).await {
            Ok(r) => r?,
            Err(_) => return Err(Error::Timeout(format!("LIST {path}"))),
        };
        let (entries, unparsed) = listparse::parse_listing(&lines);
        for l in unparsed {
            log::warn!("LIST {path}: could not read line {l:?}");
        }
        Ok(entries
            .into_iter()
            .map(|e| RemoteEntry { name: e.name, is_dir: e.is_dir, size: e.size })
            .collect())
    }

    /// Opens `path` for reading, optionally from `offset` (`REST`). The caller must end the
    /// transfer with [`XboxFtp::end_transfer`].
    pub async fn open_read(&mut self, path: &str, offset: u64) -> Result<TransferStream> {
        let (dir, name) = split(path);
        self.cwd(dir).await?;
        if offset > 0 {
            let off = usize::try_from(offset).map_err(|_| Error::Ftp("offset too large".into()))?;
            timed(self.timeout, "REST", self.stream.resume_transfer(off)).await?;
        }
        timed(self.timeout, &format!("RETR {path}"), self.stream.retr_as_stream(name)).await
    }

    /// Ends a transfer. `complete` says whether the caller read to the end of the file.
    ///
    /// A transfer stopped early is not aborted with `ABOR`: servers answer it with different
    /// sequences of 426 and 226 replies, and a reply left unread is taken by the next command
    /// as its own answer. Opening a fresh session costs one login and is always clean.
    pub async fn end_transfer(&mut self, stream: TransferStream, complete: bool) -> Result<()> {
        if complete {
            match tokio::time::timeout(self.timeout, stream.finish()).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) => log::warn!("closing transfer: {e}; reconnecting"),
                Err(_) => log::warn!("closing transfer timed out; reconnecting"),
            }
        } else {
            drop(stream);
        }
        self.reconnect().await
    }

    /// Reads from `offset` until `target` says enough bytes have arrived, at most `cap` bytes,
    /// or the end of the file. `target` sees the bytes read so far and returns the total length
    /// wanted once it can tell (for an XBE, once the header-size field has arrived).
    ///
    /// `file_size`, when known, lets a read that ends close to the end of the file finish the
    /// transfer instead of cutting it off (see [`DRAIN_LIMIT`]).
    pub async fn read_prefix<F>(
        &mut self,
        path: &str,
        offset: u64,
        cap: usize,
        file_size: Option<u64>,
        mut target: F,
    ) -> Result<Vec<u8>>
    where
        F: FnMut(&[u8]) -> Option<usize>,
    {
        let mut stream = self.open_read(path, offset).await?;
        let mut buf = Vec::with_capacity(cap.min(1 << 20));
        let mut want = cap;
        let mut eof = false;
        let mut chunk = vec![0u8; 64 * 1024];
        while buf.len() < want {
            let n = match tokio::time::timeout(self.timeout, stream.read(&mut chunk)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    let _ = self.end_transfer(stream, false).await;
                    return Err(e.into());
                }
                Err(_) => {
                    let _ = self.end_transfer(stream, false).await;
                    return Err(Error::Timeout(format!("reading {path}")));
                }
            };
            if n == 0 {
                eof = true;
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(t) = target(&buf) {
                want = t.min(cap);
            }
        }
        buf.truncate(want.min(buf.len()));

        // Stopping a download early costs a fresh login, and UnleashX stalls for seconds on a
        // data connection closed under it. When little of the file is left it is cheaper to
        // read the rest and close the transfer cleanly.
        if !eof {
            let remaining = file_size.map(|s| s.saturating_sub(offset + want as u64));
            if remaining.is_some_and(|r| r <= DRAIN_LIMIT) {
                loop {
                    match tokio::time::timeout(self.timeout, stream.read(&mut chunk)).await {
                        Ok(Ok(0)) => {
                            eof = true;
                            break;
                        }
                        Ok(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
        }
        self.end_transfer(stream, eof).await?;
        Ok(buf)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn rest_ignored(&self) -> bool {
        self.rest_ignored
    }

    /// Reads `len` bytes at `offset`. Uses `REST` unless the server is known to ignore it, and
    /// finds that out when `plausible` rejects what a `REST` read returned (for an XBE section,
    /// data that starts with the XBE's own magic). Without `REST` it reads from the start and
    /// discards up to `offset`.
    pub async fn read_at<P>(
        &mut self,
        path: &str,
        offset: u64,
        len: usize,
        file_size: Option<u64>,
        plausible: P,
    ) -> Result<Vec<u8>>
    where
        P: Fn(&[u8]) -> bool,
    {
        if offset == 0 || !self.rest_ignored {
            let data = self.read_prefix(path, offset, len, file_size, |_| Some(len)).await?;
            if offset == 0 || plausible(&data) {
                return Ok(data);
            }
            log::info!("{}: REST is ignored; reading through instead", self.info.host);
            self.rest_ignored = true;
        }
        self.read_skipping(path, offset, len, file_size).await
    }

    async fn read_skipping(&mut self, path: &str, offset: u64, len: usize, file_size: Option<u64>) -> Result<Vec<u8>> {
        let mut stream = self.open_read(path, 0).await?;
        let mut chunk = vec![0u8; 256 * 1024];
        let mut pos = 0u64;
        let mut out = Vec::with_capacity(len);
        let end = offset + len as u64;
        let mut eof = false;
        while pos < end {
            let n = match tokio::time::timeout(self.timeout, stream.read(&mut chunk)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    let _ = self.end_transfer(stream, false).await;
                    return Err(e.into());
                }
                Err(_) => {
                    let _ = self.end_transfer(stream, false).await;
                    return Err(Error::Timeout(format!("reading {path}")));
                }
            };
            if n == 0 {
                eof = true;
                break;
            }
            let (lo, hi) = (pos, pos + n as u64);
            if hi > offset {
                let from = offset.saturating_sub(lo) as usize;
                let to = (end.min(hi) - lo) as usize;
                out.extend_from_slice(&chunk[from..to]);
            }
            pos = hi;
        }
        if !eof && file_size.is_some_and(|s| s.saturating_sub(pos) <= DRAIN_LIMIT) {
            while let Ok(Ok(n)) = tokio::time::timeout(self.timeout, stream.read(&mut chunk)).await {
                if n == 0 {
                    eof = true;
                    break;
                }
            }
        }
        self.end_transfer(stream, eof).await?;
        Ok(out)
    }

    pub async fn quit(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.stream.quit()).await;
    }
}

async fn timed<T, F>(timeout: Duration, what: &str, f: F) -> Result<T>
where
    F: std::future::Future<Output = suppaftp::FtpResult<T>>,
{
    match tokio::time::timeout(timeout, f).await {
        Ok(r) => r.map_err(Error::from),
        Err(_) => Err(Error::Timeout(what.to_string())),
    }
}

async fn open(info: &ConnectionInfo, timeout: Duration) -> Result<(Session, Option<String>)> {
    let cerr = |reason: String| Error::Connect { host: info.host.clone(), port: info.port, reason };
    let addr: SocketAddr = tokio::net::lookup_host((info.host.as_str(), info.port))
        .await
        .map_err(|e| cerr(e.to_string()))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| cerr("no IPv4 address".into()))?;

    let mut s = AsyncFtpStream::connect_timeout(addr, timeout)
        .await
        .map_err(|e| cerr(e.to_string()))?;
    let welcome = s.get_welcome_msg().map(|w| w.trim().to_string());
    match tokio::time::timeout(timeout, s.login(info.user.as_str(), info.password.as_str())).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err(Error::Login(info.user.clone())),
        Err(_) => return Err(Error::Timeout("login".into())),
    }
    s.set_mode(Mode::Passive);
    // Some Xbox servers put an unroutable address in their PASV reply; connect the data channel
    // to the address the control connection already reached instead.
    s.set_passive_nat_workaround(true);
    match tokio::time::timeout(timeout, s.transfer_type(FileType::Image)).await {
        Ok(r) => r?,
        Err(_) => return Err(Error::Timeout("TYPE I".into())),
    }
    Ok((s, welcome))
}

/// Opens a second session to the same server. Bulk jobs use their own session so browsing
/// stays responsive.
pub async fn connect_like(ftp: &XboxFtp) -> Result<XboxFtp> {
    XboxFtp::connect_with_timeout(ftp.info(), ftp.timeout()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_name_from_greeting() {
        let unleashx = "220-Client IP: 192.168.1.56\n220-Drive C\t Used: 174.03MB\n\n220 UnleashX FTP Server ready.";
        assert_eq!(server_name(unleashx), "UnleashX FTP Server ready.");
        assert_eq!(server_name("220 Welcome to XBMC"), "Welcome to XBMC");
        assert_eq!(server_name(""), "");
    }

    #[test]
    fn join_and_split() {
        assert_eq!(join("/", "F"), "/F");
        assert_eq!(join("/F/", "Games"), "/F/Games");
        assert_eq!(join("/F/Games", ""), "/F/Games");
        assert_eq!(join("", ""), "/");
        assert_eq!(split("/F/Games/Halo/default.xbe"), ("/F/Games/Halo", "default.xbe"));
        assert_eq!(split("/default.xbe"), ("/", "default.xbe"));
    }
}
