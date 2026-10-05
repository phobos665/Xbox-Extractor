//! The ISO queue: one job at a time, on its own FTP session, so browsing and cover loading
//! carry on while images build. An Xbox has 100 Mbit Ethernet, so a second concurrent
//! transfer would only split the same bandwidth.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use xib_core::ftpfs::{Cancel, IsoProgress, ProgressFn};
use xib_core::{copy, iso, ConnectionInfo, Error, XboxFtp};

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedJob {
    pub id: u64,
    pub path: String,
    pub out_path: String,
}

struct Job {
    id: u64,
    remote: String,
    out: PathBuf,
    mode: OutputMode,
    overwrite: bool,
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
        let mut out = Vec::new();
        let mut spawn = false;
        {
            let mut inner = self.inner.lock().unwrap();
            for r in reqs {
                inner.next_id += 1;
                let id = inner.next_id;
                let name = match mode {
                    OutputMode::Iso => r.iso_name.clone(),
                    OutputMode::Files => copy::default_folder_name(&r.iso_name),
                };
                let path = output_dir.join(name);
                inner.cancels.insert(id, Cancel::new());
                out.push(QueuedJob { id, path: r.path.clone(), out_path: path.to_string_lossy().into_owned() });
                inner.queue.push_back(Job { id, remote: r.path, out: path, mode, overwrite });
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

    pub fn cancel_all(&self) {
        for c in self.inner.lock().unwrap().cancels.values() {
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
        done.out_path = Some(job.out.to_string_lossy().into_owned());

        if cancel.is_cancelled() {
            done.state = JobState::Cancelled;
            return done;
        }
        let exists = match job.mode {
            OutputMode::Iso => job.out.exists(),
            OutputMode::Files => std::fs::read_dir(&job.out).map(|mut d| d.next().is_some()).unwrap_or(false),
        };
        if exists && !job.overwrite {
            done.state = JobState::Skipped;
            done.message = Some(match job.mode {
                OutputMode::Iso => "an ISO with this name already exists".into(),
                OutputMode::Files => "a folder with this name already exists".into(),
            });
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

        let result = match job.mode {
            OutputMode::Iso => iso::create_iso(session, &job.remote, &job.out, progress, cancel).await,
            OutputMode::Files => copy::copy_folder(session, &job.remote, &job.out, progress, cancel).await,
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
