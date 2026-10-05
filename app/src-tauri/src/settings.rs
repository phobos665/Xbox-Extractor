//! Settings kept between runs, as JSON in the app's config folder.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use xib_core::ConnectionInfo;

use crate::jobs::OutputMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub connection: ConnectionInfo,
    /// Hosts connected to before, most recent first.
    pub recent_hosts: Vec<String>,
    pub output_dir: String,
    /// Build ISOs, or copy each game's files as they are.
    pub output_mode: OutputMode,
    /// Where to look for titles, e.g. `F:`, `E:/Games`.
    pub roots: Vec<String>,
    pub depth: u32,
    /// Replace an ISO that already exists instead of skipping the title.
    pub overwrite: bool,
    /// Where the Extract tab unpacks ISOs.
    pub extract_dir: String,
    /// Leave out the `$SystemUpdate` folder when extracting (extract-xiso `-s`).
    pub skip_system_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            connection: ConnectionInfo::default(),
            recent_hosts: Vec::new(),
            output_dir: String::new(),
            output_mode: OutputMode::Iso,
            roots: vec!["F:".into(), "E:/Games".into(), "G:".into()],
            depth: 2,
            overwrite: false,
            extract_dir: String::new(),
            skip_system_update: true,
        }
    }
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    let mut s: Settings = path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if s.output_dir.is_empty() {
        let base = app
            .path()
            .download_dir()
            .or_else(|_| app.path().home_dir())
            .unwrap_or_else(|_| PathBuf::from("."));
        s.output_dir = base.join("Xbox ISOs").to_string_lossy().into_owned();
    }
    if s.extract_dir.is_empty() {
        let base = app.path().download_dir().or_else(|_| app.path().home_dir()).unwrap_or_else(|_| PathBuf::from("."));
        s.extract_dir = base.join("Xbox Extracted").to_string_lossy().into_owned();
    }
    s
}

pub fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let p = path(app).ok_or("no config folder on this system")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(p, json).map_err(|e| e.to_string())
}
