//! The transfer queue: one job at a time, on its own FTP session, so browsing and cover loading
//! carry on while images build. ISO builds, game copies and the Files tab's uploads and
//! downloads all wait in the same line: an Xbox has 100 Mbit Ethernet, so a second concurrent
//! transfer would only split the same bandwidth.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use xib_core::ftpfs::{Cancel, IsoProgress, ProgressFn};
use xib_core::{copy, iso, transfer, ConnectionInfo, Error, XboxFtp};

pub const EVENT: &str = "job://update";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobUpdate {
    pub id: u64,
    pub state: JobState,
    pub progress: Option<IsoProgress>,
    /// Average rate since the job started copying, bytes per second.
    pub rate: Option<f64>,
    pub message: Option<String>,
    pub out_path: Option<String>,
}

impl JobUpdate {
    fn state(id: u64, state: JobState) -> Self {
        Self { id, state, progress: None, rate: None, message: None, out_path: None }
    }
}

/// What a job produces from a game folder.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OutputMode {
    /// An XISO image.
    #[default]
    Iso,
    /// The folder's files as they are on the Xbox (what xboxrecomp reads).
    Files,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    /// Remote game folder.
    pub path: String,
    pub iso_name: String,
}

/// A Files tab transfer.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TransferRequest {
    /// Sends a local file or folder into the remote folder `remote_dir`.
    #[serde(rename_all = "camelCase")]
    Upload { local: String, remote_dir: String },
    /// Fetches a remote file or folder into the local folder `local_dir`.
    #[serde(rename_all = "camelCase")]
    Download { remote: String, is_dir: bool, size: u64, local_dir: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedJob {
    pub id: u64,
    pub path: String,
    pub out_path: String,
}

enum Work {
    /// Builds an ISO from a game folder, or copies it as files.
    Title { remote: String, out: PathBuf, mode: OutputMode },
    Upload { local: PathBuf, remote_dir: String },
    Download { remote: String, is_dir: bool, size: u64, out: PathBuf },
}

struct Job {
    id: u64,
    work: Work,
    overwrite: bool,
}

impl Job {
    /// Where the job writes, as shown to the user: a local path, or a remote one for uploads.
    fn out_path(&self) -> String {
        match &self.work {
            Work::Title { out, .. } | Work::Download { out, .. } => out.to_string_lossy().into_owned(),
            Work::Upload { local, remote_dir } => {
                xib_core::ftp::join(remote_dir, &local.file_name().unwrap_or_default().to_string_lossy())
            }
        }
    }
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Job>,
    cancels: HashMap<u64, Cancel>,
    worker: bool,
    next_id: u64,
}

#[derive(Default)]
pub struct Jobs {
    inner: Mutex<Inner>,
    conn: Mutex<Option<ConnectionInfo>>,
}

impl Jobs {
    pub fn set_connection(&self, info: Option<ConnectionInfo>) {
        *self.conn.lock().unwrap() = info;
    }

    pub fn enqueue(
        self: &Arc<Self>,
        app: &AppHandle,
        reqs: Vec<JobRequest>,
        output_dir: PathBuf,
        mode: OutputMode,
        overwrite: bool,
    ) -> Vec<QueuedJob> {
        let work = reqs.into_iter().map(|r| {
            let name = match mode {
                OutputMode::Iso => r.iso_name.clone(),
                OutputMode::Files => copy::default_folder_name(&r.iso_name),
            };
            (r.path.clone(), Work::Title { remote: r.path, out: output_dir.join(name), mode })
        });
        self.push(app, work.collect(), overwrite)
    }

    pub fn enqueue_transfers(self: &Arc<Self>, app: &AppHandle, reqs: Vec<TransferRequest>, overwrite: bool) -> Vec<QueuedJob> {
        let work = reqs.into_iter().map(|r| match r {
            TransferRequest::Upload { local, remote_dir } => {
                (local.clone(), Work::Upload { local: PathBuf::from(local), remote_dir })
            }
            TransferRequest::Download { remote, is_dir, size, local_dir } => {
                let name = xib_core::ftp::split(&remote).1.to_string();
                let out = PathBuf::from(local_dir).join(name);
                (remote.clone(), Work::Download { remote, is_dir, size, out })
            }
        });
        self.push(app, work.collect(), overwrite)
    }

    /// Queues `work`, each item with the path the UI knows it by, and starts the worker if it
    /// is not running.
    fn push(self: &Arc<Self>, app: &AppHandle, work: Vec<(String, Work)>, overwrite: bool) -> Vec<QueuedJob> {
        let mut out = Vec::new();
        let mut spawn = false;
        {
            let mut inner = self.inner.lock().unwrap();
            for (path, work) in work {
                inner.next_id += 1;
                let id = inner.next_id;
                let job = Job { id, work, overwrite };
                inner.cancels.insert(id, Cancel::new());
                out.push(QueuedJob { id, path, out_path: job.out_path() });
                inner.queue.push_back(job);
            }
            if !inner.worker && !inner.queue.is_empty() {
                inner.worker = true;
                spawn = true;
            }
        }
        for q in &out {
            let _ = app.emit(EVENT, JobUpdate::state(q.id, JobState::Queued));
        }
        if spawn {
            let me = self.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move { me.run(app).await });
        }
        out
    }

    pub fn cancel(&self, id: u64) {
        if let Some(c) = self.inner.lock().unwrap().cancels.get(&id) {
            c.cancel();
        }
    }

    async fn run(self: Arc<Self>, app: AppHandle) {
        let mut ftp: Option<XboxFtp> = None;
        loop {
            let (job, cancel) = {
                let mut inner = self.inner.lock().unwrap();
                match inner.queue.pop_front() {
                    Some(j) => {
                        let c = inner.cancels.get(&j.id).cloned().unwrap_or_default();
                        (j, c)
                    }
                    None => {
                        inner.worker = false;
                        break;
                    }
                }
            };
            let update = self.run_one(&app, &job, cancel, &mut ftp).await;
            self.inner.lock().unwrap().cancels.remove(&job.id);
            let _ = app.emit(EVENT, update);
        }
        if let Some(f) = ftp {
            f.quit().await;
        }
    }

    async fn run_one(&self, app: &AppHandle, job: &Job, cancel: Cancel, ftp: &mut Option<XboxFtp>) -> JobUpdate {
        let mut done = JobUpdate::state(job.id, JobState::Done);
        done.out_path = Some(job.out_path());

        if cancel.is_cancelled() {
            done.state = JobState::Cancelled;
            return done;
        }
        // An upload replaces what is on the Xbox; the Files tab asks before queueing one.
        let exists = match &job.work {
            Work::Title { out, mode: OutputMode::Iso, .. } => out.exists().then_some("an ISO with this name already exists"),
            Work::Title { out, mode: OutputMode::Files, .. } => std::fs::read_dir(out)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
                .then_some("a folder with this name already exists"),
            Work::Download { out, .. } => out.exists().then_some("something with this name is already there"),
            Work::Upload { .. } => None,
        };
        if let Some(why) = exists.filter(|_| !job.overwrite) {
            done.state = JobState::Skipped;
            done.message = Some(why.into());
            return done;
        }

        if ftp.is_none() {
            let info = self.conn.lock().unwrap().clone();
            let Some(info) = info else {
                done.state = JobState::Failed;
                done.message = Some("not connected to an Xbox".into());
                return done;
            };
            match XboxFtp::connect(&info).await {
                Ok(f) => *ftp = Some(f),
                Err(e) => {
                    done.state = JobState::Failed;
                    done.message = Some(e.to_string());
                    return done;
                }
            }
        }
        let session = ftp.as_mut().unwrap();

        let _ = app.emit(EVENT, JobUpdate::state(job.id, JobState::Running));
        let id = job.id;
        let a = app.clone();
        let started = Instant::now();
        let last = Mutex::new(Instant::now() - Duration::from_secs(1));
        let progress: ProgressFn = Arc::new(move |p: IsoProgress| {
            let urgent = !matches!(p, IsoProgress::Copying { .. } | IsoProgress::Listing { .. });
            {
                let mut l = last.lock().unwrap();
                if !urgent && l.elapsed() < Duration::from_millis(150) {
                    return;
                }
                *l = Instant::now();
            }
            let rate = match &p {
                IsoProgress::Copying { done, .. } => {
                    Some(*done as f64 / started.elapsed().as_secs_f64().max(0.001))
                }
                _ => None,
            };
            let _ = a.emit(
                EVENT,
                JobUpdate { id, state: JobState::Running, progress: Some(p), rate, message: None, out_path: None },
            );
        });

        let result = match &job.work {
            Work::Title { remote, out, mode: OutputMode::Iso } => iso::create_iso(session, remote, out, progress, cancel).await,
            Work::Title { remote, out, mode: OutputMode::Files } => {
                copy::copy_folder(session, remote, out, progress, cancel).await
            }
            Work::Upload { local, remote_dir } => transfer::upload(session, local, remote_dir, progress, cancel).await,
            Work::Download { remote, is_dir, size, out } => {
                transfer::download(session, remote, *is_dir, *size, out, progress, cancel).await
            }
        };
        match result {
            Ok(bytes) => {
                done.progress = Some(IsoProgress::Done { bytes });
                done
            }
            Err(e) => {
                // A failed or cancelled transfer may leave the session mid-reply; start fresh.
                if let Some(f) = ftp.take() {
                    f.quit().await;
                }
                done.out_path = None;
                done.state = if matches!(e, Error::Cancelled) { JobState::Cancelled } else { JobState::Failed };
                done.message = Some(e.to_string());
                done
            }
        }
    }
}
