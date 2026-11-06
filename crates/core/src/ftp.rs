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

use crate::error::reply_is_no_space;
use crate::listparse;
use crate::{Error, Result};

/// Runs `op` (an expression giving a future, with `s` bound to the [`XboxFtp`]) and, while it
/// fails for a passing reason ([`Error::is_transient`]: the server dropped an idle session,
/// answered `502` or `4xx`, or a reply came out of step), logs in again and runs it again, up
/// to [`XboxFtp::RETRIES`] times with a growing pause. `what` names the operation in the log.
///
/// `op` must be safe to repeat: it may have partly happened before it failed.
///
/// A macro rather than a method taking an async closure, because the futures of an async
/// closure called through a generic bound cannot yet be shown to be `Send`, which the GUI's
/// task runtime requires.
macro_rules! retrying {
    ($ftp:expr, $what:expr, |$s:ident| $op:expr) => {{
        let mut attempt = 0u32;
        loop {
            let err = {
                let $s: &mut $crate::ftp::XboxFtp = &mut *$ftp;
                match $op.await {
                    Err(e) if e.is_transient() => e,
                    r => break r,
                }
            };
            if let Err(e) = $ftp.recover(&mut attempt, err, &$what).await {
                break Err(e);
            }
        }
    }};
}
pub(crate) use retrying;

pub type Session =suppaftp::tokio::ImplAsyncFtpStream<suppaftp::tokio::AsyncNoTlsStream>;
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
#[serde(rename_all = "camelCase")]
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

    /// How many times a command that failed for a passing reason (see
    /// [`Error::is_transient`]) is tried again, each time on a fresh session.
    pub const RETRIES: u32 = 3;

    pub async fn connect(info: &ConnectionInfo) -> Result<Self> {
        Self::connect_with_timeout(info, Self::DEFAULT_TIMEOUT).await
    }

    pub async fn connect_with_timeout(info: &ConnectionInfo, timeout: Duration) -> Result<Self> {
        let (stream, welcome) = open(info, timeout).await?;
        Ok(Self { info: info.clone(), stream, welcome, timeout, rest_ignored: false })
    }

    /// Like [`XboxFtp::connect_with_timeout`], but tries again when the server is busy, as it
    /// is for a few seconds after a transfer was cut off, or when it already has as many
    /// sessions as it allows. For background jobs; an interactive connect should fail fast.
    pub async fn connect_patiently(info: &ConnectionInfo, timeout: Duration) -> Result<Self> {
        let mut attempt = 0;
        loop {
            match Self::connect_with_timeout(info, timeout).await {
                Err(e) if e.is_transient() && attempt < Self::RETRIES => {
                    attempt += 1;
                    log::warn!("connecting to {}: {e}; trying again ({attempt} of {})", info.host, Self::RETRIES);
                    tokio::time::sleep(backoff(attempt)).await;
                }
                r => return r,
            }
        }
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

    /// The free space on `drive` (`E`, `F:`), as the server's greeting reports it. Logs in
    /// again first so the figure is current. `None` when the server does not say (only some
    /// dashboards, UnleashX among them, list their drives in the greeting).
    pub async fn free_space(&mut self, drive: &str) -> Option<u64> {
        // A server that does not report space in its greeting will not start to on a new login.
        drive_free_space(self.welcome.as_deref()?, drive)?;
        if let Err(e) = self.reconnect().await {
            log::warn!("refreshing drive space: {e}");
        }
        drive_free_space(self.welcome.as_deref()?, drive)
    }

    /// Called by [`retrying!`] when an operation failed with `err`: gives up with `err` once
    /// [`XboxFtp::RETRIES`] tries are spent or the failure is not transient, else pauses and logs
    /// in again (as many times as the retries allow) and returns `Ok` to say try once more.
    pub(crate) async fn recover(&mut self, attempt: &mut u32, mut err: Error, what: &str) -> Result<()> {
        loop {
            if !err.is_transient() || *attempt >= Self::RETRIES {
                return Err(err);
            }
            *attempt += 1;
            log::warn!("{what}: {err}; trying again on a fresh session ({attempt} of {})", Self::RETRIES);
            tokio::time::sleep(backoff(*attempt)).await;
            match self.reconnect().await {
                Ok(()) => return Ok(()),
                Err(e) => err = e,
            }
        }
    }

    pub async fn cwd(&mut self, path: &str) -> Result<()> {
        retrying!(self, format!("CWD {path}"), |s| s.cwd_once(path))
    }

    async fn cwd_once(&mut self, path: &str) -> Result<()> {
        let p = if path.is_empty() { "/" } else { path };
        let t = self.timeout;
        match tokio::time::timeout(t, self.stream.cwd(p)).await {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot open {p}"), e)),
            Err(_) => Err(Error::Timeout(format!("CWD {p}"))),
        }
    }

    /// Lists a directory. Lines the parser cannot read are logged and skipped; a listing of
    /// which no line could be read is an error, so an unfamiliar server format is not shown as
    /// an empty folder.
    pub async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        retrying!(self, format!("LIST {path}"), |s| s.list_once(path))
    }

    async fn list_once(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        self.cwd_once(path).await?;
        let t = self.timeout;
        let lines = match tokio::time::timeout(t, self.stream.list(None)).await {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot list {path}"), e))?,
            Err(_) => return Err(Error::Timeout(format!("LIST {path}"))),
        };
        let (entries, unparsed) = listparse::parse_listing(&lines);
        for l in &unparsed {
            log::warn!("LIST {path}: could not read line {l:?}");
        }
        if entries.is_empty() {
            if let Some(first) = unparsed.first() {
                return Err(Error::Ftp(format!(
                    "could not read the server's listing of {path}. It sent {} line(s) in a format \
                     this app does not know, starting with: {first:?}",
                    unparsed.len()
                )));
            }
        }
        Ok(entries
            .into_iter()
            .map(|e| RemoteEntry { name: e.name, is_dir: e.is_dir, size: e.size })
            .collect())
    }

    /// Opens `path` for reading, optionally from `offset` (`REST`). The caller must end the
    /// transfer with [`XboxFtp::end_transfer`].
    pub async fn open_read(&mut self, path: &str, offset: u64) -> Result<TransferStream> {
        retrying!(self, format!("RETR {path}"), |s| s.open_read_once(path, offset))
    }

    async fn open_read_once(&mut self, path: &str, offset: u64) -> Result<TransferStream> {
        let (dir, name) = split(path);
        self.cwd_once(dir).await?;
        if offset > 0 {
            let off = usize::try_from(offset).map_err(|_| Error::Ftp("offset too large".into()))?;
            timed(self.timeout, "REST", self.stream.resume_transfer(off)).await?;
        }
        match tokio::time::timeout(self.timeout, self.stream.retr_as_stream(name)).await {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot read {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("RETR {path}"))),
        }
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

    /// Opens `path` for writing (`STOR`), replacing any file already there. The caller must
    /// end the upload with [`XboxFtp::finish_write`], or drop the stream and reconnect.
    pub async fn open_write(&mut self, path: &str) -> Result<TransferStream> {
        retrying!(self, format!("STOR {path}"), |s| s.open_write_once(path))
    }

    async fn open_write_once(&mut self, path: &str) -> Result<TransferStream> {
        let (dir, name) = split(path);
        self.cwd_once(dir).await?;
        let t = self.timeout;
        match tokio::time::timeout(t, self.stream.put_with_stream(name)).await {
            Ok(Err(e)) if reply_is_no_space(&e) => Err(Error::Ftp(format!("the Xbox's drive is full; cannot write {path}"))),
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot write {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("STOR {path}"))),
        }
    }

    /// Closes an upload and waits for the server to confirm it stored the file. The wait is
    /// longer than for other replies: a server may write the end of a large file out before
    /// it answers.
    pub async fn finish_write(&mut self, stream: TransferStream, path: &str) -> Result<()> {
        let t = self.timeout.max(FINISH_TIMEOUT);
        match tokio::time::timeout(t, stream.finish()).await {
            Ok(Err(e)) if reply_is_no_space(&e) => Err(Error::Ftp(format!("the Xbox's drive filled up while storing {path}"))),
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("the Xbox did not store {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("finishing {path}"))),
        }
    }

    /// Whether `path` is listed in its folder. For checking whether a command whose reply was
    /// lost took effect.
    async fn exists_once(&mut self, path: &str) -> Result<bool> {
        let (dir, name) = split(path);
        Ok(self.list_once(dir).await?.iter().any(|e| e.name.eq_ignore_ascii_case(name)))
    }

    /// Creates the folder `path`. Its parent must exist.
    pub async fn mkdir(&mut self, path: &str) -> Result<()> {
        let mut again = false;
        retrying!(self, format!("MKD {path}"), |s| async {
            let r = s.mkdir_once(path).await;
            // A retried MKD whose first try worked is refused because the folder now exists.
            if again && r.as_ref().is_err_and(|e| !e.is_transient()) && s.cwd_once(path).await.is_ok() {
                return Ok(());
            }
            again = true;
            r
        })
    }

    async fn mkdir_once(&mut self, path: &str) -> Result<()> {
        let (dir, name) = split(path);
        self.cwd_once(dir).await?;
        let t = self.timeout;
        match tokio::time::timeout(t, self.stream.mkdir(name)).await {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot create {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("MKD {path}"))),
        }
    }

    /// Deletes the file at `path` (`DELE`).
    pub async fn remove_file(&mut self, path: &str) -> Result<()> {
        self.remove(path, false).await
    }

    /// Deletes the empty folder at `path` (`RMD`).
    pub async fn remove_dir(&mut self, path: &str) -> Result<()> {
        self.remove(path, true).await
    }

    async fn remove(&mut self, path: &str, is_dir: bool) -> Result<()> {
        let (cmd, mut again) = (if is_dir { "RMD" } else { "DELE" }, false);
        retrying!(self, format!("{cmd} {path}"), |s| async {
            let r = s.remove_once(path, is_dir).await;
            // A retried delete whose first try worked is refused because nothing is there.
            if again && r.as_ref().is_err_and(|e| !e.is_transient()) && s.exists_once(path).await.is_ok_and(|x| !x) {
                return Ok(());
            }
            again = true;
            r
        })
    }

    async fn remove_once(&mut self, path: &str, is_dir: bool) -> Result<()> {
        let (dir, name) = split(path);
        self.cwd_once(dir).await?;
        let t = self.timeout;
        let r = if is_dir {
            tokio::time::timeout(t, self.stream.rmdir(name)).await
        } else {
            tokio::time::timeout(t, self.stream.rm(name)).await
        };
        match r {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot delete {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("{} {path}", if is_dir { "RMD" } else { "DELE" }))),
        }
    }

    /// Renames the file or folder at `path` to `new_name`, in the same folder.
    pub async fn rename(&mut self, path: &str, new_name: &str) -> Result<()> {
        let mut again = false;
        let new_path = join(split(path).0, new_name);
        retrying!(self, format!("rename {path}"), |s| async {
            let r = s.rename_once(path, new_name).await;
            // A retried rename whose first try worked is refused because the old name is gone.
            if again
                && r.as_ref().is_err_and(|e| !e.is_transient())
                && s.exists_once(&new_path).await.is_ok_and(|x| x)
                && s.exists_once(path).await.is_ok_and(|x| !x)
            {
                return Ok(());
            }
            again = true;
            r
        })
    }

    async fn rename_once(&mut self, path: &str, new_name: &str) -> Result<()> {
        let (dir, name) = split(path);
        self.cwd_once(dir).await?;
        let t = self.timeout;
        match tokio::time::timeout(t, self.stream.rename(name, new_name)).await {
            Ok(r) => r.map_err(|e| Error::ftp(format_args!("cannot rename {path}"), e)),
            Err(_) => Err(Error::Timeout(format!("rename {path}"))),
        }
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
        retrying!(self, format!("reading {path}"), |s| async {
            s.read_prefix_once(path, offset, cap, file_size, &mut target).await
        })
    }

    async fn read_prefix_once<F>(
        &mut self,
        path: &str,
        offset: u64,
        cap: usize,
        file_size: Option<u64>,
        target: &mut F,
    ) -> Result<Vec<u8>>
    where
        F: FnMut(&[u8]) -> Option<usize>,
    {
        let mut stream = self.open_read_once(path, offset).await?;
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
        retrying!(self, format!("reading {path}"), |s| s.read_skipping(path, offset, len, file_size))
    }

    async fn read_skipping(&mut self, path: &str, offset: u64, len: usize, file_size: Option<u64>) -> Result<Vec<u8>> {
        let mut stream = self.open_read_once(path, 0).await?;
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

/// How long to wait for a server to confirm it stored an upload.
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// The pause before retry `attempt` (from 1): half a second, then one, then two.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 << attempt.saturating_sub(1).min(3))
}

/// Reads the free space on `drive` from a server greeting that lists its drives, as UnleashX's
/// does: `220-Drive F:  Used: 1.20GB  Free: 3.50GB`. Sizes are read in powers of 1024.
pub fn drive_free_space(welcome: &str, drive: &str) -> Option<u64> {
    let want = drive.trim_matches(['/', ':']).to_ascii_uppercase();
    for line in welcome.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(at) = lower.find("drive") else { continue };
        let Some(letter) = line[at + 5..].trim_start().chars().next() else { continue };
        if !letter.to_string().eq_ignore_ascii_case(&want) {
            continue;
        }
        let Some(f) = lower.find("free") else { continue };
        let rest = line[f + 4..].trim_start_matches([':', '=', ' ', '\t']);
        if let Some(n) = parse_size(rest) {
            return Some(n);
        }
    }
    None
}

/// Parses `3.50GB`, `3.50 GB`, `512,000 KB` or `1048576 bytes` at the start of `s`.
fn parse_size(s: &str) -> Option<u64> {
    let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).unwrap_or(s.len());
    let num: f64 = s[..num_end].replace(',', "").parse().ok()?;
    let unit: String = s[num_end..].trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let mult: u64 = match unit.to_ascii_lowercase().as_str() {
        "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return None,
    };
    Some((num * mult as f64) as u64)
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

/// Says what a failed connection most likely means, in place of the bare socket error.
fn explain_connect_error(e: &suppaftp::FtpError) -> String {
    let io = match e {
        suppaftp::FtpError::ConnectionError(io) => io,
        other => return other.to_string(),
    };
    // EHOSTUNREACH. On macOS 15 and later this is what an app gets for any address on the
    // local network until the user allows it Local Network access; the same code from a
    // terminal succeeds, because the terminal's permission is inherited.
    if cfg!(target_os = "macos") && io.raw_os_error() == Some(65) {
        return "macOS blocked the connection. Allow this app under System Settings > Privacy & \
                Security > Local Network, then try again"
            .into();
    }
    match io.kind() {
        std::io::ErrorKind::TimedOut => {
            "no answer. Check the Xbox is on, on this network, and its dashboard (UnleashX, \
             EvolutionX, XBMC...) is running its FTP server"
                .into()
        }
        std::io::ErrorKind::ConnectionRefused => {
            "the Xbox refused the connection. Its dashboard's FTP server may be switched off, or \
             on another port"
                .into()
        }
        _ => io.to_string(),
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
        .map_err(|e| cerr(explain_connect_error(&e)))?;
    let welcome = s.get_welcome_msg().map(|w| w.trim().to_string());
    match tokio::time::timeout(timeout, s.login(info.user.as_str(), info.password.as_str())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            // 530 is a wrong user or password. A 421 or a dropped connection is a server with
            // all its sessions in use, or still busy with one that was cut off.
            let e = Error::ftp("logging in", e);
            return Err(if e.is_transient() { e } else { Error::Login(info.user.clone()) });
        }
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
    XboxFtp::connect_patiently(ftp.info(), ftp.timeout()).await
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
    fn free_space_from_greeting() {
        let g = "220-Client IP: 192.168.1.56\n220-Drive C:\t Used: 174.03MB\t Free: 325.97MB\n\
                 220-Drive F:  Used: 100.00GB  Free: 1.5 GB\n220-Drive E: Free = 2,048 KB\n220 UnleashX FTP Server ready.";
        assert_eq!(drive_free_space(g, "C"), Some((325.97 * 1048576.0) as u64));
        assert_eq!(drive_free_space(g, "f:"), Some(3 << 29));
        assert_eq!(drive_free_space(g, "E"), Some(2 << 20));
        assert_eq!(drive_free_space(g, "G"), None);
        // UnleashX's greeting as some versions send it: used space only.
        assert_eq!(drive_free_space("220-Drive C\t Used: 174.03MB\n220 ready", "C"), None);
        assert_eq!(drive_free_space("220 Welcome to XBMC", "C"), None);
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
