//! Tauri commands behind the GUI. The work itself is in `xib-core`; this keeps one browsing
//! session to the Xbox, a cache of what the last scan found, and the ISO queue.

mod extract;
mod jobs;
mod settings;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use xib_core::scan::{self, ScanOptions, ScanRoot, Title};
use xib_core::ftp::server_name;
use xib_core::{discover, ConnectionInfo, XboxFtp};

use jobs::{JobRequest, Jobs, OutputMode, QueuedJob};
use settings::Settings;

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Default)]
struct Session {
    ftp: Mutex<Option<XboxFtp>>,
    info: std::sync::Mutex<Option<ConnectionInfo>>,
    /// What the last scan found, keyed by `scan::cache_key`.
    titles: std::sync::Mutex<HashMap<String, Title>>,
}

/// Where a console's last scan is kept, so a rescan can skip titles whose XBE has not changed.
fn scan_cache_path(app: &AppHandle, info: &ConnectionInfo) -> Option<PathBuf> {
    let name: String = format!("{}_{}", info.host, info.port)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' { c } else { '-' })
        .collect();
    Some(app.path().app_cache_dir().ok()?.join("scans").join(format!("{name}.json")))
}

fn load_scan_cache(app: &AppHandle, info: &ConnectionInfo) -> HashMap<String, Title> {
    scan_cache_path(app, info)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Title>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|t| (scan::cache_key(&t.path), t))
        .collect()
}

fn save_scan_cache(app: &AppHandle, session: &Session) {
    let Some(info) = session.info.lock().unwrap().clone() else { return };
    let Some(path) = scan_cache_path(app, &info) else { return };
    let titles: Vec<Title> = session.titles.lock().unwrap().values().cloned().collect();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(&titles) {
        let _ = std::fs::write(path, json);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Connected {
    /// The greeting line that names the server, e.g. "UnleashX FTP Server ready."
    server: Option<String>,
    drives: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanRequest {
    roots: Vec<String>,
    depth: u32,
    /// Read every XBE again instead of reusing unchanged titles from the last scan.
    #[serde(default)]
    full: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderSize {
    bytes: u64,
    files: u64,
}

#[tauri::command]
async fn discover_xboxes(app: AppHandle) -> CmdResult<Vec<discover::Found>> {
    let a = app.clone();
    Ok(discover::discover(None, 21, move |f| {
        let _ = a.emit("discover://found", f);
    })
    .await)
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    info: ConnectionInfo,
    session: State<'_, Session>,
    jobs: State<'_, Arc<Jobs>>,
) -> CmdResult<Connected> {
    let mut ftp = XboxFtp::connect(&info).await.map_err(err)?;
    let drives = scan::drives(&mut ftp).await.map_err(err)?.into_iter().map(|d| d.name).collect();
    let server = ftp.welcome().map(server_name);
    if let Some(old) = session.ftp.lock().await.replace(ftp) {
        old.quit().await;
    }
    *session.info.lock().unwrap() = Some(info.clone());
    jobs.set_connection(Some(info.clone()));

    let mut s = settings::load(&app);
    s.recent_hosts.retain(|h| h != &info.host);
    s.recent_hosts.insert(0, info.host.clone());
    s.recent_hosts.truncate(8);
    s.connection = info;
    let _ = settings::save(&app, &s);
    Ok(Connected { server, drives })
}

#[tauri::command]
async fn disconnect(session: State<'_, Session>, jobs: State<'_, Arc<Jobs>>) -> CmdResult<()> {
    if let Some(f) = session.ftp.lock().await.take() {
        f.quit().await;
    }
    *session.info.lock().unwrap() = None;
    jobs.set_connection(None);
    Ok(())
}

#[tauri::command]
async fn scan_titles(app: AppHandle, request: ScanRequest, session: State<'_, Session>) -> CmdResult<Vec<Title>> {
    let mut guard = session.ftp.lock().await;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    let opts = ScanOptions {
        roots: request.roots.iter().filter_map(|r| ScanRoot::parse(r)).collect(),
        depth: request.depth,
    };
    let known = match (request.full, session.info.lock().unwrap().clone()) {
        (false, Some(info)) => load_scan_cache(&app, &info),
        _ => HashMap::new(),
    };
    let a = app.clone();
    let titles = scan::scan_with_cache(ftp, &opts, &known, |e| {
        let _ = a.emit("scan://event", &e);
    })
    .await
    .map_err(err)?;
    drop(guard);
    {
        let mut map = session.titles.lock().unwrap();
        map.clear();
        for t in &titles {
            map.insert(scan::cache_key(&t.path), t.clone());
        }
    }
    save_scan_cache(&app, &session);
    Ok(titles)
}

fn cover_cache(app: &AppHandle, t: &Title) -> Option<PathBuf> {
    let dir = app.path().app_cache_dir().ok()?.join("covers");
    Some(dir.join(format!("{}-{}.png", t.title_id, t.xbe_size)))
}

fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

/// The title's cover from its XBE, as a data URL. Cached on disk by title ID and XBE size, so a
/// rescan of the same console does not fetch it again.
#[tauri::command]
async fn title_image(app: AppHandle, path: String, session: State<'_, Session>) -> CmdResult<Option<String>> {
    let title = session.titles.lock().unwrap().get(&scan::cache_key(&path)).cloned().ok_or("unknown title")?;
    let cache = cover_cache(&app, &title);
    if let Some(png) = cache.as_ref().and_then(|p| std::fs::read(p).ok()) {
        return Ok(Some(data_url(&png)));
    }
    let mut guard = session.ftp.lock().await;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    match scan::title_image(ftp, &title).await {
        Ok(Some(png)) => {
            if let Some(p) = cache {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(p, &png);
            }
            Ok(Some(data_url(&png)))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            log::warn!("cover for {path}: {e}");
            Ok(None)
        }
    }
}

/// Measures a title's folder and remembers the answer in the scan cache, so a rescan of an
/// unchanged title does not measure it again.
#[tauri::command]
async fn title_size(app: AppHandle, path: String, session: State<'_, Session>) -> CmdResult<FolderSize> {
    let (bytes, files) = {
        let mut guard = session.ftp.lock().await;
        let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
        scan::folder_size(ftp, &path).await.map_err(err)?
    };
    if let Some(t) = session.titles.lock().unwrap().get_mut(&scan::cache_key(&path)) {
        t.size_bytes = Some(bytes);
        t.file_count = Some(files);
    }
    save_scan_cache(&app, &session);
    Ok(FolderSize { bytes, files })
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    settings::load(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    settings::save(&app, &settings)
}

/// Queues titles to be built into ISOs or copied as plain files, per `mode`.
#[tauri::command]
fn enqueue_jobs(
    app: AppHandle,
    jobs: State<'_, Arc<Jobs>>,
    requests: Vec<JobRequest>,
    output_dir: String,
    mode: OutputMode,
    overwrite: bool,
) -> CmdResult<Vec<QueuedJob>> {
    if output_dir.trim().is_empty() {
        return Err("choose an output folder first".into());
    }
    Ok(jobs.enqueue(&app, requests, PathBuf::from(output_dir), mode, overwrite))
}

#[tauri::command]
fn cancel_job(jobs: State<'_, Arc<Jobs>>, id: u64) {
    jobs.cancel(id);
}

#[tauri::command]
fn cancel_all_jobs(jobs: State<'_, Arc<Jobs>>) {
    jobs.cancel_all();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,suppaftp=error")).try_init();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .manage(Session::default())
        .manage(Arc::new(Jobs::default()))
        .manage(Arc::new(extract::Extracts::default()))
        .invoke_handler(tauri::generate_handler![
            discover_xboxes,
            connect,
            disconnect,
            scan_titles,
            title_image,
            title_size,
            get_settings,
            save_settings,
            enqueue_jobs,
            cancel_job,
            cancel_all_jobs,
            extract::enqueue_extracts,
            extract::cancel_extract,
            extract::sidecar_version,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
