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

#[tokio::test(flavor = "multi_thread")]
async fn upload_download_rename_and_delete() {
    use xib_core::transfer;

    let xbox = tempfile::tempdir().unwrap();
    make_tree(xbox.path());
    let info = start_server(xbox.path().to_path_buf(), 50700..=50740).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();
    let none: ProgressFn = Arc::new(|_| {});

    // A local folder with a subfolder, an empty file, and junk the Xbox should not get.
    let pc = tempfile::tempdir().unwrap();
    let src = pc.path().join("Homebrew");
    std::fs::create_dir_all(src.join("media")).unwrap();
    std::fs::write(src.join("default.xbe"), pattern(2 * 1024 * 1024 + 7, 5)).unwrap();
    std::fs::write(src.join("media/empty.bin"), b"").unwrap();
    std::fs::write(src.join(".DS_Store"), b"junk").unwrap();

    let sent = transfer::upload(&mut ftp, &src, "/E/Apps", none.clone(), Cancel::new()).await;
    assert!(sent.is_err(), "the parent folder does not exist yet");
    ftp.mkdir("/E/Apps").await.unwrap();
    let sent = transfer::upload(&mut ftp, &src, "/E/Apps", none.clone(), Cancel::new()).await.unwrap();
    assert_eq!(sent, 2 * 1024 * 1024 + 7);
    let on_xbox = xbox.path().join("E/Apps/Homebrew");
    assert!(!on_xbox.join(".DS_Store").exists());
    assert_eq!(std::fs::read(on_xbox.join("default.xbe")).unwrap(), std::fs::read(src.join("default.xbe")).unwrap());
    assert!(on_xbox.join("media/empty.bin").exists());

    // Uploading again merges into the folder that is now there.
    transfer::upload(&mut ftp, &src, "/E/Apps", none.clone(), Cancel::new()).await.unwrap();

    // A single file, then back down again.
    let one = pc.path().join("notes.txt");
    std::fs::write(&one, b"from the pc").unwrap();
    transfer::upload(&mut ftp, &one, "/E/Apps", none.clone(), Cancel::new()).await.unwrap();
    let back = pc.path().join("back.txt");
    transfer::download(&mut ftp, "/E/Apps/notes.txt", false, 11, &back, none.clone(), Cancel::new()).await.unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), b"from the pc");
    let back_dir = pc.path().join("Back");
    transfer::download(&mut ftp, "/E/Apps/Homebrew", true, 0, &back_dir, none.clone(), Cancel::new()).await.unwrap();
    assert_eq!(std::fs::read(back_dir.join("default.xbe")).unwrap(), std::fs::read(src.join("default.xbe")).unwrap());

    ftp.rename("/E/Apps/notes.txt", "readme.txt").await.unwrap();
    let names: Vec<String> = ftp.list_dir("/E/Apps").await.unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"readme.txt".to_string()) && !names.contains(&"notes.txt".to_string()), "{names:?}");

    transfer::remove(&mut ftp, "/E/Apps/Homebrew", true).await.unwrap();
    transfer::remove(&mut ftp, "/E/Apps/readme.txt", false).await.unwrap();
    assert!(ftp.list_dir("/E/Apps").await.unwrap().is_empty());
    assert!(transfer::remove(&mut ftp, "/E", true).await.is_err());
    assert!(xbox.path().join("E/UDATA").exists());

    // A name FATX cannot hold is refused before anything is sent.
    let long = pc.path().join("a".repeat(43));
    std::fs::write(&long, b"x").unwrap();
    assert!(transfer::upload(&mut ftp, &long, "/E/Apps", none.clone(), Cancel::new()).await.is_err());
    assert!(ftp.list_dir("/E/Apps").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_upload_removes_the_partial_file() {
    use xib_core::transfer;

    let xbox = tempfile::tempdir().unwrap();
    make_tree(xbox.path());
    let info = start_server(xbox.path().to_path_buf(), 50800..=50840).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();

    let pc = tempfile::tempdir().unwrap();
    let big = pc.path().join("big.bin");
    std::fs::write(&big, pattern(4 * 1024 * 1024, 1)).unwrap();
    let cancel = Cancel::new();
    let c = cancel.clone();
    let progress: ProgressFn = Arc::new(move |p| {
        if let IsoProgress::Copying { done, .. } = p {
            if done > 1024 * 1024 {
                c.cancel();
            }
        }
    });
    let err = transfer::upload(&mut ftp, &big, "/E", progress, cancel).await.unwrap_err();
    assert!(matches!(err, xib_core::Error::Cancelled), "{err}");
    assert!(!xbox.path().join("E/big.bin").exists());
    assert!(!ftp.list_dir("/E").await.unwrap().is_empty());
}

/// What [`flaky_proxy`] does to the first command with a given verb.
#[derive(Clone, Copy)]
enum Fault {
    /// Answers `502` instead of passing the command on, as Xbox servers do now and then.
    Refuse,
    /// Hangs up the session, as a server that dropped an idle session has.
    HangUp,
}

/// A control-connection proxy in front of the FTP server on `upstream` that misbehaves the way
/// Xbox servers do. Each fault applies once, to the first command with its verb, across all
/// sessions. `greeting` lines are put in front of the server's greeting. Data connections go
/// straight to the server: the client connects to the port from `PASV` on the proxy's address,
/// which is the server's too.
async fn flaky_proxy(upstream: u16, faults: &[(&'static str, Fault)], greeting: &'static str) -> ConnectionInfo {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let faults = Arc::new(Mutex::new(faults.iter().map(|&(v, f)| (v, Some(f))).collect::<Vec<_>>()));
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.unwrap();
            let server = TcpStream::connect(("127.0.0.1", upstream)).await.unwrap();
            let faults = faults.clone();
            tokio::spawn(async move {
                let (client_r, client_w) = client.into_split();
                let (server_r, mut server_w) = server.into_split();
                let client_w = Arc::new(tokio::sync::Mutex::new(client_w));
                let to_client = client_w.clone();
                let replies = tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    let mut first = true;
                    while let Ok(Some(l)) = lines.next_line().await {
                        let mut w = to_client.lock().await;
                        if std::mem::take(&mut first) {
                            let _ = w.write_all(greeting.as_bytes()).await;
                        }
                        if w.write_all(format!("{l}\r\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
                let mut lines = BufReader::new(client_r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let verb = l.split(' ').next().unwrap_or("").to_ascii_uppercase();
                    let fault = faults.lock().unwrap().iter_mut().find(|(v, f)| *v == verb && f.is_some()).and_then(|(_, f)| f.take());
                    match fault {
                        Some(Fault::Refuse) => {
                            let _ = client_w.lock().await.write_all(b"502 Command not implemented.\r\n").await;
                        }
                        Some(Fault::HangUp) => break,
                        None => {
                            if server_w.write_all(format!("{l}\r\n").as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                replies.abort();
                let _ = client_w.lock().await.shutdown().await;
            });
        }
    });
    ConnectionInfo::new("127.0.0.1").with_port(port)
}

#[tokio::test(flavor = "multi_thread")]
async fn recovers_from_502s_and_dropped_sessions() {
    use xib_core::transfer;

    let xbox = tempfile::tempdir().unwrap();
    let game = make_tree(xbox.path());
    let server = start_server(xbox.path().to_path_buf(), 50900..=50940).await;
    let faults = [("CWD", Fault::HangUp), ("LIST", Fault::Refuse), ("RETR", Fault::Refuse), ("STOR", Fault::Refuse), ("MKD", Fault::HangUp)];
    let info = flaky_proxy(server.port, &faults, "").await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();
    let none: ProgressFn = Arc::new(|_| {});

    // The first CWD loses the session and the first LIST is refused; the listing still arrives.
    let names: Vec<String> = ftp.list_dir("/F/Games").await.unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["Test Game".to_string()]);

    ftp.mkdir("/E/Apps").await.unwrap();
    assert!(xbox.path().join("E/Apps").is_dir());

    let pc = tempfile::tempdir().unwrap();
    let one = pc.path().join("notes.bin");
    std::fs::write(&one, pattern(300_000, 9)).unwrap();
    transfer::upload(&mut ftp, &one, "/E/Apps", none.clone(), Cancel::new()).await.unwrap();
    assert_eq!(std::fs::read(xbox.path().join("E/Apps/notes.bin")).unwrap(), std::fs::read(&one).unwrap());

    let back = pc.path().join("Game");
    transfer::download(&mut ftp, "/F/Games/Test Game", true, 0, &back, none.clone(), Cancel::new()).await.unwrap();
    assert_eq!(
        std::fs::read(back.join("media/big.bin")).unwrap(),
        std::fs::read(game.join("media/big.bin")).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_refused_when_the_xbox_drive_is_full() {
    use xib_core::transfer;

    let xbox = tempfile::tempdir().unwrap();
    make_tree(xbox.path());
    let server = start_server(xbox.path().to_path_buf(), 50950..=50990).await;
    let greeting = "220-Drive E:  Used: 7.50GB  Free: 1.00MB\r\n220-Drive F:  Used: 1.00GB  Free: 100.00GB\r\n";
    let info = flaky_proxy(server.port, &[], greeting).await;
    let mut ftp = XboxFtp::connect(&info).await.unwrap();
    let none: ProgressFn = Arc::new(|_| {});

    let pc = tempfile::tempdir().unwrap();
    let big = pc.path().join("big.bin");
    std::fs::write(&big, pattern(2 * 1024 * 1024, 2)).unwrap();
    let err = transfer::upload(&mut ftp, &big, "/E", none.clone(), Cancel::new()).await.unwrap_err();
    assert!(matches!(err, xib_core::Error::NoSpace { .. }), "{err}");
    assert!(!xbox.path().join("E/big.bin").exists());

    // F: has room.
    transfer::upload(&mut ftp, &big, "/F", none, Cancel::new()).await.unwrap();
    assert!(xbox.path().join("F/big.bin").exists());
}
