//! Finding titles on the Xbox's drives.
//!
//! A title is a folder holding `default.xbe`. Dashboards keep them in a few conventional
//! places (`F:\Games`, `E:\Games`, straight under `F:\` or `G:\`), so the scan starts from a
//! set of roots and descends a couple of levels, stopping at the first folder that has an XBE.
//! For each title it reads only the XBE's header, a few kilobytes, for the name and ID.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ftp::{join, RemoteEntry, XboxFtp};
use crate::iso::default_iso_name;
use crate::{Error, Result};

const XBE_NAME: &str = "default.xbe";
/// More than any real XBE header; a bound on what a scan reads per title.
const HEADER_CAP: usize = 1 << 20;
const IMAGE_CAP: usize = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanRoot {
    /// Drive letter, e.g. `F`.
    pub drive: String,
    /// Folder under the drive, `/`-separated, empty for the drive itself.
    pub sub: String,
}

impl ScanRoot {
    /// Parses `F`, `F:`, `F:/Games`, `F:\Games` or `/F/Games`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().replace('\\', "/");
        let s = s.trim_start_matches('/');
        let mut parts = s.splitn(2, '/');
        let drive = parts.next()?.trim_end_matches(':').to_string();
        if drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()) {
            return None;
        }
        let sub = parts.next().unwrap_or("").trim_matches('/').to_string();
        Some(Self { drive: drive.to_ascii_uppercase(), sub })
    }

    pub fn display(&self) -> String {
        if self.sub.is_empty() { format!("{}:", self.drive) } else { format!("{}:/{}", self.drive, self.sub) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanOptions {
    pub roots: Vec<ScanRoot>,
    /// How many folder levels below each root to look for titles.
    pub depth: u32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            roots: ["F", "E:/Games", "G"].iter().filter_map(|s| ScanRoot::parse(s)).collect(),
            depth: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Title {
    /// Remote folder path, e.g. `/F/Games/Halo`.
    pub path: String,
    pub folder: String,
    /// The XBE's file name as the server lists it (case varies).
    pub xbe_file: String,
    pub xbe_size: u64,
    pub title_name: String,
    pub title_id: String,
    pub title_code: Option<String>,
    pub region: String,
    pub disc_number: u32,
    pub version: u32,
    /// Raw offset and size of the `$$XTIMAGE` section, when the XBE has one.
    pub image_section: Option<(u32, u32)>,
    /// Suggested output file name.
    pub iso_name: String,
    /// Why the XBE could not be read, when it could not. The title is still listed.
    pub error: Option<String>,
    /// Total bytes and files in the folder, once measured ([`folder_size`]).
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub file_count: Option<u64>,
    /// Taken from a previous scan because its XBE had not changed size.
    #[serde(default)]
    pub cached: bool,
}

impl Title {
    pub fn xbe_path(&self) -> String {
        join(&self.path, &self.xbe_file)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScanEvent {
    Visiting { path: String },
    Found { title: Box<Title> },
    /// A configured root whose drive the server does not list.
    RootMissing { root: String },
    /// A folder the server would not open.
    Skipped { path: String, reason: String },
}

/// The drive folders at the server's root (`C`, `E`, `F`, ...). Some servers name them `F:`.
pub async fn drives(ftp: &mut XboxFtp) -> Result<Vec<RemoteEntry>> {
    Ok(ftp.list_dir("/").await?.into_iter().filter(|e| e.is_dir).collect())
}

fn resolve_root(drives: &[RemoteEntry], root: &ScanRoot) -> Option<String> {
    let d = drives
        .iter()
        .find(|e| e.name.trim_end_matches(':').eq_ignore_ascii_case(&root.drive))?;
    let base = join("/", &d.name);
    Some(if root.sub.is_empty() { base } else { join(&base, &root.sub) })
}

/// Scans the roots in `opts`, reporting each folder visited and each title found as it goes.
pub async fn scan<F>(ftp: &mut XboxFtp, opts: &ScanOptions, on_event: F) -> Result<Vec<Title>>
where
    F: FnMut(ScanEvent),
{
    scan_with_cache(ftp, opts, &HashMap::new(), on_event).await
}

/// Keys titles from an earlier scan for [`scan_with_cache`].
pub fn cache_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// Like [`scan`], but a folder whose XBE has the same name and size as in `known` (keyed by
/// [`cache_key`]) reuses that title instead of reading the XBE again. Each folder is still
/// listed, so added and removed titles are found; what is skipped is the header read, which
/// over FTP costs a transfer and a fresh login per title.
pub async fn scan_with_cache<F>(
    ftp: &mut XboxFtp,
    opts: &ScanOptions,
    known: &HashMap<String, Title>,
    mut on_event: F,
) -> Result<Vec<Title>>
where
    F: FnMut(ScanEvent),
{
    let drives = drives(ftp).await?;
    let mut titles = Vec::new();
    let mut seen = HashSet::new();

    for root in &opts.roots {
        let Some(start) = resolve_root(&drives, root) else {
            on_event(ScanEvent::RootMissing { root: root.display() });
            continue;
        };
        let mut stack = vec![(start, opts.depth)];
        while let Some((dir, depth)) = stack.pop() {
            if !seen.insert(dir.to_ascii_lowercase()) {
                continue;
            }
            on_event(ScanEvent::Visiting { path: dir.clone() });
            let listing = match ftp.list_dir(&dir).await {
                Ok(l) => l,
                Err(Error::Ftp(e)) => {
                    // A folder that does not exist (no Games folder on E:) is normal.
                    log::debug!("skipping {dir}: {e}");
                    on_event(ScanEvent::Skipped { path: dir.clone(), reason: e });
                    continue;
                }
                Err(Error::Timeout(what)) => {
                    // One slow folder should not end the scan. The session may be mid-reply, so
                    // start a fresh one and move on.
                    log::warn!("{what} timed out; skipping {dir}");
                    ftp.reconnect().await?;
                    on_event(ScanEvent::Skipped { path: dir.clone(), reason: format!("timed out: {what}") });
                    continue;
                }
                Err(e) => return Err(e),
            };

            if let Some(xbe) = listing
                .iter()
                .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(XBE_NAME))
            {
                let reuse = known
                    .get(&cache_key(&dir))
                    .filter(|t| t.xbe_size == xbe.size && t.xbe_file == xbe.name && t.error.is_none());
                let title = match reuse {
                    Some(t) => Title { path: dir.clone(), cached: true, ..t.clone() },
                    None => read_title(ftp, &dir, xbe).await,
                };
                on_event(ScanEvent::Found { title: Box::new(title.clone()) });
                titles.push(title);
                continue;
            }

            if depth == 0 {
                continue;
            }
            let mut subdirs: Vec<_> = listing
                .iter()
                .filter(|e| e.is_dir && !e.name.starts_with('.') && !e.name.starts_with('$'))
                .map(|e| join(&dir, &e.name))
                .collect();
            // Reversed so the stack pops them in listing order.
            subdirs.reverse();
            stack.extend(subdirs.into_iter().map(|p| (p, depth - 1)));
        }
    }
    Ok(titles)
}

fn folder_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Reads one title's XBE header. Never fails: an unreadable XBE gives a title named after its
/// folder, with the reason in `error`.
pub async fn read_title(ftp: &mut XboxFtp, dir: &str, xbe: &RemoteEntry) -> Title {
    let folder = folder_name(dir);
    let mut t = Title {
        path: dir.to_string(),
        folder: folder.clone(),
        xbe_file: xbe.name.clone(),
        xbe_size: xbe.size,
        title_name: folder.clone(),
        title_id: String::new(),
        title_code: None,
        region: String::new(),
        disc_number: 0,
        version: 0,
        image_section: None,
        iso_name: default_iso_name(&folder, "", 0),
        error: None,
        size_bytes: None,
        file_count: None,
        cached: false,
    };

    let path = join(dir, &xbe.name);
    let data = ftp
        .read_prefix(&path, 0, HEADER_CAP, Some(xbe.size), |b| {
            (b.len() >= xbe::SIZE_FIELD_END).then(|| xbe::headers_size(b).unwrap_or(b.len()))
        })
        .await;
    let info = match data {
        Ok(d) => xbe::parse(&d).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match info {
        Ok(x) => {
            if !x.title_name.is_empty() {
                t.title_name = x.title_name.clone();
            }
            t.title_id = x.title_id_hex();
            t.title_code = x.title_id_code();
            t.region = x.region_label();
            t.disc_number = x.disc_number;
            t.version = x.version;
            t.image_section = x
                .section(xbe::TITLE_IMAGE_SECTION)
                .filter(|s| s.raw_size > 0)
                .map(|s| (s.raw_addr, s.raw_size));
            t.iso_name = default_iso_name(&t.title_name, &t.region, t.disc_number);
        }
        Err(e) => t.error = Some(e),
    }
    t
}

fn xpr_to_png(data: &[u8]) -> Result<Vec<u8>> {
    let img = xbe::image::decode_xpr(data).map_err(|e| Error::Image(e.to_string()))?;
    xbe::image::to_png(&img).map_err(|e| Error::Image(e.to_string()))
}

/// The title image as PNG, or `None` when there is none.
///
/// Tries the dashboard's copy first, `E:\UDATA\<title id>\TitleImage.xbx`: about 10 KB, kept
/// for any title that has saved. Otherwise reads the XBE's `$$XTIMAGE` section, which sits after
/// the code, often tens of megabytes in; on a server that ignores `REST` that means reading the
/// XBE up to it.
pub async fn title_image(ftp: &mut XboxFtp, title: &Title) -> Result<Option<Vec<u8>>> {
    if !title.title_id.is_empty() {
        let udata = format!("/E/UDATA/{}/TitleImage.xbx", title.title_id.to_ascii_lowercase());
        if let Ok(data) = ftp.read_prefix(&udata, 0, IMAGE_CAP, None, |_| None).await {
            match xpr_to_png(&data) {
                Ok(png) => return Ok(Some(png)),
                Err(e) => log::debug!("{udata}: {e}"),
            }
        }
    }
    let Some((addr, size)) = title.image_section else { return Ok(None) };
    let size = (size as usize).min(IMAGE_CAP);
    let data = ftp
        .read_at(&title.xbe_path(), addr as u64, size, Some(title.xbe_size), |d| !d.starts_with(b"XBEH"))
        .await?;
    xpr_to_png(&data).map(Some)
}

/// Total bytes and file count under a folder.
pub async fn folder_size(ftp: &mut XboxFtp, path: &str) -> Result<(u64, u64)> {
    let mut stack = vec![path.to_string()];
    let (mut bytes, mut files) = (0u64, 0u64);
    while let Some(dir) = stack.pop() {
        for e in ftp.list_dir(&dir).await? {
            if e.is_dir {
                stack.push(join(&dir, &e.name));
            } else {
                bytes += e.size;
                files += 1;
            }
        }
    }
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_parsing() {
        assert_eq!(ScanRoot::parse("F").unwrap(), ScanRoot { drive: "F".into(), sub: "".into() });
        assert_eq!(ScanRoot::parse("e:\\Games\\").unwrap(), ScanRoot { drive: "E".into(), sub: "Games".into() });
        assert_eq!(ScanRoot::parse("/G/Games/Racing").unwrap().sub, "Games/Racing");
        assert!(ScanRoot::parse("FF:/x").is_none());
        assert!(ScanRoot::parse("").is_none());
    }

    #[test]
    fn roots_resolve_against_server_drive_names() {
        let drives = vec![
            RemoteEntry { name: "C".into(), is_dir: true, size: 0 },
            RemoteEntry { name: "f:".into(), is_dir: true, size: 0 },
        ];
        let r = ScanRoot::parse("F:/Games").unwrap();
        assert_eq!(resolve_root(&drives, &r).as_deref(), Some("/f:/Games"));
        assert_eq!(resolve_root(&drives, &ScanRoot::parse("G").unwrap()), None);
    }
}
