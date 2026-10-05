//! Copying a game folder from the Xbox as plain files, for tools that want the game's data
//! rather than a disc image (xboxrecomp reads `default.xbe` and the files beside it).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;

use crate::ftp::{join, XboxFtp};
use crate::ftpfs::{Cancel, IsoProgress, ProgressFn};
use crate::iso::part_path;
use crate::{Error, Result};

const CHUNK: usize = 1 << 20;

struct RemoteFile {
    remote: String,
    rel: PathBuf,
    size: u64,
}

/// Copies the folder at `remote_root` to the local folder `out`, keeping its structure.
///
/// Files are written under `<out>.part` and the folder is renamed to `out` when every file has
/// arrived complete, so a cancelled or failed copy never leaves a folder that looks finished.
/// An existing `out` is replaced. Returns the bytes copied.
pub async fn copy_folder(
    ftp: &mut XboxFtp,
    remote_root: &str,
    out: &Path,
    progress: ProgressFn,
    cancel: Cancel,
) -> Result<u64> {
    let part = part_path(out);
    if part.exists() {
        std::fs::remove_dir_all(&part)?;
    }
    let result = copy_into(ftp, remote_root, &part, progress.clone(), &cancel).await;
    match result {
        Ok(bytes) => {
            if out.exists() {
                std::fs::remove_dir_all(out)?;
            }
            std::fs::rename(&part, out)?;
            progress(IsoProgress::Done { bytes });
            Ok(bytes)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&part);
            Err(e)
        }
    }
}

async fn copy_into(
    ftp: &mut XboxFtp,
    remote_root: &str,
    part: &Path,
    progress: ProgressFn,
    cancel: &Cancel,
) -> Result<u64> {
    // Walk the tree first, so progress has a total from the start.
    let mut files = Vec::new();
    let mut dirs = vec![(remote_root.trim_end_matches('/').to_string(), PathBuf::new())];
    let (mut ndirs, mut total) = (0u64, 0u64);
    std::fs::create_dir_all(part)?;
    while let Some((remote, rel)) = dirs.pop() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        for e in ftp.list_dir(&remote).await? {
            let child_rel = rel.join(&e.name);
            if e.is_dir {
                std::fs::create_dir_all(part.join(&child_rel))?;
                dirs.push((join(&remote, &e.name), child_rel));
            } else {
                total += e.size;
                files.push(RemoteFile { remote: join(&remote, &e.name), rel: child_rel, size: e.size });
            }
        }
        ndirs += 1;
        progress(IsoProgress::Listing { dirs: ndirs, files: files.len() as u64, bytes: total });
    }

    let mut done = 0u64;
    let mut buf = vec![0u8; CHUNK];
    for f in &files {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        progress(IsoProgress::Copying { file: f.remote.clone(), done, total });
        let mut out = BufWriter::with_capacity(CHUNK, File::create(part.join(&f.rel))?);
        if f.size == 0 {
            continue;
        }

        let mut stream = ftp.open_read(&f.remote, 0).await?;
        let timeout = ftp.timeout();
        let mut copied = 0u64;
        let result: Result<()> = async {
            loop {
                if cancel.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let n = match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
                    Ok(r) => r?,
                    Err(_) => return Err(Error::Timeout(format!("reading {}", f.remote))),
                };
                if n == 0 {
                    return Ok(());
                }
                out.write_all(&buf[..n])?;
                copied += n as u64;
                done += n as u64;
                progress(IsoProgress::Copying { file: f.remote.clone(), done, total });
            }
        }
        .await;
        let end = ftp.end_transfer(stream, result.is_ok()).await;
        result?;
        end?;
        out.flush()?;
        if copied != f.size {
            return Err(Error::ShortRead { path: f.remote.clone(), expected: f.size, got: copied });
        }
    }
    progress(IsoProgress::Finishing);
    Ok(done)
}

/// A folder name for a title copied as files: the ISO name without `.iso`.
pub fn default_folder_name(iso_name: &str) -> String {
    iso_name.strip_suffix(".iso").unwrap_or(iso_name).to_string()
}
