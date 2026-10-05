//! The Extract tab: unpacking ISOs with extract-xiso itself.
//!
//! extract-xiso is the reference for this format, so rather than reimplement it the app ships
//! it as a sidecar (third_party/extract-xiso, built by app/scripts/build-sidecar.mjs) and runs
//! it once per ISO: `-l` first for the total size, then `-x -d <folder>`. Progress is the size
//! of what has landed in the output folder, measured twice a second, because extract-xiso's
//! own progress lines reach a pipe in 4 KB bursts.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;
use xib_core::ftpfs::Cancel;

use crate::jobs::JobState;

pub const EVENT: &str = "extract://update";
const SIDECAR: &str = "extract-xiso";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractUpdate {
    pub id: u64,
    pub state: JobState,
    pub total_bytes: Option<u64>,
    pub done_bytes: Option<u64>,
    pub total_files: Option<u64>,
    /// The file extract-xiso last reported, relative to the image root.
    pub current: Option<String>,
    pub message: Option<String>,
    pub out_dir: Option<String>,
}

impl ExtractUpdate {
    fn state(id: u64, state: JobState) -> Self {
        Self {
            id,
            state,
            total_bytes: None,
            done_bytes: None,
            total_files: None,
            current: None,
            message: None,
            out_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedExtract {
    pub id: u64,
    pub iso: String,
    pub out_dir: String,
}

struct Item {
    id: u64,
    iso: PathBuf,
    out_dir: PathBuf,
    skip_system_update: bool,
    overwrite: bool,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Item>,
    cancels: HashMap<u64, Cancel>,
    worker: bool,
    next_id: u64,
}

#[derive(Default)]
pub struct Extracts {
    inner: Mutex<Inner>,
}

/// The folder an ISO extracts into: its file name without `.iso`.
fn out_dir_for(iso: &Path, dest: &Path) -> PathBuf {
    let stem = iso.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "xiso".into());
    dest.join(stem)
}

#[tauri::command]
pub fn enqueue_extracts(
    app: AppHandle,
    extracts: State<'_, Arc<Extracts>>,
    isos: Vec<String>,
    dest_dir: String,
    skip_system_update: bool,
    overwrite: bool,
) -> Result<Vec<QueuedExtract>, String> {
    if dest_dir.trim().is_empty() {
        return Err("choose a folder to extract into first".into());
    }
    let dest = PathBuf::from(dest_dir);
    let mut queued = Vec::new();
    let spawn;
    {
        let mut inner = extracts.inner.lock().unwrap();
        for iso in isos {
            inner.next_id += 1;
            let id = inner.next_id;
            let iso = PathBuf::from(iso);
            let out_dir = out_dir_for(&iso, &dest);
            inner.cancels.insert(id, Cancel::new());
            queued.push(QueuedExtract {
                id,
                iso: iso.to_string_lossy().into_owned(),
                out_dir: out_dir.to_string_lossy().into_owned(),
            });
            inner.queue.push_back(Item { id, iso, out_dir, skip_system_update, overwrite });
        }
        spawn = !inner.worker && !inner.queue.is_empty();
        if spawn {
            inner.worker = true;
        }
    }
    if spawn {
        let me: Arc<Extracts> = Arc::clone(&extracts);
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move { me.run(app2).await });
    }
    Ok(queued)
}

#[tauri::command]
pub fn cancel_extract(extracts: State<'_, Arc<Extracts>>, id: u64) {
    if let Some(c) = extracts.inner.lock().unwrap().cancels.get(&id) {
        c.cancel();
    }
}

/// extract-xiso's version line, or why it cannot be run. Shown in the Extract tab.
#[tauri::command]
pub async fn sidecar_version(app: AppHandle) -> Result<String, String> {
    let out = app
        .shell()
        .sidecar(SIDECAR)
        .map_err(|e| e.to_string())?
        .arg("-v")
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(text.lines().next().unwrap_or("").to_string())
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(true)
}

/// Reads `N files in <iso> total B bytes` from extract-xiso's `-l` output.
fn parse_totals(stdout: &str) -> Option<(u64, u64)> {
    let line = stdout.lines().rev().find(|l| l.contains(" files in ") && l.contains(" total "))?;
    let files = line.trim().split_whitespace().next()?.parse().ok()?;
    let bytes = line.rsplit(" total ").next()?.trim().trim_end_matches("bytes").trim().parse().ok()?;
    Some((files, bytes))
}

/// The file named in the last `extracting <path> (N bytes) [P%]` segment of `chunk`.
fn current_file(chunk: &str) -> Option<String> {
    let seg = chunk
        .split(['\r', '\n'])
        .rev()
        .find(|s| s.starts_with("extracting ") || s.starts_with("creating "))?;
    let s = seg.trim_start_matches("extracting ").trim_start_matches("creating ");
    let end = s.rfind(" (").unwrap_or(s.len());
    Some(s[..end].to_string())
}

fn tail(text: &str, lines: usize) -> String {
    let v: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

impl Extracts {
    async fn run(self: Arc<Self>, app: AppHandle) {
        loop {
            let (item, cancel) = {
                let mut inner = self.inner.lock().unwrap();
                match inner.queue.pop_front() {
                    Some(i) => {
                        let c = inner.cancels.get(&i.id).cloned().unwrap_or_default();
                        (i, c)
                    }
                    None => {
                        inner.worker = false;
                        break;
                    }
                }
            };
            let update = run_one(&app, &item, &cancel).await;
            self.inner.lock().unwrap().cancels.remove(&item.id);
            let _ = app.emit(EVENT, update);
        }
    }
}

async fn run_one(app: &AppHandle, item: &Item, cancel: &Cancel) -> ExtractUpdate {
    let mut up = ExtractUpdate::state(item.id, JobState::Done);
    up.out_dir = Some(item.out_dir.to_string_lossy().into_owned());
    if cancel.is_cancelled() {
        up.state = JobState::Cancelled;
        return up;
    }
    if !item.iso.is_file() {
        up.state = JobState::Failed;
        up.message = Some("the ISO is no longer there".into());
        return up;
    }
    let existed = item.out_dir.exists();
    if existed && !is_empty_dir(&item.out_dir) && !item.overwrite {
        up.state = JobState::Skipped;
        up.message = Some("the folder already exists".into());
        return up;
    }

    let _ = app.emit(EVENT, ExtractUpdate::state(item.id, JobState::Running));
    let iso = item.iso.to_string_lossy().into_owned();

    // Totals first, from the listing.
    let shell = app.shell();
    let listing = match shell.sidecar(SIDECAR) {
        Ok(cmd) => cmd.args(["-l", iso.as_str()]).output().await,
        Err(e) => {
            up.state = JobState::Failed;
            up.message = Some(format!("extract-xiso is missing from this build: {e}"));
            return up;
        }
    };
    let (total_files, total_bytes) = match listing {
        Ok(out) if out.status.success() => {
            match parse_totals(&String::from_utf8_lossy(&out.stdout)) {
                Some(t) => t,
                None => {
                    up.state = JobState::Failed;
                    up.message = Some("extract-xiso could not read this image".into());
                    return up;
                }
            }
        }
        Ok(out) => {
            up.state = JobState::Failed;
            let err = String::from_utf8_lossy(&out.stderr);
            up.message = Some(if err.trim().is_empty() { "not an Xbox ISO".into() } else { tail(&err, 2) });
            return up;
        }
        Err(e) => {
            up.state = JobState::Failed;
            up.message = Some(e.to_string());
            return up;
        }
    };
    up.total_bytes = Some(total_bytes);
    up.total_files = Some(total_files);

    if let Err(e) = std::fs::create_dir_all(&item.out_dir) {
        up.state = JobState::Failed;
        up.message = Some(e.to_string());
        return up;
    }
    let out_dir = item.out_dir.to_string_lossy().into_owned();
    let out_prefix = out_dir.trim_end_matches(['/', '\\']).to_string();
    let mut args = vec!["-x".to_string()];
    if item.skip_system_update {
        args.push("-s".into());
    }
    args.extend(["-d".into(), out_dir, iso]);

    let spawned = shell.sidecar(SIDECAR).map(|c| c.args(args).set_raw_out(true).spawn());
    let (mut rx, child) = match spawned {
        Ok(Ok(x)) => x,
        Ok(Err(e)) | Err(e) => {
            up.state = JobState::Failed;
            up.message = Some(e.to_string());
            return up;
        }
    };

    let mut child = Some(child);
    let mut current: Option<String> = None;
    let mut stderr = String::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut exit: Option<i32> = None;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(CommandEvent::Stdout(b)) => {
                    if let Some(f) = current_file(&String::from_utf8_lossy(&b)) {
                        // With -d, extract-xiso names files by their full output path.
                        let rel = f.strip_prefix(out_prefix.as_str()).map(str::to_string).unwrap_or(f);
                        current = Some(rel);
                    }
                }
                Some(CommandEvent::Stderr(b)) => stderr.push_str(&String::from_utf8_lossy(&b)),
                Some(CommandEvent::Error(e)) => stderr.push_str(&e),
                Some(CommandEvent::Terminated(t)) => {
                    exit = Some(t.code.unwrap_or(-1));
                    break;
                }
                Some(_) => {}
                None => break,
            },
            _ = tick.tick() => {
                if cancel.is_cancelled() {
                    if let Some(c) = child.take() {
                        let _ = c.kill();
                    }
                    break;
                }
                let dir = item.out_dir.clone();
                let done = tokio::task::spawn_blocking(move || dir_size(&dir)).await.unwrap_or(0);
                let mut p = ExtractUpdate::state(item.id, JobState::Running);
                p.total_bytes = Some(total_bytes);
                p.total_files = Some(total_files);
                p.done_bytes = Some(done.min(total_bytes));
                p.current = current.clone();
                let _ = app.emit(EVENT, p);
            }
        }
    }

    let failed = cancel.is_cancelled() || exit != Some(0);
    if failed {
        if !existed {
            let _ = std::fs::remove_dir_all(&item.out_dir);
        }
        if cancel.is_cancelled() {
            up.state = JobState::Cancelled;
        } else {
            up.state = JobState::Failed;
            let why = tail(&stderr, 2);
            up.message = Some(if why.is_empty() { format!("extract-xiso exited with {exit:?}") } else { why });
        }
        up.out_dir = None;
        return up;
    }
    up.done_bytes = Some(total_bytes);
    if stderr.to_ascii_lowercase().contains("warning") {
        up.message = Some(tail(&stderr, 1));
    }
    up
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_from_listing() {
        let out = "listing Halo.iso:\n\n/default.xbe (123 bytes)\n\n3 files in Halo.iso total 16795730 bytes\n";
        assert_eq!(parse_totals(out), Some((3, 16795730)));
        assert_eq!(parse_totals("nope"), None);
    }

    #[test]
    fn current_file_from_progress() {
        let chunk = "extracting /media/a.bin (100 bytes) [50%]\rextracting /media/b dir/c.bin (9 bytes) [100%]\r\n";
        assert_eq!(current_file(chunk).as_deref(), Some("/media/b dir/c.bin"));
        assert_eq!(current_file("creating /media/ (0 bytes) [OK]\n").as_deref(), Some("/media/"));
    }

    #[test]
    fn out_dir_drops_extension() {
        assert_eq!(out_dir_for(Path::new("/a/Halo (USA).iso"), Path::new("/x")), PathBuf::from("/x/Halo (USA)"));
    }
}
