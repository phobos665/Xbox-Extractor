//! End to end against a real FTP server on localhost: scan a fake Xbox drive layout, read a
//! title image, and stream a game folder into an XISO that must match what xdvdfs builds from
//! the same folder on local disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use xib_core::ftpfs::{Cancel, IsoProgress, ProgressFn};
use xib_core::scan::{self, ScanOptions, ScanRoot};
use xib_core::{iso, ConnectionInfo, XboxFtp};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An XPR0 bundle holding one 4x4 DXT1 texture, solid red.
fn red_xpr() -> Vec<u8> {
    let mut d = vec![0u8; 0x40];
    d[0..4].copy_from_slice(b"XPR0");
    d[4..8].copy_from_slice(&(0x48u32).to_le_bytes());
    d[8..12].copy_from_slice(&(0x40u32).to_le_bytes());
    let format: u32 = (0x0C << 8) | (2 << 4) | (1 << 16) | (2 << 20) | (2 << 24);
    d[24..28].copy_from_slice(&format.to_le_bytes());
    d.extend_from_slice(&[0x00, 0xF8, 0x00, 0x00, 0, 0, 0, 0]);
    d
}

fn xbe_with_image(title: &str, id: u32, region: u32) -> Vec<u8> {
    let img = red_xpr();
    let at = 0x3000u32;
    let mut d = xbe::synthetic(title, id, region, Some((at, img.len() as u32)));
    d.resize(at as usize, 0xCC);
    d.extend_from_slice(&img);
    d
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u32).wrapping_mul(2654435761).rotate_left(seed as u32) as u8).collect()
}

/// Lays out a fake Xbox: F:\Games\Test Game, F:\Another, and an E: drive with no Games folder.
fn make_tree(root: &Path) -> PathBuf {
    let game = root.join("F/Games/Test Game");
    std::fs::create_dir_all(game.join("media/sub")).unwrap();
    std::fs::write(game.join("default.xbe"), xbe_with_image("Test Game: Turbo", 0x4D53_0042, 1)).unwrap();
    std::fs::write(game.join("media/big.bin"), pattern(3 * 1024 * 1024 + 123, 3)).unwrap();
    std::fs::write(game.join("media/sub/notes.txt"), b"hello from the xbox\n").unwrap();
    std::fs::write(game.join("empty.dat"), b"").unwrap();

    let other = root.join("F/Another");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("default.xbe"), xbe::synthetic("Another One", 0x5553_0001, 7, None)).unwrap();

    std::fs::create_dir_all(root.join("E/UDATA")).unwrap();
    std::fs::create_dir_all(root.join("C")).unwrap();
    game
}

async fn start_server(root: PathBuf, passive: std::ops::RangeInclusive<u16>) -> ConnectionInfo {
    let port = free_port();
    let server = libunftp::ServerBuilder::new(Box::new(move || {
        unftp_sbe_fs::Filesystem::new(root.clone()).unwrap()
    }))
    .greeting("UnleashX FTP test server")
    .passive_ports(passive)
    .build()
    .unwrap();
    tokio::spawn(server.listen(format!("127.0.0.1:{port}")));
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    ConnectionInfo::new("127.0.0.1").with_port(port)
}

trait WithPort {
    fn with_port(self, port: u16) -> Self;
}
impl WithPort for ConnectionInfo {
    fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

async fn pack_locally(dir: &Path, out: &Path) {
    let mut fs = xdvdfs::write::fs::StdFilesystem::create(dir);
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(out).unwrap();
    xdvdfs::write::img::create_xdvdfs_image(&mut fs, &mut f, |_| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn scans_and_reads_title_image() {
    let tmp = tempfile::tempdir().unwrap();
    make_tree(tmp.path());
    let info = start_server(tmp.path().to_path_buf(), 50100..=50140).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();
    assert!(ftp.welcome().unwrap().contains("UnleashX"));

    let opts = ScanOptions {
        roots: vec![ScanRoot::parse("F").unwrap(), ScanRoot::parse("E:/Games").unwrap(), ScanRoot::parse("G").unwrap()],
        depth: 2,
    };
    let mut events = Vec::new();
    let mut titles = scan::scan(&mut ftp, &opts, |e| events.push(e)).await.unwrap();
    titles.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(titles.len(), 2, "{titles:#?}");

    let t = &titles[1];
    assert_eq!(t.path, "/F/Games/Test Game");
    assert_eq!(t.title_name, "Test Game: Turbo");
    assert_eq!(t.title_id, "4D530042");
    assert_eq!(t.title_code.as_deref(), Some("MS-066"));
    assert_eq!(t.region, "USA");
    assert_eq!(t.iso_name, "Test Game - Turbo (USA).iso");
    assert!(t.error.is_none());

    assert_eq!(titles[0].title_name, "Another One");
    assert_eq!(titles[0].region, "World");
    assert!(events.iter().any(|e| matches!(e, scan::ScanEvent::RootMissing { root } if root == "G:")));

    let png = scan::title_image(&mut ftp, t).await.unwrap().unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    assert_eq!(scan::title_image(&mut ftp, &titles[0]).await.unwrap(), None);

    // The session still works after the partial reads above aborted their transfers.
    let (bytes, files) = scan::folder_size(&mut ftp, &t.path).await.unwrap();
    assert_eq!(files, 4);
    assert_eq!(bytes, 0x3000 + 0x48 + 3 * 1024 * 1024 + 123 + 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn streamed_iso_matches_local_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let game = make_tree(tmp.path());
    let out_dir = tempfile::tempdir().unwrap();
    let info = start_server(tmp.path().to_path_buf(), 50200..=50240).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let progress: ProgressFn = Arc::new(move |p| s.lock().unwrap().push(p));

    let out = out_dir.path().join("Test Game (USA).iso");
    let bytes = iso::create_iso(&mut ftp, "/F/Games/Test Game", &out, progress, Cancel::new())
        .await
        .unwrap();
    assert_eq!(bytes, std::fs::metadata(&out).unwrap().len());
    assert!(!iso::part_path(&out).exists());

    let local = out_dir.path().join("local.iso");
    pack_locally(&game, &local).await;
    let a = std::fs::read(&out).unwrap();
    let b = std::fs::read(&local).unwrap();
    assert_eq!(a.len(), b.len(), "image sizes differ");
    assert!(a == b, "streamed image differs from a local pack of the same folder");

    let seen = seen.lock().unwrap();
    let last_copy = seen
        .iter()
        .rev()
        .find_map(|p| match p {
            IsoProgress::Copying { done, total, .. } => Some((*done, *total)),
            _ => None,
        })
        .unwrap();
    assert_eq!(last_copy.0, last_copy.1);
    assert!(matches!(seen.last(), Some(IsoProgress::Done { .. })));

    // If extract-xiso is built next door, it must accept the image too.
    let tool = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../extract-xiso/build/extract-xiso");
    if tool.exists() {
        let o = std::process::Command::new(&tool).arg("-l").arg(&out).output().unwrap();
        let listing = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success(), "extract-xiso -l failed: {listing}");
        assert!(listing.contains("notes.txt"), "{listing}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_job_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    make_tree(tmp.path());
    let out_dir = tempfile::tempdir().unwrap();
    let info = start_server(tmp.path().to_path_buf(), 50300..=50340).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();

    let cancel = Cancel::new();
    let c = cancel.clone();
    let progress: ProgressFn = Arc::new(move |p| {
        if let IsoProgress::Copying { done, .. } = p {
            if done > 1024 * 1024 {
                c.cancel();
            }
        }
    });
    let out = out_dir.path().join("x.iso");
    let err = iso::create_iso(&mut ftp, "/F/Games/Test Game", &out, progress, cancel).await.unwrap_err();
    assert!(matches!(err, xib_core::Error::Cancelled), "{err}");
    assert!(!out.exists());
    assert!(!iso::part_path(&out).exists());

    // And the session is usable for the next job.
    assert!(!ftp.list_dir("/F/Games").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rescan_reuses_titles_whose_xbe_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    make_tree(tmp.path());
    let info = start_server(tmp.path().to_path_buf(), 50400..=50440).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();
    let opts = ScanOptions { roots: vec![ScanRoot::parse("F").unwrap()], depth: 2 };

    let first = scan::scan(&mut ftp, &opts, |_| {}).await.unwrap();
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|t| !t.cached));

    // Mark the cache so a reused entry is recognisable, and record a measured size.
    let mut known = std::collections::HashMap::new();
    for t in &first {
        let mut t = t.clone();
        t.title_name = format!("cached {}", t.title_name);
        t.size_bytes = Some(42);
        known.insert(scan::cache_key(&t.path), t);
    }

    // Change one XBE's size on disk: that title must be read again.
    let other = tmp.path().join("F/Another/default.xbe");
    let mut bytes = std::fs::read(&other).unwrap();
    bytes.extend_from_slice(&[0u8; 16]);
    std::fs::write(&other, bytes).unwrap();

    let mut second = scan::scan_with_cache(&mut ftp, &opts, &known, |_| {}).await.unwrap();
    second.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(second[0].path, "/F/Another");
    assert!(!second[0].cached);
    assert_eq!(second[0].title_name, "Another One");
    assert_eq!(second[0].size_bytes, None);

    assert_eq!(second[1].path, "/F/Games/Test Game");
    assert!(second[1].cached);
    assert_eq!(second[1].title_name, "cached Test Game: Turbo");
    assert_eq!(second[1].size_bytes, Some(42));
}

fn tree_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((p.strip_prefix(root).unwrap().to_path_buf(), std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_folder_matches_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let game = make_tree(tmp.path());
    let out_dir = tempfile::tempdir().unwrap();
    let info = start_server(tmp.path().to_path_buf(), 50500..=50540).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();

    let out = out_dir.path().join("Test Game (USA)");
    std::fs::create_dir_all(out.join("stale")).unwrap();
    let progress: ProgressFn = Arc::new(|_| {});
    let bytes = xib_core::copy::copy_folder(&mut ftp, "/F/Games/Test Game", &out, progress, Cancel::new())
        .await
        .unwrap();

    assert!(!iso::part_path(&out).exists());
    assert!(!out.join("stale").exists(), "an existing folder is replaced, not merged");
    assert!(out.join("media/sub").is_dir());
    let copied = tree_files(&out);
    assert_eq!(copied, tree_files(&game));
    assert_eq!(bytes, copied.iter().map(|(_, d)| d.len() as u64).sum::<u64>());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_copy_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    make_tree(tmp.path());
    let out_dir = tempfile::tempdir().unwrap();
    let info = start_server(tmp.path().to_path_buf(), 50600..=50640).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();

    let cancel = Cancel::new();
    let c = cancel.clone();
    let progress: ProgressFn = Arc::new(move |p| {
        if let IsoProgress::Copying { done, .. } = p {
            if done > 1024 * 1024 {
                c.cancel();
            }
        }
    });
    let out = out_dir.path().join("x");
    let err = xib_core::copy::copy_folder(&mut ftp, "/F/Games/Test Game", &out, progress, cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, xib_core::Error::Cancelled), "{err}");
    assert!(!out.exists());
    assert!(!iso::part_path(&out).exists());
    assert!(!ftp.list_dir("/F").await.unwrap().is_empty());
}
