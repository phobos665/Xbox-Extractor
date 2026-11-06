//! The Files tab: browsing and changing the Xbox's drives over a session of its own. Uploads
//! and downloads go through the job queue instead (`enqueue_transfers`).

use std::sync::Arc;

use tokio::sync::MutexGuard;
use xib_core::XboxFtp;

use serde::Deserialize;
use tauri::{AppHandle, State};
use xib_core::ftp::{join, split};
use xib_core::{transfer, RemoteEntry};

use crate::jobs::{Jobs, QueuedJob, TransferRequest};
use crate::{err, CmdResult, Session};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRef {
    path: String,
    is_dir: bool,
}

/// The Files tab's own session, opened on first use and after a reconnect to the Xbox.
async fn files_session(session: &Session) -> CmdResult<MutexGuard<'_, Option<XboxFtp>>> {
    let mut guard = session.files.lock().await;
    if guard.is_none() {
        let info = session.info.lock().unwrap().clone().ok_or("not connected to an Xbox")?;
        *guard = Some(XboxFtp::connect_patiently(&info, XboxFtp::DEFAULT_TIMEOUT).await.map_err(err)?);
    }
    Ok(guard)
}

#[tauri::command]
pub async fn browse(path: String, session: State<'_, Session>) -> CmdResult<Vec<RemoteEntry>> {
    let mut guard = files_session(&session).await?;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    ftp.list_dir(&path).await.map_err(err)
}

#[tauri::command]
pub async fn make_dir(path: String, session: State<'_, Session>) -> CmdResult<()> {
    transfer::check_name(split(&path).1).map_err(err)?;
    let mut guard = files_session(&session).await?;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    ftp.mkdir(&path).await.map_err(err)
}

/// Renames within the same folder; returns the new path.
#[tauri::command]
pub async fn rename_entry(path: String, new_name: String, session: State<'_, Session>) -> CmdResult<String> {
    transfer::check_name(&new_name).map_err(err)?;
    let mut guard = files_session(&session).await?;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    ftp.rename(&path, &new_name).await.map_err(err)?;
    Ok(join(split(&path).0, &new_name))
}

/// Deletes each entry, folders with their contents. Stops at the first failure, leaving the
/// entries before it deleted.
#[tauri::command]
pub async fn delete_entries(entries: Vec<EntryRef>, session: State<'_, Session>) -> CmdResult<()> {
    let mut guard = files_session(&session).await?;
    let ftp = guard.as_mut().ok_or("not connected to an Xbox")?;
    for e in entries {
        transfer::remove(ftp, &e.path, e.is_dir).await.map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
pub fn enqueue_transfers(
    app: AppHandle,
    jobs: State<'_, Arc<Jobs>>,
    requests: Vec<TransferRequest>,
    overwrite: bool,
) -> CmdResult<Vec<QueuedJob>> {
    Ok(jobs.enqueue_transfers(&app, requests, overwrite))
}
