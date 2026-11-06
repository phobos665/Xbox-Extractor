//! A game folder on the Xbox, seen by xdvdfs as the source filesystem of an image.
//!
//! xdvdfs first walks the whole tree through `read_dir` to lay the image out, then calls
//! `copy_file_in` once per file with the file's final offset. So the folder is listed once, each
//! file is fetched exactly once, and its bytes go straight to their place in the image: nothing
//! is staged on local disk.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use xdvdfs::blockdev::BlockDeviceWrite;
use xdvdfs::write::fs::{FileEntry, FileType, Filesystem, PathVec};

use crate::ftp::{join, XboxFtp};
use crate::space;
use crate::{Error, Result};

const CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IsoProgress {
    /// Walking the folder. Counts so far.
    Listing { dirs: u64, files: u64, bytes: u64 },
    /// Copying. `done` and `total` are bytes across the whole image's files.
    Copying { file: String, done: u64, total: u64 },
    /// Writing the volume descriptor and padding.
    Finishing,
    Done { bytes: u64 },
}

pub type ProgressFn = Arc<dyn Fn(IsoProgress) + Send + Sync>;

/// A cancellation flag shared between a job and whoever may stop it.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct FtpFilesystem<'a> {
    ftp: &'a mut XboxFtp,
    root: String,
    progress: ProgressFn,
    cancel: Cancel,
    dirs: u64,
    files: u64,
    total: u64,
    done: u64,
    /// The local folder the image is written in, checked for room before the first file.
    space_dir: Option<PathBuf>,
}

impl<'a> FtpFilesystem<'a> {
    pub fn new(ftp: &'a mut XboxFtp, root: &str, progress: ProgressFn, cancel: Cancel) -> Self {
        Self {
            ftp,
            root: root.trim_end_matches('/').to_string(),
            progress,
            cancel,
            dirs: 0,
            files: 0,
            total: 0,
            done: 0,
            space_dir: None,
        }
    }

    /// Checks, once the tree is walked and before any file is fetched, that the local disk
    /// holding `dir` has room for the image.
    pub fn check_space_in(mut self, dir: PathBuf) -> Self {
        self.space_dir = Some(dir);
        self
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    fn remote(&self, path: &PathVec) -> String {
        path.iter().fold(self.root.clone(), |acc, c| join(&acc, c))
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancel.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// Fetches one file into the image at `offset`. On failure, takes back the progress it
    /// reported, so a retry counts from where the file started.
    async fn copy_once<W>(&mut self, remote: &str, dest: &mut W, offset: u64, size: u64) -> Result<u64>
    where
        W: BlockDeviceWrite<io::Error>,
    {
        let start = self.done;
        let mut stream = self.ftp.open_read(remote, 0).await?;
        let mut buf = vec![0u8; CHUNK];
        let mut copied = 0u64;
        let timeout = self.ftp.timeout();
        let result: Result<()> = async {
            loop {
                self.check_cancel()?;
                let n = match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
                    Ok(r) => r?,
                    Err(_) => return Err(Error::Timeout(format!("reading {remote}"))),
                };
                if n == 0 {
                    return Ok(());
                }
                let take = (n as u64).min(size.saturating_sub(copied)) as usize;
                if take > 0 {
                    dest.write(offset + copied, &buf[..take]).await?;
                }
                copied += n as u64;
                self.done += take as u64;
                (self.progress)(IsoProgress::Copying {
                    file: remote.to_string(),
                    done: self.done,
                    total: self.total,
                });
            }
        }
        .await;

        let complete = result.is_ok();
        let end = self.ftp.end_transfer(stream, complete).await;
        let checked = result.and(end).and_then(|()| {
            if copied != size {
                // Short reads are what a connection dropped mid-file looks like on some servers.
                return Err(Error::Transient(format!("{remote}: expected {size} bytes, the Xbox sent {copied}")));
            }
            Ok(copied)
        });
        if checked.is_err() {
            self.done = start;
        }
        checked
    }
}

#[async_trait::async_trait]
impl<W> Filesystem<W, Error, io::Error> for FtpFilesystem<'_>
where
    W: BlockDeviceWrite<io::Error>,
{
    async fn read_dir(&mut self, path: &PathVec) -> Result<Vec<FileEntry>> {
        self.check_cancel()?;
        let remote = self.remote(path);
        let listing = self.ftp.list_dir(&remote).await?;
        self.dirs += 1;
        let mut out = Vec::with_capacity(listing.len());
        for e in listing {
            if e.is_dir {
                out.push(FileEntry { name: e.name, file_type: FileType::Directory, len: 0 });
            } else {
                self.files += 1;
                self.total += e.size;
                out.push(FileEntry { name: e.name, file_type: FileType::File, len: e.size });
            }
        }
        (self.progress)(IsoProgress::Listing { dirs: self.dirs, files: self.files, bytes: self.total });
        Ok(out)
    }

    async fn copy_file_in(&mut self, src: &PathVec, dest: &mut W, offset: u64, size: u64) -> Result<u64> {
        self.check_cancel()?;
        if let Some(dir) = self.space_dir.take() {
            // Each file is padded to a 2 KiB sector, and the directory tables take a little more.
            space::check_local(&dir, self.total + self.files * 2048 + (1 << 20))?;
        }
        let remote = self.remote(src);
        (self.progress)(IsoProgress::Copying { file: remote.clone(), done: self.done, total: self.total });
        if size == 0 {
            return Ok(0);
        }
        // A file cut off by a dropped connection is fetched again from the start, into the
        // same place in the image.
        let mut attempt = 0;
        loop {
            match self.copy_once(&remote, dest, offset, size).await {
                Err(e) if e.is_transient() => {
                    self.ftp.recover(&mut attempt, e, &format!("reading {remote}")).await?;
                }
                r => return r,
            }
        }
    }

    async fn copy_file_buf(&mut self, src: &PathVec, buf: &mut [u8], offset: u64) -> Result<u64> {
        let remote = self.remote(src);
        let want = buf.len();
        let data = self.ftp.read_prefix(&remote, offset, want, None, |_| Some(want)).await?;
        buf[..data.len()].copy_from_slice(&data);
        buf[data.len()..].fill(0);
        Ok(buf.len() as u64)
    }

    fn path_to_string(&self, path: &PathVec) -> String {
        self.remote(path)
    }
}
