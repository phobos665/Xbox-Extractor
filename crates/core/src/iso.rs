//! Building one XISO from a game folder on the Xbox.

use std::path::{Path, PathBuf};

use crate::ftp::XboxFtp;
use crate::ftpfs::{Cancel, FtpFilesystem, IsoProgress, ProgressFn};
use crate::writer::IsoWriter;
use crate::{Error, Result};

/// Streams the folder at `remote_root` into an XISO at `out`.
///
/// The image is written to `<out>.part` and renamed when complete, so a cancelled or failed
/// job never leaves a file that looks finished. Returns the image size in bytes.
pub async fn create_iso(
    ftp: &mut XboxFtp,
    remote_root: &str,
    out: &Path,
    progress: ProgressFn,
    cancel: Cancel,
) -> Result<u64> {
    let part = part_path(out);
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }

    let result = build(ftp, remote_root, &part, progress.clone(), cancel).await;
    match result {
        Ok(bytes) => {
            std::fs::rename(&part, out)?;
            progress(IsoProgress::Done { bytes });
            Ok(bytes)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

async fn build(
    ftp: &mut XboxFtp,
    remote_root: &str,
    part: &Path,
    progress: ProgressFn,
    cancel: Cancel,
) -> Result<u64> {
    let mut image = IsoWriter::create(part)?;
    {
        let mut fs = FtpFilesystem::new(ftp, remote_root, progress.clone(), cancel.clone());
        xdvdfs::write::img::create_xdvdfs_image(&mut fs, &mut image, |_| {})
            .await
            .map_err(Error::from)?;
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    progress(IsoProgress::Finishing);
    Ok(image.finish()?)
}

pub fn part_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// A file name for a title's image: `Title (Region).iso`, with ` (Disc N)` for multi-disc
/// titles. Characters Windows or macOS reject in file names become spaces or dashes.
pub fn default_iso_name(title: &str, region: &str, disc_number: u32) -> String {
    let mut name = String::with_capacity(title.len() + 16);
    for c in title.chars() {
        match c {
            ':' => name.push_str(" -"),
            '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*' => name.push(' '),
            c if c.is_control() => {}
            c => name.push(c),
        }
    }
    let mut name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    while name.ends_with('.') {
        name.pop();
    }
    if name.is_empty() {
        name = "Untitled".into();
    }
    if !region.is_empty() {
        name.push_str(&format!(" ({region})"));
    }
    if disc_number > 1 {
        name.push_str(&format!(" (Disc {disc_number})"));
    }
    name.push_str(".iso");
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_names() {
        assert_eq!(default_iso_name("Halo: Combat Evolved", "USA", 0), "Halo - Combat Evolved (USA).iso");
        assert_eq!(default_iso_name("What?  Now...", "Europe", 2), "What Now (Europe) (Disc 2).iso");
        assert_eq!(default_iso_name("", "", 0), "Untitled.iso");
    }

    #[test]
    fn part_suffix() {
        assert_eq!(part_path(Path::new("/x/Halo.iso")), PathBuf::from("/x/Halo.iso.part"));
    }
}
