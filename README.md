# Xbox ISO Batcher

List the games on an original Xbox over FTP and turn any of them into ISOs on your computer, in
bulk. Also extracts ISOs back into folders. An open-source take on the ISO side of
[Qwix](https://avalaunch.net/qwix/), for macOS and Windows.

- **Find the Xbox.** Type its IP address, or press *Find Xbox* to search the local network.
  Most consoles log in with `xbox` / `xbox` on port 21.
- **See every title.** Scans `F:`, `E:\Games` and `G:` (configurable) for folders with a
  `default.xbe`, and shows each title's name, ID, region, size and cover from the XBE.
- **Bulk ISOs, or plain files.** Select titles, press *Create ISOs*. Each folder is streamed from
  the Xbox straight into an XISO: files go directly to their place in the image, and nothing is
  copied to disk first. Switch the *ISO / Files* toggle to *Files* to copy each game folder
  as it is instead: `default.xbe` and its data, which is what
  [xboxrecomp](https://github.com/sp00nznet/xboxrecomp) works from. Jobs run one at a time,
  with progress, speed, ETA and cancel, and a cancelled job leaves nothing behind.
- **Fast rescans.** A rescan lists the drives again but reuses every title whose `default.xbe`
  has not changed size. *Full rescan* reads everything again.
- **Extract ISOs.** The *Extract ISOs* tab unpacks images with
  [extract-xiso](https://github.com/XboxDev/extract-xiso), bundled with the app, one folder per
  ISO. It can skip `$SystemUpdate`.

Tested against UnleashX. EvolutionX, Avalaunch and XBMC use the same commands and should work.

## What the ISOs are

The ISOs are game-partition XISOs, the same kind `extract-xiso -c` makes. They play in xemu and in
the ISO loaders of modded consoles. They are not byte-identical to a disc dump (Redump): a game
copied to a hard drive has lost the disc's video partition and original layout.

Only make ISOs of games you own.

## Layout

```
crates/xbe     XBE header and certificate parsing; title image (XPR/DXT) decoding
crates/core    FTP client for Xbox servers, title scanner, streaming XISO writer (xdvdfs)
crates/cli     `xib`, the same features without the GUI
app/           Tauri 2 app: Rust commands in src-tauri, React front end in src
third_party/   extract-xiso, vendored and built as a sidecar program
```

## Building

You need Rust (stable), Node 22 with pnpm, CMake and a C compiler (Xcode command line tools on
macOS, Visual Studio Build Tools on Windows).

```sh
cd app
pnpm install
pnpm tauri dev      # run
pnpm tauri build    # .app/.dmg on macOS, .msi/.exe on Windows
```

`pnpm tauri dev` and `pnpm tauri build` build extract-xiso first (`app/scripts/build-sidecar.mjs`).
CI builds macOS (Apple Silicon and Intel) and Windows: `.github/workflows/build.yml`. Builds are
not signed, so macOS will ask you to allow the app the first time (right-click, Open).

## Command line

```sh
cargo run -p xib-cli -- discover
cargo run -p xib-cli -- scan -H 192.168.1.20 --sizes
cargo run -p xib-cli -- scan -H 192.168.1.20 --cache scan.json   # reuse unchanged titles
cargo run -p xib-cli -- iso -H 192.168.1.20 -o ~/ISOs "/F/Games/Halo"
cargo run -p xib-cli -- iso -H 192.168.1.20 --files -o ~/games "/F/Games/Halo"   # files, no ISO
cargo run -p xib-cli -- covers -H 192.168.1.20 -o ~/covers
```

## Developing without a console

```sh
cargo run -p xib-core --example fake_xbox    # FTP server on 127.0.0.1:2121 with five fake titles
```

Then connect the app or the CLI to `127.0.0.1`, port `2121`. `cargo test --workspace` runs the
tests, including end-to-end runs against a local FTP server. One of them checks that an ISO
streamed over FTP is byte-identical to one xdvdfs builds from the same folder on disk.

## Notes on Xbox FTP servers

Found on a real UnleashX, and handled:

- **Multi-line greeting.** It runs to a dozen lines of drive statistics; the app shows the line
  that names the server.
- **`REST` is ignored.** UnleashX accepts it but always sends from the start of the file. The
  app detects this and reads through instead.
- **Abandoned transfers are slow.** UnleashX stalls for two to three seconds when a download is
  cut off. Header reads finish small files instead of aborting them, which took a full scan of
  44 titles from 143 s to 33 s.
- **Cover art.** Covers come first from the dashboard's own copy,
  `E:\UDATA\<title id>\TitleImage.xbx`, which is about 10 KB, and otherwise from the XBE.

## Licence

GPL-3.0-or-later; see `LICENSE`.

Third-party code:

- [xdvdfs](https://github.com/antangelo/xdvdfs) (MIT) builds the XISO images.
- [extract-xiso](https://github.com/XboxDev/extract-xiso) is shipped as a separate program under
  its own BSD-style licence (`third_party/extract-xiso/LICENSE.TXT`). This product includes
  software developed by in <in@fishtank.com>.


## Acknowledgements

- [Qwix](https://avalaunch.net/qwix/) - Absolute wizards. I've used their program for many years and truly, thank you for what you've done over the years.
- [extract-xiso](https://github.com/XboxDev/extract-xiso) - Amazing tool and the basis for this wrapper-tool.
- [xdvdfs](https://github.com/antangelo/xdvdfs) - Similar to extract-xiso, a major part of this tooling.

