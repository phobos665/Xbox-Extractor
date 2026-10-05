//! A pretend Xbox for working on the GUI without a console: an FTP server on localhost serving
//! C:, E: and F: drives with a handful of titles, each with a generated cover.
//!
//!     cargo run -p xib-core --example fake_xbox            # 127.0.0.1:2121, user/pass anything
//!     cargo run -p xib-core --example fake_xbox -- 2121 64  # port, MB of filler per title
//!
//! The titles are synthetic XBE headers plus filler data. Nothing here is game content.

use std::path::Path;

const TITLES: &[(&str, &str, u32, u32, [u8; 3])] = &[
    ("F/Games/Test Racer", "Test Racer: Turbo", 0x4D53_0042, 1, [220, 60, 40]),
    ("F/Games/Space Shooter", "Space Shooter", 0x4541_0011, 7, [60, 120, 230]),
    ("F/Games/Puzzle Time (Disc 2)", "Puzzle Time", 0x5553_0100, 4, [240, 200, 40]),
    ("F/Another Folder", "Another One", 0x5448_0007, 2, [140, 70, 200]),
    ("E/Games/Homebrew Demo", "Homebrew Demo", 0x0000_0001, 7, [70, 200, 120]),
];

/// An XPR0 bundle holding a 64x64 DXT1 texture: a diagonal gradient of `rgb` to black.
fn cover_xpr(rgb: [u8; 3]) -> Vec<u8> {
    let (w, h) = (64u32, 64u32);
    let mut d = vec![0u8; 0x40];
    d[0..4].copy_from_slice(b"XPR0");
    d[8..12].copy_from_slice(&(0x40u32).to_le_bytes());
    let format: u32 = (0x0C << 8) | (2 << 4) | (1 << 16) | (6 << 20) | (6 << 24);
    d[24..28].copy_from_slice(&format.to_le_bytes());
    let to565 = |r: u32, g: u32, b: u32| (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)) as u16;
    for by in 0..h / 4 {
        for bx in 0..w / 4 {
            let t = (bx + by) as f32 / ((w / 4 + h / 4) as f32);
            let k = |c: u8| (c as f32 * (1.0 - t * 0.8)) as u32;
            let c0 = to565(k(rgb[0]), k(rgb[1]), k(rgb[2]));
            let c1 = to565(k(rgb[0]) / 3, k(rgb[1]) / 3, k(rgb[2]) / 3);
            let (c0, c1) = if c0 > c1 { (c0, c1) } else { (c1, c0) };
            d.extend_from_slice(&c0.to_le_bytes());
            d.extend_from_slice(&c1.to_le_bytes());
            d.extend_from_slice(&0x5A5A_0000u32.to_le_bytes());
        }
    }
    let total = d.len() as u32;
    d[4..8].copy_from_slice(&total.to_le_bytes());
    d
}

fn write_title(root: &Path, dir: &str, name: &str, id: u32, region: u32, rgb: [u8; 3], filler_mb: usize) {
    let game = root.join(dir);
    std::fs::create_dir_all(game.join("media")).unwrap();
    let img = cover_xpr(rgb);
    let at = 0x4000u32;
    let mut xbe = xbe::synthetic(name, id, region, Some((at, img.len() as u32)));
    xbe.resize(at as usize, 0);
    xbe.extend_from_slice(&img);
    std::fs::write(game.join("default.xbe"), xbe).unwrap();
    let filler: Vec<u8> = (0..filler_mb * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(game.join("media/data.bin"), filler).unwrap();
    std::fs::write(game.join("media/readme.txt"), format!("{name}\n")).unwrap();
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(2121);
    let filler_mb: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(16);

    let root = std::env::temp_dir().join("xib-fake-xbox");
    let _ = std::fs::remove_dir_all(&root);
    for (dir, name, id, region, rgb) in TITLES {
        write_title(&root, dir, name, *id, *region, *rgb, filler_mb);
    }
    std::fs::create_dir_all(root.join("C")).unwrap();
    std::fs::create_dir_all(root.join("E/UDATA")).unwrap();

    println!("fake Xbox on 127.0.0.1:{port}, serving {}", root.display());
    let r = root.clone();
    let server = libunftp::ServerBuilder::new(Box::new(move || unftp_sbe_fs::Filesystem::new(r.clone()).unwrap()))
        .greeting("UnleashX FTP Server (fake)")
        .passive_ports(51000..=51100)
        .build()
        .unwrap();
    server.listen(format!("127.0.0.1:{port}")).await.unwrap();
}
