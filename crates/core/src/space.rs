//! Checking there is room for a transfer before it starts, so a long copy does not fail near
//! the end with a full disk.

use std::path::Path;

use crate::{Error, Result};

/// Room left over on top of what a transfer needs: filesystems need some for their own
/// records, and a disk filled to the last byte makes the rest of the computer misbehave.
pub const LOCAL_MARGIN: u64 = 64 << 20;

/// FATX allocates in 16 KiB clusters on the Xbox's data drives; a file takes whole clusters.
pub const FATX_CLUSTER: u64 = 16 << 10;

/// The bytes `size` takes on a FATX drive.
pub fn fatx_size(size: u64) -> u64 {
    size.div_ceil(FATX_CLUSTER).max(1) * FATX_CLUSTER
}

/// Checks that the local disk holding `dir` has `needed` bytes free, plus [`LOCAL_MARGIN`].
/// `dir` need not exist yet; its nearest existing parent is checked. A disk whose free space
/// cannot be read is let through.
pub fn check_local(dir: &Path, needed: u64) -> Result<()> {
    let Some(existing) = dir.ancestors().find(|p| !p.as_os_str().is_empty() && p.exists()) else {
        return Ok(());
    };
    let free = match fs4::available_space(existing) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("cannot read free space of {}: {e}", existing.display());
            return Ok(());
        }
    };
    if needed.saturating_add(LOCAL_MARGIN) > free {
        return Err(Error::NoSpace { place: existing.display().to_string(), needed, free });
    }
    Ok(())
}

/// The drive letter a remote path is on: `E` for `/E/Apps` or `/E:/Apps`.
pub fn remote_drive(path: &str) -> Option<String> {
    let first = path.trim_start_matches('/').split('/').next()?.trim_end_matches(':');
    (first.len() == 1 && first.chars().all(|c| c.is_ascii_alphabetic())).then(|| first.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatx_rounding() {
        assert_eq!(fatx_size(0), FATX_CLUSTER);
        assert_eq!(fatx_size(1), FATX_CLUSTER);
        assert_eq!(fatx_size(FATX_CLUSTER), FATX_CLUSTER);
        assert_eq!(fatx_size(FATX_CLUSTER + 1), 2 * FATX_CLUSTER);
    }

    #[test]
    fn drives_of_remote_paths() {
        assert_eq!(remote_drive("/E/Apps").as_deref(), Some("E"));
        assert_eq!(remote_drive("/f:/Games").as_deref(), Some("F"));
        assert_eq!(remote_drive("/").as_deref(), None);
        assert_eq!(remote_drive("/Games/x").as_deref(), None);
    }

    #[test]
    fn plenty_of_room_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_local(&dir.path().join("not/yet/made"), 0).is_ok());
        assert!(matches!(check_local(dir.path(), u64::MAX / 2), Err(Error::NoSpace { .. })));
    }
}
