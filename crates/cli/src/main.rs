//! `xib`: the batcher without a GUI. Useful for scripting, and for trying a new FTP server.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};
use xib_core::ftpfs::{Cancel, IsoProgress, ProgressFn};
use xib_core::scan::{self, ScanEvent, ScanOptions, ScanRoot};
use xib_core::{discover, iso, ConnectionInfo, XboxFtp};

#[derive(Parser)]
#[command(name = "xib", version, about = "List titles on an original Xbox over FTP and stream them into ISOs")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Conn {
    /// The Xbox's IP address or host name.
    #[arg(long, short = 'H')]
    host: String,
    #[arg(long, default_value_t = 21)]
    port: u16,
    #[arg(long, default_value = "xbox")]
    user: String,
    #[arg(long, default_value = "xbox")]
    password: String,
}

impl Conn {
    fn info(&self) -> ConnectionInfo {
        ConnectionInfo {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Look for FTP servers on the local network.
    Discover {
        /// Any address in the /24 to search; defaults to each of this machine's networks.
        #[arg(long)]
        near: Option<std::net::Ipv4Addr>,
        #[arg(long, default_value_t = 21)]
        port: u16,
    },
    /// List a remote folder.
    Ls {
        #[command(flatten)]
        conn: Conn,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Find titles on the Xbox's drives.
    Scan {
        #[command(flatten)]
        conn: Conn,
        /// Where to look, e.g. F, E:/Games. Repeatable. Defaults to F, E:/Games and G.
        #[arg(long = "root")]
        roots: Vec<String>,
        #[arg(long, default_value_t = 2)]
        depth: u32,
        /// Also total each title's size (one listing per folder).
        #[arg(long)]
        sizes: bool,
        /// Print the titles as JSON.
        #[arg(long)]
        json: bool,
        /// A file of titles from an earlier scan (the GUI keeps one per console). Titles whose
        /// XBE has not changed size are taken from it instead of being read again; the file is
        /// rewritten with this scan's results.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Save each title's cover (from its XBE) as PNG.
    Covers {
        #[command(flatten)]
        conn: Conn,
        /// Output folder.
        #[arg(long, short)]
        out: PathBuf,
    },
    /// Print bytes of a remote file in hex (for looking at a format a server sends).
    Peek {
        #[command(flatten)]
        conn: Conn,
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = 64)]
        len: usize,
    },
    /// Stream game folders into ISOs.
    Iso {
        #[command(flatten)]
        conn: Conn,
        /// Output folder.
        #[arg(long, short)]
        out: PathBuf,
        /// Copy the folders as plain files instead of building ISOs (what xboxrecomp needs).
        #[arg(long)]
        files: bool,
        /// Remote game folders, e.g. "/F/Games/Halo".
        #[arg(required = true)]
        folders: Vec<String>,
    },
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,suppaftp=error")).init();
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> xib_core::Result<()> {
    match cli.cmd {
        Cmd::Discover { near, port } => {
            eprintln!("searching for FTP servers on port {port}...");
            let d = discover::discover(near, port, |_| {}).await;
            for n in &d.networks {
                eprintln!("searched the /24 of {} {}", n.address, n.adapter);
            }
            if d.found.is_empty() {
                println!("nothing answered on port {port}");
            }
            for f in d.found {
                let tag = if f.looks_like_xbox { "xbox " } else { "     " };
                println!("{tag} {:<15} {}", f.host, f.banner);
            }
        }
        Cmd::Ls { conn, path } => {
            let mut ftp = XboxFtp::connect(&conn.info()).await?;
            if let Some(w) = ftp.welcome() {
                eprintln!("{w}");
            }
            for e in ftp.list_dir(&path).await? {
                let kind = if e.is_dir { "dir " } else { "file" };
                println!("{kind} {:>12}  {}", e.size, e.name);
            }
        }
        Cmd::Scan { conn, roots, depth, sizes, json, cache } => {
            let mut ftp = XboxFtp::connect(&conn.info()).await?;
            if let Some(w) = ftp.welcome() {
                eprintln!("{w}");
            }
            let mut opts = ScanOptions { depth, ..ScanOptions::default() };
            if !roots.is_empty() {
                opts.roots = roots.iter().filter_map(|r| ScanRoot::parse(r)).collect();
            }
            let known: std::collections::HashMap<String, scan::Title> = cache
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| serde_json::from_slice::<Vec<scan::Title>>(&b).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|t| (scan::cache_key(&t.path), t))
                .collect();
            let started = Instant::now();
            let titles = scan::scan_with_cache(&mut ftp, &opts, &known, |e| match e {
                ScanEvent::Visiting { path } => log::info!("visiting {path}"),
                ScanEvent::RootMissing { root } => eprintln!("({root} not on this Xbox)"),
                ScanEvent::Skipped { path, reason } => eprintln!("(skipped {path}: {reason})"),
                ScanEvent::Found { title } => {
                    if !json {
                        eprintln!("found {}", title.path);
                    }
                }
            })
            .await?;
            let reused = titles.iter().filter(|t| t.cached).count();
            eprintln!(
                "{} titles in {:.1}s ({reused} unchanged since the cached scan)",
                titles.len(),
                started.elapsed().as_secs_f32()
            );
            if let Some(p) = &cache {
                std::fs::write(p, serde_json::to_vec(&titles).unwrap())?;
            }

            let mut sized = Vec::new();
            for t in &titles {
                let size = if sizes { Some(scan::folder_size(&mut ftp, &t.path).await?) } else { None };
                sized.push((t, size));
            }
            if json {
                let v: Vec<_> = sized
                    .iter()
                    .map(|(t, s)| {
                        let mut j = serde_json::to_value(t).unwrap();
                        if let Some((b, f)) = s {
                            j["sizeBytes"] = (*b).into();
                            j["fileCount"] = (*f).into();
                        }
                        j
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                for (t, s) in sized {
                    let size = s.map(|(b, _)| human(b)).unwrap_or_default();
                    let id = t.title_code.clone().unwrap_or_else(|| t.title_id.clone());
                    let err = t.error.as_deref().map(|e| format!("  [{e}]")).unwrap_or_default();
                    println!("{:<9} {:<8} {:>9}  {:<40} {}{err}", id, t.region, size, t.title_name, t.path);
                }
            }
        }
        Cmd::Peek { conn, path, offset, len } => {
            let mut ftp = XboxFtp::connect(&conn.info()).await?;
            let data = ftp.read_prefix(&path, offset, len, None, |_| Some(len)).await?;
            for (i, row) in data.chunks(16).enumerate() {
                let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
                let text: String = row.iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '.' }).collect();
                println!("{:08x}  {:<48} {text}", offset + (i * 16) as u64, hex.join(" "));
            }
        }
        Cmd::Covers { conn, out } => {
            let mut ftp = XboxFtp::connect(&conn.info()).await?;
            let titles = scan::scan(&mut ftp, &ScanOptions::default(), |_| {}).await?;
            std::fs::create_dir_all(&out)?;
            for t in titles {
                match scan::title_image(&mut ftp, &t).await {
                    Ok(Some(png)) => {
                        let name = t.iso_name.trim_end_matches(".iso").to_string() + ".png";
                        std::fs::write(out.join(&name), png)?;
                        println!("{name}");
                    }
                    Ok(None) => println!("({} has no cover)", t.title_name),
                    Err(e) => println!("({}: {e})", t.title_name),
                }
            }
        }
        Cmd::Iso { conn, out, files, folders } => {
            let mut ftp = XboxFtp::connect(&conn.info()).await?;
            for folder in folders {
                let dir = folder.trim_end_matches('/').to_string();
                let (parent, _) = xib_core::ftp::split(&dir);
                let entry = ftp
                    .list_dir(parent)
                    .await?
                    .into_iter()
                    .find(|e| e.is_dir && xib_core::ftp::join(parent, &e.name) == dir);
                let name = match entry {
                    Some(_) => {
                        let xbe = ftp
                            .list_dir(&dir)
                            .await?
                            .into_iter()
                            .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case("default.xbe"));
                        match xbe {
                            Some(x) => scan::read_title(&mut ftp, &dir, &x).await.iso_name,
                            None => iso::default_iso_name(xib_core::ftp::split(&dir).1, "", 0),
                        }
                    }
                    None => {
                        eprintln!("{dir}: no such folder");
                        continue;
                    }
                };
                let name = if files { xib_core::copy::default_folder_name(&name) } else { name };
                let path = out.join(&name);
                eprintln!("{dir} -> {}", path.display());
                let started = Instant::now();
                let progress: ProgressFn = Arc::new(move |p| match p {
                    IsoProgress::Listing { dirs, files, bytes } => {
                        eprint!("\r  listing: {dirs} folders, {files} files, {}        ", human(bytes));
                    }
                    IsoProgress::Copying { done, total, .. } => {
                        let secs = started.elapsed().as_secs_f64().max(0.001);
                        let pct = if total > 0 { done as f64 * 100.0 / total as f64 } else { 100.0 };
                        eprint!(
                            "\r  {pct:5.1}%  {} of {}  {}/s        ",
                            human(done),
                            human(total),
                            human((done as f64 / secs) as u64)
                        );
                        let _ = std::io::stderr().flush();
                    }
                    IsoProgress::Finishing => eprint!("\r  finishing...                              "),
                    IsoProgress::Done { bytes } => eprintln!("\r  done: {}                                   ", human(bytes)),
                });
                if files {
                    xib_core::copy::copy_folder(&mut ftp, &dir, &path, progress, Cancel::new()).await?;
                } else {
                    iso::create_iso(&mut ftp, &dir, &path, progress, Cancel::new()).await?;
                }
            }
        }
    }
    Ok(())
}
