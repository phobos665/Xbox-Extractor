//! Moving files and folders between this computer and the Xbox, for the file browser.
//!
//! The Xbox's drives are FATX, which is stricter than the FTP server lets on: a name longer
//! than 42 characters or a file of 4 GiB or more is refused partway through a transfer, often
//! with a bare `550`. Uploads check for both before sending anything.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;

use crate::copy;
use crate::ftp::{join, split, XboxFtp};
use crate::ftpfs::{Cancel, IsoProgress, ProgressFn};
use crate::{Error, Result};

const CHUNK: usize = 1 << 20;

/// The longest name FATX stores.
pub const FATX_MAX_NAME: usize = 42;

/// FATX stores files up to 4 GiB less one byte.
pub const FATX_MAX_FILE: u64 = u32::MAX as u64;

/// Checks that `name` can be stored on a FATX drive.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(Error::Ftp(format!("\"{name}\" is not a usable name")));
    }
    if name.chars().count() > FATX_MAX_NAME {
        return Err(Error::Ftp(format!(
            "\"{name}\" is longer than the {FATX_MAX_NAME} characters an Xbox drive allows"
        )));
    }
    if let Some(c) = name.chars().find(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()) {
        return Err(Error::Ftp(format!("\"{name}\" contains {c:?}, which an Xbox drive does not allow")));
    }
    Ok(())
}

/// Files a Mac or Windows leaves in folders that have no business on the Xbox.
fn is_junk(name: &str) -> bool {
    name == ".DS_Store" || name.starts_with("._") || name.eq_ignore_ascii_case("Thumbs.db") || name.eq_ignore_ascii_case("desktop.ini")
}

struct LocalFile {
    local: PathBuf,
    remote: String,
    size: u64,
}

/// Uploads the local file or folder `local` into the remote folder `remote_dir`, keeping its
/// name. Files already on the Xbox under the same names are replaced; a folder is merged into
/// one of the same name. Returns the bytes sent.
///
/// A file stopped partway is deleted from the Xbox, so a cancelled upload does not leave a
/// truncated file that looks complete. Files sent before it stay.
pub async fn upload(
    ftp: &mut XboxFtp,
    local: &Path,
    remote_dir: &str,
    progress: ProgressFn,
    cancel: Cancel,
) -> Result<u64> {
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Ftp(format!("{} has no usable name", local.display())))?;
    check_name(name)?;
    let top = join(remote_dir, name);

    // Walk the local tree first: it gives progress a total, and finds names the Xbox would
    // refuse before anything is sent.
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut total = 0u64;
    let meta = std::fs::metadata(local)?;
    if meta.is_dir() {
        let mut stack = vec![(local.to_path_buf(), top.clone())];
        while let Some((dir, remote)) = stack.pop() {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            dirs.push(remote.clone());
            let mut children: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
            children.sort_by_key(|e| e.file_name());
            for e in children {
                let Some(child) = e.file_name().to_str().map(str::to_string) else {
                    return Err(Error::Ftp(format!("{} has a name that is not valid Unicode", e.path().display())));
                };
                if is_junk(&child) {
                    continue;
                }
                check_name(&child).map_err(|err| Error::Ftp(format!("{}: {err}", e.path().display())))?;
                let m = e.metadata()?;
                let remote_child = join(&remote, &child);
                if m.is_dir() {
                    stack.push((e.path(), remote_child));
                } else {
                    total += m.len();
                    files.push(LocalFile { local: e.path(), remote: remote_child, size: m.len() });
                }
            }
            progress(IsoProgress::Listing { dirs: dirs.len() as u64, files: files.len() as u64, bytes: total });
        }
    } else {
        total = meta.len();
        files.push(LocalFile { local: local.to_path_buf(), remote: top.clone(), size: meta.len() });
    }
    if let Some(f) = files.iter().find(|f| f.size > FATX_MAX_FILE) {
        return Err(Error::Ftp(format!(
            "{} is 4 GiB or larger, which an Xbox drive cannot store",
            f.local.display()
        )));
    }

    for d in &dirs {
        ensure_dir(ftp, d).await?;
    }

    let mut done = 0u64;
    let mut buf = vec![0u8; CHUNK];
    for f in &files {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        progress(IsoProgress::Copying { file: f.remote.clone(), done, total });
        let base = done;
        send_file(ftp, f, &mut buf, &cancel, |n| {
            progress(IsoProgress::Copying { file: f.remote.clone(), done: base + n, total })
        })
        .await?;
        done += f.size;
    }
    progress(IsoProgress::Done { bytes: done });
    Ok(done)
}

/// Creates `path` unless it is already there.
async fn ensure_dir(ftp: &mut XboxFtp, path: &str) -> Result<()> {
    match ftp.mkdir(path).await {
        Ok(()) => Ok(()),
        // Servers word "already exists" differently, if at all; finding the folder settles it.
        Err(e) => match ftp.cwd(path).await {
            Ok(()) => Ok(()),
            Err(_) => Err(e),
        },
    }
}

async fn send_file(
    ftp: &mut XboxFtp,
    f: &LocalFile,
    buf: &mut [u8],
    cancel: &Cancel,
    mut on_bytes: impl FnMut(u64),
) -> Result<()> {
    let mut src = File::open(&f.local)?;
    let mut stream = ftp.open_write(&f.remote).await?;
    let timeout = ftp.timeout();
    let mut sent = 0u64;
    let result: Result<()> = async {
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let n = src.read(buf)?;
            if n == 0 {
                return Ok(());
            }
            match tokio::time::timeout(timeout, stream.write_all(&buf[..n])).await {
                Ok(r) => r?,
                Err(_) => return Err(Error::Timeout(format!("sending {}", f.remote))),
            }
            sent += n as u64;
            on_bytes(sent);
        }
    }
    .await;
    match result {
        Ok(()) => ftp.finish_write(stream, &f.remote).await,
        Err(e) => {
            // As with downloads, a stopped upload leaves the control connection in doubt.
            drop(stream);
            if ftp.reconnect().await.is_ok() {
                let _ = ftp.remove_file(&f.remote).await;
            }
            Err(e)
        }
    }
}

/// Downloads the remote file or folder `remote` to the local path `out` (a file, or a folder
/// holding the folder's contents). `size` is the file's listed size and is ignored for folders.
/// An existing `out` is replaced. Returns the bytes copied.
pub async fn download(
    ftp: &mut XboxFtp,
    remote: &str,
    is_dir: bool,
    size: u64,
    out: &Path,
    progress: ProgressFn,
    cancel: Cancel,
) -> Result<u64> {
    if is_dir {
        copy::copy_folder(ftp, remote, out, progress, cancel).await
    } else {
        copy::copy_file(ftp, remote, size, out, progress, cancel).await
    }
}

/// Deletes the file or folder at `path`; a folder with everything in it.
///
/// Refuses a drive (`/C`, `/E`) or the root: emptying C: or E: leaves an Xbox that will not
/// boot to a dashboard.
pub async fn remove(ftp: &mut XboxFtp, path: &str, is_dir: bool) -> Result<()> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || split(trimmed).0 == "/" {
        return Err(Error::Ftp(format!("will not delete {path}: it is a whole drive")));
    }
    if !is_dir {
        return ftp.remove_file(trimmed).await;
    }
    // Depth first: list a folder, delete its files, then come back to it once its subfolders
    // are gone.
    let mut stack = vec![(trimmed.to_string(), false)];
    while let Some((dir, emptied)) = stack.pop() {
        if emptied {
            ftp.remove_dir(&dir).await?;
            continue;
        }
        stack.push((dir.clone(), true));
        for e in ftp.list_dir(&dir).await? {
            let child = join(&dir, &e.name);
            if e.is_dir {
                stack.push((child, false));
            } else {
                ftp.remove_file(&child).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatx_names() {
        assert!(check_name("default.xbe").is_ok());
        assert!(check_name(&"a".repeat(42)).is_ok());
        assert!(check_name(&"a".repeat(43)).is_err());
        assert!(check_name("what?").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
    }

    #[test]
    fn junk_files() {
        assert!(is_junk(".DS_Store"));
        assert!(is_junk("._default.xbe"));
        assert!(is_junk("thumbs.db"));
        assert!(!is_junk("default.xbe"));
    }
}
