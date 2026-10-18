use crate::model::*;
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::{SinkExt, StreamExt};
use rusqlite::{Connection, OptionalExtension, params};
#[cfg(dashboard_assets)]
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, sync::mpsc};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

type Shared = Arc<Mutex<Store>>;
type ApiResult<T> = std::result::Result<T, ApiError>;

#[cfg(dashboard_assets)]
#[derive(RustEmbed)]
#[folder = "dashboard/dist/"]
struct DashboardAssets;

#[cfg(not(dashboard_assets))]
struct DashboardAssets;

#[derive(Debug)]
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        eprintln!("hub: {error:#}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "hub operation failed".into(),
        )
    }
}
fn invalid(error: impl ToString) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
}
fn missing(kind: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{kind} not found"))
}
fn lock(state: &Shared) -> ApiResult<MutexGuard<'_, Store>> {
    state.lock().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "hub store unavailable".into(),
        )
    })
}

struct Session {
    id: Uuid,
    tx: mpsc::UnboundedSender<HubMessage>,
}
struct Store {
    db: Connection,
    blobs: PathBuf,
    token: String,
    sessions: HashMap<String, Session>,
}
#[derive(Default, Serialize, Deserialize)]
struct WorkerSettings {
    paused: bool,
    max_jobs: Option<usize>,
    capacity: usize,
}

fn identifier(value: &str) -> Result<()> {
    safe_relative(value)?;
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        bail!(
            "identifier must start with a letter or digit and contain only letters, digits, _, . or - (maximum 128 characters)"
        )
    }
    Ok(())
}
fn image_ref(image: &str) -> Result<()> {
    if image.is_empty()
        || image.len() > 2048
        || image.starts_with('-')
        || !image.bytes().all(|b| b.is_ascii_graphic())
    {
        bail!("invalid container image reference")
    }
    Ok(())
}
fn terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled")
}
fn active(state: &str) -> bool {
    matches!(state, "assigned" | "running" | "cancelling" | "unknown")
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

impl Store {
    fn open(data: PathBuf, token: String) -> Result<Self> {
        crate::worker::private_dir(&data)?;
        crate::worker::private_dir(&data.join("blobs"))?;
        let db = Connection::open(data.join("hub.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, data TEXT NOT NULL, environment TEXT, cancel_requested INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS workers (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS worker_settings (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS environments (id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS environment_revisions (id TEXT NOT NULL, revision TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(id, revision));
            CREATE TABLE IF NOT EXISTS environment_status (worker TEXT NOT NULL, environment TEXT NOT NULL, revision TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(worker, environment, revision));
            CREATE TABLE IF NOT EXISTS logs (job TEXT NOT NULL, attempt TEXT NOT NULL, seq INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(job, attempt, seq));")?;
        let mut store = Self {
            db,
            blobs: data.join("blobs"),
            token,
            sessions: HashMap::new(),
        };
        for mut worker in store.workers()? {
            worker.online = false;
            let settings = store.worker_settings(&worker)?;
            worker.paused = settings.paused;
            worker.max_jobs = settings
                .max_jobs
                .unwrap_or(settings.capacity)
                .min(settings.capacity);
            store.save_worker(&worker)?;
        }
        for mut job in store.jobs()? {
            if active(&job.state) {
                job.state = "unknown".into();
                job.error = Some(
                    "hub restarted; awaiting worker reconciliation, no automatic retry".into(),
                );
                job.updated_at = now();
                store.save_job(&job)?;
            }
        }
        for mut status in store.statuses()? {
            if status.state == "preparing" {
                status.state = "unknown".into();
                status.error = Some(
                    "hub restarted during preparation; reconnect or explicitly prepare again"
                        .into(),
                );
                store.save_status(&status)?;
            }
        }
        Ok(store)
    }
    fn rows<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        let mut statement = self.db.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    fn one<T: DeserializeOwned>(&self, sql: &str, id: &str) -> Result<Option<T>> {
        let value: Option<String> = self.db.query_row(sql, [id], |r| r.get(0)).optional()?;
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn workers(&self) -> Result<Vec<WorkerInfo>> {
        self.rows("SELECT data FROM workers ORDER BY id")
    }
    fn jobs(&self) -> Result<Vec<Job>> {
        self.rows("SELECT data FROM jobs ORDER BY rowid")
    }
    fn environments(&self) -> Result<Vec<Environment>> {
        self.rows("SELECT data FROM environments ORDER BY id")
    }
    fn statuses(&self) -> Result<Vec<EnvironmentStatus>> {
        self.rows("SELECT data FROM environment_status ORDER BY worker, environment, revision")
    }
    fn job(&self, id: &str) -> Result<Option<Job>> {
        self.one("SELECT data FROM jobs WHERE id=?1", id)
    }
    fn environment(&self, id: &str) -> Result<Option<Environment>> {
        self.one("SELECT data FROM environments WHERE id=?1", id)
    }
    fn job_environment(&self, id: &str) -> Result<Option<Environment>> {
        let value: Option<String> =
            self.db
                .query_row("SELECT environment FROM jobs WHERE id=?1", [id], |row| {
                    row.get(0)
                })?;
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn save_job(&mut self, job: &Job) -> Result<()> {
        self.db.execute(
            "UPDATE jobs SET data=?2 WHERE id=?1",
            params![job.id, encode(job)?],
        )?;
        Ok(())
    }
    fn save_worker(&mut self, worker: &WorkerInfo) -> Result<()> {
        self.db.execute("INSERT INTO workers(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![worker.id, encode(worker)?])?;
        Ok(())
    }
    fn delete_worker(&mut self, id: &str) -> Result<()> {
        self.db.execute("DELETE FROM worker_settings WHERE id=?1", [id])?;
        self.db.execute("DELETE FROM workers WHERE id=?1", [id])?;
        Ok(())
    }
    /// Removes an environment with its revisions, worker statuses and preparation logs.
    /// Deleting a missing id succeeds so retries are idempotent; jobs that still use it block deletion.
    fn delete_environment(&mut self, id: &str) -> Result<(), ApiError> {
        if self.jobs()?.iter().any(|job| {
            matches!(job.state.as_str(), "queued" | "assigned" | "running" | "cancelling")
                && job.spec.environment.as_deref() == Some(id)
        }) {
            return Err(invalid("environment is used by queued or active jobs; wait or cancel them first"));
        }
        let tx = self.db.transaction().map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM environments WHERE id=?1", [id]).map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM environment_revisions WHERE id=?1", [id]).map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM environment_status WHERE environment=?1", [id]).map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM logs WHERE substr(job, 1, length(?1)) = ?1", [format!("environment:{id}:")]).map_err(anyhow::Error::from)?;
        tx.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn worker_settings(&self, worker: &WorkerInfo) -> Result<WorkerSettings> {
        Ok(self
            .one("SELECT data FROM worker_settings WHERE id=?1", &worker.id)?
            .unwrap_or(WorkerSettings {
                paused: worker.paused,
                capacity: worker.max_jobs,
                ..Default::default()
            }))
    }
    fn save_worker_settings(&mut self, id: &str, settings: &WorkerSettings) -> Result<()> {
        self.db.execute("INSERT INTO worker_settings(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![id, encode(settings)?])?;
        Ok(())
    }
    fn control_worker(&mut self, id: &str, control: WorkerControl) -> ApiResult<WorkerInfo> {
        let mut worker = self
            .one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", id)?
            .ok_or_else(|| missing("worker"))?;
        if control.paused.is_none() && control.max_jobs.is_none() {
            return Err(invalid("specify paused or max_jobs"));
        }
        let mut settings = self.worker_settings(&worker)?;
        if let Some(limit) = control.max_jobs {
            if !(1..=256).contains(&limit) {
                return Err(invalid("max_jobs must be between 1 and 256"));
            }
            if limit > settings.capacity {
                return Err(invalid(format!(
                    "max_jobs cannot exceed worker's advertised capacity of {}; restart the worker with a higher --max-jobs first",
                    settings.capacity
                )));
            }
            settings.max_jobs = Some(limit);
        }
        if let Some(paused) = control.paused {
            settings.paused = paused;
        }
        worker.paused = settings.paused;
        worker.max_jobs = settings
            .max_jobs
            .unwrap_or(settings.capacity)
            .min(settings.capacity);
        self.save_worker_settings(id, &settings)?;
        self.save_worker(&worker)?;
        self.schedule()?;
        Ok(worker)
    }
    fn set_priority(&mut self, id: &str, priority: i32) -> ApiResult<Job> {
        validate_priority(priority).map_err(invalid)?;
        let mut job = self.job(id)?.ok_or_else(|| missing("job"))?;
        if job.state != "queued" {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "priority can only be changed for a queued job".into(),
            ));
        }
        job.spec.priority = priority;
        job.updated_at = now();
        self.save_job(&job)?;
        self.schedule()?;
        Ok(self.job(id)?.expect("saved job"))
    }
    fn save_status(&mut self, status: &EnvironmentStatus) -> Result<()> {
        self.db.execute("INSERT INTO environment_status(worker,environment,revision,data) VALUES(?1,?2,?3,?4) ON CONFLICT(worker,environment,revision) DO UPDATE SET data=excluded.data", params![status.worker_id, status.environment_id, status.revision, encode(status)?])?;
        Ok(())
    }
    fn cancel_requested(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT cancel_requested FROM jobs WHERE id=?1",
            [id],
            |row| row.get(0),
        )?)
    }
    fn status(&self, worker: &str, environment: &Environment) -> Result<Option<EnvironmentStatus>> {
        let value: Option<String> = self.db.query_row("SELECT data FROM environment_status WHERE worker=?1 AND environment=?2 AND revision=?3", params![worker, environment.id, environment.revision], |row| row.get(0)).optional()?;
        value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn validate_files(&self, entries: &[FileEntry]) -> Result<()> {
        let mut paths = BTreeSet::new();
        for file in entries {
            safe_relative(&file.path)?;
            validate_hash(&file.hash)?;
            if !paths.insert(file.path.as_str()) {
                bail!("duplicate file path: {}", file.path)
            }
            let metadata = std::fs::metadata(self.blobs.join(&file.hash))
                .with_context(|| format!("blob missing for {}", file.path))?;
            if !metadata.is_file() || metadata.len() != file.size {
                bail!("blob size mismatch for {}", file.path)
            }
        }
        for path in &paths {
            let mut ancestor = path.rsplit_once('/').map(|(parent, _)| parent);
            while let Some(parent) = ancestor {
                if paths.contains(parent) {
                    bail!("input file also used as a directory: {parent}")
                }
                ancestor = parent.rsplit_once('/').map(|(ancestor, _)| ancestor);
            }
        }
        Ok(())
    }
    fn add_job(&mut self, spec: JobSpec) -> ApiResult<Job> {
        spec.validate().map_err(invalid)?;
        self.validate_files(&spec.inputs).map_err(invalid)?;
        if let Some(image) = &spec.container {
            image_ref(image).map_err(invalid)?;
        }
        let environment = spec
            .environment
            .as_deref()
            .map(|id| {
                identifier(id).map_err(invalid)?;
                self.environment(id)?.ok_or_else(|| missing("environment"))
            })
            .transpose()?;
        if let (Some(image), Some(env_image)) = (
            &spec.container,
            environment.as_ref().and_then(|e| e.image.as_ref()),
        ) && image != env_image
        {
            return Err(invalid(
                "job container differs from pinned environment image",
            ));
        }
        let workers = self.workers()?;
        let reasons = incompatibilities(&workers, &spec, environment.as_ref());
        if workers.is_empty() || reasons.len() == workers.len() {
            return Err(invalid(format!(
                "no compatible registered worker: {}",
                if workers.is_empty() {
                    "register a worker first".into()
                } else {
                    reasons.join("; ")
                }
            )));
        }
        let job = Job {
            id: Uuid::new_v4().to_string(),
            attempt_id: Uuid::new_v4().to_string(),
            state: "queued".into(),
            spec,
            worker_id: None,
            created_at: now(),
            updated_at: now(),
            exit_code: None,
            error: None,
            results: vec![],
        };
        self.db
            .execute(
                "INSERT INTO jobs(id,data,environment) VALUES(?1,?2,?3)",
                params![
                    job.id,
                    encode(&job)?,
                    environment.as_ref().map(encode).transpose()?
                ],
            )
            .map_err(anyhow::Error::from)?;
        self.schedule()?;
        Ok(self.job(&job.id)?.expect("inserted job"))
    }
    fn schedule(&mut self) -> Result<()> {
        let workers = self.workers()?;
        let mut jobs = self.jobs()?;
        // Stable ordering retains the database creation order for equal priorities.
        jobs.sort_by_key(|job| std::cmp::Reverse(job.spec.priority));
        for job in &mut jobs {
            if job.state == "queued" && self.cancel_requested(&job.id)? {
                job.state = "cancelled".into();
                job.updated_at = now();
                self.save_job(job)?;
            }
        }
        for worker in workers
            .iter()
            .filter(|worker| worker.online && !worker.paused)
        {
            let Some(tx) = self.sessions.get(&worker.id).map(|s| s.tx.clone()) else {
                continue;
            };
            let mut occupied = jobs
                .iter()
                .filter(|job| job.worker_id.as_deref() == Some(&worker.id) && active(&job.state))
                .count();
            let statuses = self.statuses()?;
            if statuses
                .iter()
                .any(|status| status.worker_id == worker.id && status.state == "preparing")
            {
                continue;
            }
            let pending = statuses
                .iter()
                .find(|status| status.worker_id == worker.id && status.state == "pending");
            if let Some(status) = pending {
                if occupied == 0
                    && let Some(environment) =
                        self.environment_revision(&status.environment_id, &status.revision)?
                {
                    let mut status = status.clone();
                    status.state = "preparing".into();
                    self.save_status(&status)?;
                    // Persist before delivery: an interrupted setup is never silently rerun.
                    if tx.send(HubMessage::Prepare { environment }).is_err() {
                        status.state = "unknown".into();
                        status.error = Some(
                            "worker connection lost before preparation acknowledgement".into(),
                        );
                        self.save_status(&status)?;
                    }
                }
                continue;
            }
            for job in &mut jobs {
                if occupied >= worker.max_jobs || job.state != "queued" {
                    continue;
                }
                let environment = self.job_environment(&job.id)?;
                if !candidate(worker, &job.spec, environment.as_ref()).is_empty() {
                    continue;
                }
                if let Some(environment) = &environment {
                    match self.status(&worker.id, environment)? {
                        Some(status) if status.state == "ready" => {}
                        Some(status) if matches!(status.state.as_str(), "failed" | "unknown") => {
                            continue;
                        }
                        _ => {
                            self.save_status(&EnvironmentStatus {
                                worker_id: worker.id.clone(),
                                environment_id: environment.id.clone(),
                                revision: environment.revision.clone(),
                                state: "pending".into(),
                                error: None,
                            })?;
                            if occupied == 0 {
                                let mut status = self
                                    .status(&worker.id, environment)?
                                    .expect("pending status");
                                status.state = "preparing".into();
                                self.save_status(&status)?;
                                if tx
                                    .send(HubMessage::Prepare {
                                        environment: environment.clone(),
                                    })
                                    .is_err()
                                {
                                    status.state = "unknown".into();
                                    status.error = Some(
                                        "worker connection lost before preparation acknowledgement"
                                            .into(),
                                    );
                                    self.save_status(&status)?;
                                }
                            }
                            break;
                        }
                    }
                }
                job.state = "assigned".into();
                job.worker_id = Some(worker.id.clone());
                job.updated_at = now();
                self.save_job(job)?;
                // A lost delivery remains an unknown attempt; it is not queued on another host.
                if tx
                    .send(HubMessage::Run {
                        job: Box::new(job.clone()),
                        environment,
                    })
                    .is_err()
                {
                    job.state = "unknown".into();
                    job.error = Some("worker connection lost during assignment".into());
                    self.save_job(job)?;
                }
                occupied += 1;
            }
        }
        // Failed setup is actionable; only an explicit prepare call can retry that revision.
        for mut job in jobs.into_iter().filter(|job| job.state == "queued") {
            let Some(environment) = self.job_environment(&job.id)? else {
                continue;
            };
            let candidates: Vec<_> = workers
                .iter()
                .filter(|w| candidate(w, &job.spec, Some(&environment)).is_empty())
                .collect();
            if !candidates.is_empty()
                && candidates.iter().all(|w| {
                    self.status(&w.id, &environment)
                        .ok()
                        .flatten()
                        .is_some_and(|s| s.state == "failed")
                })
            {
                job.state = "failed".into();
                job.error = Some(format!(
                    "environment {} revision {} preparation failed; inspect status and explicitly prepare to retry",
                    environment.id, environment.revision
                ));
                job.updated_at = now();
                self.save_job(&job)?;
            }
        }
        Ok(())
    }
    fn environment_revision(&self, id: &str, revision: &str) -> Result<Option<Environment>> {
        // A deleted id has no catalogue entry and no job can use it again, so stale worker statuses
        // and replayed logs are rejected here before the job-history scan below.
        if self.environment(id)?.is_none() {
            return Ok(None);
        }
        let stored: Option<String> = self
            .db
            .query_row(
                "SELECT data FROM environment_revisions WHERE id=?1 AND revision=?2",
                params![id, revision],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored) = stored {
            return Ok(Some(serde_json::from_str(&stored)?));
        }
        if let Some(environment) = self.environment(id)?
            && environment.revision == revision
        {
            return Ok(Some(environment));
        }
        let mut statement = self.db.prepare(
            "SELECT environment FROM jobs WHERE environment IS NOT NULL ORDER BY rowid DESC",
        )?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            let environment: Environment = serde_json::from_str(&row?)?;
            if environment.id == id && environment.revision == revision {
                return Ok(Some(environment));
            }
        }
        Ok(None)
    }
    fn assigned(&self, worker: &str, job_id: &str, attempt: &str) -> Result<Job> {
        let job = self.job(job_id)?.context("unknown job")?;
        if job.worker_id.as_deref() != Some(worker) || job.attempt_id != attempt {
            bail!("worker/attempt does not own job")
        }
        Ok(job)
    }
    fn report(&mut self, worker: &str, report: AttemptReport) -> Result<()> {
        let mut job = self.assigned(worker, &report.job_id, &report.attempt_id)?;
        if terminal(&job.state) {
            return Ok(());
        }
        if !active(&job.state) {
            bail!("job is not assigned")
        }
        if report.state == "unknown" {
            job.state = "unknown".into();
            job.error = report.error.or_else(|| {
                Some("worker cannot establish the attempt outcome; no automatic retry".into())
            });
        } else if matches!(report.state.as_str(), "running" | "assigned" | "cancelling") {
            job.state = if self.cancel_requested(&job.id)? {
                "cancelling"
            } else {
                "running"
            }
            .into();
            job.error = None;
        } else if terminal(&report.state) {
            job.state = report.state;
            job.exit_code = report.exit_code;
            job.error = report.error;
            if job.state == "succeeded" && job.exit_code != Some(0) {
                job.state = "failed".into();
                job.error = Some("worker reported success without exit code 0".into());
            }
            match self.validate_files(&report.results) {
                Ok(()) => {
                    if report.results.iter().all(|file| {
                        job.spec.outputs.iter().any(|output| {
                            file.path == *output || file.path.starts_with(&format!("{output}/"))
                        })
                    }) {
                        job.results = report.results;
                    } else {
                        job.state = "failed".into();
                        job.error =
                            Some("worker returned a result outside declared outputs".into());
                        job.results.clear();
                    }
                }
                Err(error) => {
                    job.state = "failed".into();
                    job.error = Some(format!("invalid result manifest: {error:#}"));
                    job.results.clear();
                }
            }
        } else {
            bail!("unsupported attempt report state")
        }
        job.updated_at = now();
        self.save_job(&job)?;
        Ok(())
    }
    fn receive_status(&mut self, worker: &str, status: EnvironmentStatus) -> Result<()> {
        if status.worker_id != worker {
            bail!("environment status belongs to a different worker")
        }
        identifier(&status.environment_id)?;
        identifier(&status.revision)?;
        if status.state == "evicted" {
            // The worker reclaimed the revision's disk space; the next job prepares it again.
            self.db.execute(
                "DELETE FROM environment_status WHERE worker=?1 AND environment=?2 AND revision=?3",
                params![worker, status.environment_id, status.revision],
            )?;
            return Ok(());
        }
        if !matches!(status.state.as_str(), "ready" | "failed" | "preparing") {
            bail!("invalid worker environment state")
        }
        if self
            .environment_revision(&status.environment_id, &status.revision)?
            .is_none()
        {
            bail!("unknown environment revision")
        }
        self.save_status(&status)
    }
    #[allow(clippy::too_many_arguments)]
    fn connect(
        &mut self,
        id: String,
        mut capabilities: Capabilities,
        max_jobs: usize,
        attempts: Vec<AttemptReport>,
        environments: Vec<EnvironmentStatus>,
        version: String,
        tx: mpsc::UnboundedSender<HubMessage>,
    ) -> Result<Uuid> {
        identifier(&id)?;
        if max_jobs == 0 || max_jobs > 256 {
            bail!("max_jobs must be between 1 and 256")
        }
        if !matches!(capabilities.os.as_str(), "linux" | "macos" | "windows") {
            bail!("unsupported worker OS")
        }
        capabilities.arch = normalize_arch(&capabilities.arch);
        if !matches!(capabilities.arch.as_str(), "x86_64" | "aarch64") {
            bail!("unsupported worker architecture")
        }
        let session = Uuid::new_v4();
        let previous = self.one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", &id)?;
        let mut settings = match previous {
            Some(worker) => self.worker_settings(&worker)?,
            None => WorkerSettings::default(),
        };
        settings.capacity = max_jobs;
        self.save_worker_settings(&id, &settings)?;
        self.sessions
            .insert(id.clone(), Session { id: session, tx });
        self.save_worker(&WorkerInfo {
            id: id.clone(),
            capabilities,
            max_jobs: settings.max_jobs.unwrap_or(max_jobs).min(max_jobs),
            paused: settings.paused,
            online: true,
            last_seen: now(),
            version,
            update_error: None,
        })?;
        let reported: BTreeSet<_> = attempts
            .iter()
            .map(|report| (report.job_id.clone(), report.attempt_id.clone()))
            .collect();
        for report in attempts {
            if let Err(error) = self.report(&id, report) {
                eprintln!("ignored worker reconciliation report: {error:#}");
            }
        }
        for mut job in self.jobs()? {
            if job.worker_id.as_deref() == Some(&id) && active(&job.state) {
                if !reported.contains(&(job.id.clone(), job.attempt_id.clone())) {
                    job.state = "unknown".into();
                    job.updated_at = now();
                    job.error = Some(
                        "worker did not report this attempt; cancel explicitly before retrying"
                            .into(),
                    );
                    self.save_job(&job)?;
                }
                if self.cancel_requested(&job.id)? {
                    let _ = self.sessions[&id].tx.send(HubMessage::Cancel {
                        job_id: job.id.clone(),
                        attempt_id: job.attempt_id.clone(),
                    });
                }
            }
        }
        for status in environments {
            if let Some(environment) =
                self.environment_revision(&status.environment_id, &status.revision)?
                && self
                    .status(&id, &environment)?
                    .is_some_and(|current| current.state == "pending")
                && matches!(status.state.as_str(), "ready" | "failed")
            {
                // A reconnect may replay the previous result after a new explicit prepare was queued.
                continue;
            }
            if let Err(error) = self.receive_status(&id, status) {
                eprintln!("ignored environment reconciliation status: {error:#}");
            }
        }
        self.schedule()?;
        Ok(session)
    }
    fn receive(&mut self, worker: &str, session: Uuid, message: WorkerMessage) -> Result<()> {
        if self.sessions.get(worker).is_none_or(|s| s.id != session) {
            bail!("superseded worker connection")
        }
        if let Some(mut info) =
            self.one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", worker)?
        {
            info.last_seen = now();
            self.save_worker(&info)?;
        }
        // Logs never change scheduling; skipping `schedule` keeps reconnect log replay linear.
        let reschedule = !matches!(message, WorkerMessage::Log { .. });
        match message {
            WorkerMessage::Hello { .. } => bail!("hello already received"),
            WorkerMessage::Started { job_id, attempt_id } => {
                let mut job = self.assigned(worker, &job_id, &attempt_id)?;
                if active(&job.state) {
                    job.state = if self.cancel_requested(&job.id)? {
                        "cancelling"
                    } else {
                        "running"
                    }
                    .into();
                    job.error = None;
                    job.updated_at = now();
                    self.save_job(&job)?;
                }
            }
            WorkerMessage::Log { entry } => {
                if let Some(environment) = entry.job_id.strip_prefix("environment:") {
                    let (id, revision) = environment
                        .split_once(':')
                        .context("invalid environment log ID")?;
                    identifier(id)?;
                    identifier(revision)?;
                    // Workers replay preparation logs on every reconnect; rejecting a deleted
                    // environment's log would drop the connection and loop forever.
                    if self.environment(id)?.is_none() {
                        return Ok(());
                    }
                    let environment = self
                        .environment_revision(id, revision)?
                        .context("unknown environment log revision")?;
                    if self.status(worker, &environment)?.is_none() {
                        bail!("worker has no preparation for this environment")
                    }
                    let stable = format!("environment:{worker}:{id}:{revision}");
                    if entry.attempt_id != stable && Uuid::parse_str(&entry.attempt_id).is_err() {
                        bail!("invalid preparation log attempt")
                    }
                } else {
                    self.assigned(worker, &entry.job_id, &entry.attempt_id)?;
                }
                if entry.seq > i64::MAX as u64
                    || !matches!(entry.stream.as_str(), "stdout" | "stderr" | "system")
                    || entry.text.len() > 1024 * 1024
                {
                    bail!("invalid log entry")
                }
                self.db.execute(
                    "INSERT OR IGNORE INTO logs(job,attempt,seq,data) VALUES(?1,?2,?3,?4)",
                    params![
                        entry.job_id,
                        entry.attempt_id,
                        entry.seq as i64,
                        encode(&entry)?
                    ],
                )?;
            }
            WorkerMessage::Finished { report } => self.report(worker, report)?,
            WorkerMessage::Environment { status } => self.receive_status(worker, status)?,
            WorkerMessage::Heartbeat => {}
            WorkerMessage::UpdateFailed { error } => {
                if let Some(mut info) = self.one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", worker)? {
                    info.update_error = Some(error);
                    self.save_worker(&info)?;
                }
            }
        }
        if reschedule {
            self.schedule()?;
        }
        Ok(())
    }
    fn disconnect(&mut self, worker: &str, session: Uuid) -> Result<()> {
        if self.sessions.get(worker).is_none_or(|s| s.id != session) {
            return Ok(());
        }
        self.sessions.remove(worker);
        if let Some(mut info) =
            self.one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", worker)?
        {
            info.online = false;
            info.last_seen = now();
            self.save_worker(&info)?;
        }
        for mut job in self.jobs()? {
            if job.worker_id.as_deref() == Some(worker) && active(&job.state) {
                job.state = "unknown".into();
                job.updated_at = now();
                job.error =
                    Some("worker disconnected; awaiting reconciliation, no automatic retry".into());
                self.save_job(&job)?;
            }
        }
        for mut status in self.statuses()? {
            if status.worker_id == worker && status.state == "preparing" {
                status.state = "unknown".into();
                status.error = Some(
                    "worker disconnected during preparation; reconnect or explicitly prepare again"
                        .into(),
                );
                self.save_status(&status)?;
            }
        }
        self.schedule()
    }
}

fn candidate(
    worker: &WorkerInfo,
    spec: &JobSpec,
    environment: Option<&Environment>,
) -> Vec<String> {
    let container = spec
        .container
        .as_ref()
        .or(environment.and_then(|environment| environment.image.as_ref()));
    let mut capabilities = worker.capabilities.clone();
    let mut reasons = vec![];
    if container.is_some() {
        if !capabilities
            .runtimes
            .iter()
            .any(|runtime| matches!(runtime.as_str(), "docker" | "podman"))
        {
            reasons.push("no usable Docker/Podman runtime".into());
        }
        let native_os = capabilities.os.clone();
        capabilities.os = "linux".into();
        if native_os != "linux" {
            // Desktop runtimes execute in a VM; host CPU feature probes are not VM evidence.
            capabilities.cpu_features.clear();
        }
        capabilities.abi.clear();
        capabilities.tools.clear();
        capabilities
            .gpu
            .retain(|gpu| native_os == "linux" && matches!(gpu.as_str(), "cuda" | "rocm"));
        if let Some(environment) = environment {
            capabilities.abi = environment.requirements.abi.clone().unwrap_or_default();
            capabilities.tools = environment.requirements.tools.clone();
        }
    } else if let Some(environment) = environment {
        capabilities
            .tools
            .extend(environment.requirements.tools.clone());
    }
    if let Some(target) =
        environment.and_then(|environment| environment.requirements.build_target.as_ref())
    {
        // A recipe declares the target; the worker must link a probe before publishing ready.
        capabilities
            .tools
            .insert(format!("target:{target}"), "ready".into());
    }
    if let Some(environment) = environment {
        let mut requirements = environment.requirements.clone();
        // Tools are installed/verified during preparation; OS, CPU and native ABI cannot be claimed by a recipe.
        requirements.tools.clear();
        reasons.extend(
            capabilities
                .mismatch(&requirements)
                .into_iter()
                .map(|reason| format!("environment: {reason}")),
        );
    }
    reasons.extend(capabilities.mismatch(&spec.requirements));
    if let Some(target) = &spec.requirements.build_target
        && capabilities
            .tools
            .get(&format!("target:{target}"))
            .is_none_or(|state| state != "ready")
    {
        reasons.push(format!(
            "build target {target} has no verified compiler/linker probe"
        ));
    }
    reasons
}
fn incompatibilities(
    workers: &[WorkerInfo],
    spec: &JobSpec,
    environment: Option<&Environment>,
) -> Vec<String> {
    workers
        .iter()
        .filter_map(|worker| {
            let reasons = candidate(worker, spec, environment);
            (!reasons.is_empty()).then(|| format!("{}: {}", worker.id, reasons.join(", ")))
        })
        .collect()
}

pub async fn run(listen: String, data_dir: PathBuf, token_file: PathBuf) -> Result<()> {
    let token = crate::client::read_token(&token_file)?;
    let state = Arc::new(Mutex::new(Store::open(data_dir, token)?));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    eprintln!("YLCC hub listening on {listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
fn router(state: Shared) -> Router {
    let protected = Router::new()
        .route("/v1/workers", get(workers))
        .route("/v1/workers/{id}", axum::routing::delete(delete_worker))
        .route("/v1/workers/{id}/control", post(control_worker))
        .route("/v1/workers/{id}/update", post(update_worker))
        .route("/v1/blobs/missing", post(missing_blobs))
        .route("/v1/blobs/{hash}", put(put_blob).get(get_blob))
        .route("/v1/jobs", post(submit).get(jobs))
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/logs", get(logs))
        .route("/v1/jobs/{id}/cancel", post(cancel))
        .route("/v1/jobs/{id}/priority", post(set_priority))
        .route(
            "/v1/environments",
            get(environments).post(upsert_environment),
        )
        .route("/v1/environments/status", get(statuses))
        .route("/v1/environments/{id}", axum::routing::delete(delete_environment))
        .route("/v1/environments/{id}/prepare", post(prepare))
        .route("/v1/environments/{id}/logs", get(environment_logs))
        .route("/v1/worker/ws", get(worker_socket))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/", get(dashboard_index))
        .route("/{*path}", get(dashboard_asset))
        .route(
            "/health",
            get(|| async {
                Json(serde_json::json!({
                    "status":"ok",
                    "version":crate::VERSION,
                    "capabilities":["job_priority", "worker_control"]
                }))
            }),
        )
        .merge(protected)
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}
async fn dashboard_index() -> Response {
    dashboard_asset(Path("index.html".into())).await
}
#[cfg(dashboard_assets)]
async fn dashboard_asset(Path(path): Path<String>) -> Response {
    if path == "v1" || path.starts_with("v1/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if path.is_empty() { "index.html" } else { &path };
    let asset = DashboardAssets::get(path).or_else(|| {
        (!path.contains('.') && !path.starts_with("assets/"))
            .then(|| DashboardAssets::get("index.html"))
            .flatten()
    });
    let Some(asset) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    let mut response = Response::new(Body::from(asset.data.into_owned()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type.parse().expect("valid content type"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-cache".parse().expect("valid cache control"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        "nosniff".parse().expect("valid security header"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
            .parse()
            .expect("valid content security policy"),
    );
    response
}
async fn authenticate(
    State(state): State<Shared>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let valid = {
        let Ok(store) = state.lock() else {
            return ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "hub store unavailable".into(),
            )
            .into_response();
        };
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|value| {
                value.len() == store.token.len()
                    && value
                        .bytes()
                        .zip(store.token.bytes())
                        .fold(0u8, |acc, (left, right)| acc | left ^ right)
                        == 0
            })
    };
    if !valid {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "valid Bearer token required".into(),
        )
        .into_response();
    }
    next.run(request).await
}
async fn workers(State(state): State<Shared>) -> ApiResult<Json<Vec<WorkerInfo>>> {
    Ok(Json(lock(&state)?.workers()?))
}
async fn delete_worker(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let mut guard = lock(&state)?;
    // Deleting a missing worker succeeds so retries are idempotent.
    if guard.jobs().unwrap_or_default().into_iter().any(|job| {
        (job.state == "assigned" || job.state == "running") && job.worker_id.as_deref() == Some(&id)
    }) {
        return Err(invalid("worker has active jobs; drain it first"));
    }
    guard.delete_worker(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn control_worker(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(control): Json<WorkerControl>,
) -> ApiResult<Json<WorkerInfo>> {
    Ok(Json(lock(&state)?.control_worker(&id, control)?))
}
async fn update_worker(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> ApiResult<Json<WorkerInfo>> {
    let version = request.version;
    if !version.starts_with('v')
        || version.len() > 64
        || !version[1..].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return Err(invalid("version must be a release tag such as v0.4.6"));
    }
    let mut store = lock(&state)?;
    let mut worker = store
        .one::<WorkerInfo>("SELECT data FROM workers WHERE id=?1", &id)?
        .ok_or_else(|| missing("worker"))?;
    if worker.capabilities.containerized {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "container workers update by recreating the container from the new release image".into(),
        ));
    }
    let Some(session) = store.sessions.get(&id).filter(|_| worker.online) else {
        return Err(ApiError(StatusCode::CONFLICT, "worker is offline".into()));
    };
    if session.tx.send(HubMessage::Update { version }).is_err() {
        return Err(ApiError(StatusCode::CONFLICT, "worker connection closed".into()));
    }
    worker.update_error = None;
    store.save_worker(&worker)?;
    Ok(Json(worker))
}
async fn set_priority(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(request): Json<PriorityRequest>,
) -> ApiResult<Json<Job>> {
    Ok(Json(lock(&state)?.set_priority(&id, request.priority)?))
}
async fn jobs(State(state): State<Shared>) -> ApiResult<Json<Vec<Job>>> {
    Ok(Json(lock(&state)?.jobs()?))
}
async fn job(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    Ok(Json(lock(&state)?.job(&id)?.ok_or_else(|| missing("job"))?))
}
async fn submit(State(state): State<Shared>, Json(spec): Json<JobSpec>) -> ApiResult<Json<Job>> {
    Ok(Json(lock(&state)?.add_job(spec)?))
}
async fn logs(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<LogEntry>>> {
    let store = lock(&state)?;
    if store.job(&id)?.is_none() {
        return Err(missing("job"));
    }
    let mut statement = store
        .db
        .prepare("SELECT data FROM logs WHERE job=?1 ORDER BY seq")
        .map_err(anyhow::Error::from)?;
    let mut entries = vec![];
    for row in statement
        .query_map([id], |r| r.get::<_, String>(0))
        .map_err(anyhow::Error::from)?
    {
        entries.push(
            serde_json::from_str(&row.map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?,
        );
    }
    Ok(Json(entries))
}
async fn cancel(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    let mut store = lock(&state)?;
    let mut job = store.job(&id)?.ok_or_else(|| missing("job"))?;
    if !terminal(&job.state) {
        store
            .db
            .execute("UPDATE jobs SET cancel_requested=1 WHERE id=?1", [&id])
            .map_err(anyhow::Error::from)?;
        if job.state == "queued" {
            job.state = "cancelled".into();
        } else if let Some(session) = job
            .worker_id
            .as_deref()
            .and_then(|worker| store.sessions.get(worker))
        {
            job.state = "cancelling".into();
            if session
                .tx
                .send(HubMessage::Cancel {
                    job_id: job.id.clone(),
                    attempt_id: job.attempt_id.clone(),
                })
                .is_err()
            {
                job.state = "unknown".into();
                job.error = Some("cancel recorded; awaiting worker reconnection".into());
            }
        } else {
            job.state = "unknown".into();
            job.error = Some("cancel recorded; awaiting worker reconnection".into());
        }
        job.updated_at = now();
        store.save_job(&job)?;
        store.schedule()?;
    }
    Ok(Json(job))
}
async fn missing_blobs(
    State(state): State<Shared>,
    Json(request): Json<MissingBlobs>,
) -> ApiResult<Json<MissingBlobs>> {
    let store = lock(&state)?;
    let mut hashes = BTreeSet::new();
    for hash in request.hashes {
        validate_hash(&hash).map_err(invalid)?;
        if !store.blobs.join(&hash).is_file() {
            hashes.insert(hash);
        }
    }
    Ok(Json(MissingBlobs {
        hashes: hashes.into_iter().collect(),
    }))
}
async fn put_blob(
    State(state): State<Shared>,
    Path(hash): Path<String>,
    body: Body,
) -> ApiResult<StatusCode> {
    validate_hash(&hash).map_err(invalid)?;
    let directory = lock(&state)?.blobs.clone();
    let temporary = tempfile::NamedTempFile::new_in(&directory).map_err(anyhow::Error::from)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen().map_err(anyhow::Error::from)?);
    let mut digest = Sha256::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        digest.update(&chunk);
        file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
    }
    file.flush().await.map_err(anyhow::Error::from)?;
    file.sync_all().await.map_err(anyhow::Error::from)?;
    if hex::encode(digest.finalize()) != hash {
        return Err(invalid("body does not match requested SHA-256"));
    }
    drop(file);
    if let Err(error) = temporary.persist_noclobber(directory.join(&hash))
        && error.error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(anyhow::Error::from(error.error).into());
    }
    Ok(StatusCode::CREATED)
}
async fn get_blob(State(state): State<Shared>, Path(hash): Path<String>) -> ApiResult<Response> {
    validate_hash(&hash).map_err(invalid)?;
    let path = lock(&state)?.blobs.join(hash);
    let file = tokio::fs::File::open(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing("blob")
        } else {
            anyhow::Error::from(error).into()
        }
    })?;
    let length = file.metadata().await.map_err(anyhow::Error::from)?.len();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().expect("static header"),
    );
    headers.insert(header::CONTENT_LENGTH, length.into());
    Ok((headers, Body::from_stream(ReaderStream::new(file))).into_response())
}
async fn environments(State(state): State<Shared>) -> ApiResult<Json<Vec<Environment>>> {
    Ok(Json(lock(&state)?.environments()?))
}
async fn statuses(State(state): State<Shared>) -> ApiResult<Json<Vec<EnvironmentStatus>>> {
    Ok(Json(lock(&state)?.statuses()?))
}
async fn delete_environment(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    identifier(&id).map_err(invalid)?;
    let mut store = lock(&state)?;
    store.delete_environment(&id)?;
    // Offline workers miss this and reclaim the files through their unused-age collection.
    for session in store.sessions.values() {
        let _ = session.tx.send(HubMessage::Evict { environment_id: id.clone() });
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn environment_logs(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<LogEntry>>> {
    identifier(&id).map_err(invalid)?;
    let store = lock(&state)?;
    if store.environment(&id)?.is_none() {
        return Err(missing("environment"));
    }
    let mut entries = vec![];
    let mut statement = store
        .db
        .prepare("SELECT data FROM logs WHERE substr(job, 1, length(?1)) = ?1 ORDER BY rowid")
        .map_err(anyhow::Error::from)?;
    for row in statement
        .query_map([format!("environment:{id}:")], |r| r.get::<_, String>(0))
        .map_err(anyhow::Error::from)?
    {
        entries.push(
            serde_json::from_str(&row.map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?,
        );
    }
    Ok(Json(entries))
}
async fn upsert_environment(
    State(state): State<Shared>,
    Json(environment): Json<Environment>,
) -> ApiResult<Json<Environment>> {
    identifier(&environment.id).map_err(invalid)?;
    identifier(&environment.revision).map_err(invalid)?;
    if environment.requirements.emulation {
        return Err(invalid("emulation is not implemented"));
    }
    if let Some(image) = &environment.image {
        image_ref(image).map_err(invalid)?;
        let (repository, digest) = image.rsplit_once("@sha256:").ok_or_else(|| {
            invalid("catalogue images require an immutable repository@sha256:digest reference")
        })?;
        if repository.is_empty() {
            return Err(invalid("image repository is required"));
        }
        validate_hash(digest).map_err(invalid)?;
        if !environment.setup.is_empty() {
            return Err(invalid(
                "image setup is not persistent; build a custom image or use a native setup recipe",
            ));
        }
    }
    for key in environment.env.keys() {
        validate_env_key(key).map_err(invalid)?;
    }
    let mut store = lock(&state)?;
    if let Some(previous) = store.environment_revision(&environment.id, &environment.revision)?
        && encode(&previous)? != encode(&environment)?
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "environment revisions are immutable; choose a new revision".into(),
        ));
    }
    store
        .db
        .execute(
            "INSERT OR IGNORE INTO environment_revisions(id,revision,data) VALUES(?1,?2,?3)",
            params![environment.id, environment.revision, encode(&environment)?],
        )
        .map_err(anyhow::Error::from)?;
    store.db.execute("INSERT INTO environments(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![environment.id, encode(&environment)?]).map_err(anyhow::Error::from)?;
    store.schedule()?;
    Ok(Json(environment))
}
async fn prepare(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Json(request): Json<PrepareRequest>,
) -> ApiResult<Json<Vec<EnvironmentStatus>>> {
    let mut store = lock(&state)?;
    let environment = store
        .environment(&id)?
        .ok_or_else(|| missing("environment"))?;
    let workers: Vec<_> = store
        .workers()?
        .into_iter()
        .filter(|worker| request.worker_id.as_ref().is_none_or(|id| id == &worker.id))
        .collect();
    if workers.is_empty() {
        return Err(missing("worker"));
    }
    let spec = JobSpec::default();
    let mut result = vec![];
    let mut reasons = vec![];
    for worker in workers {
        let mismatch = candidate(&worker, &spec, Some(&environment));
        if !mismatch.is_empty() {
            reasons.push(format!("{}: {}", worker.id, mismatch.join(", ")));
            continue;
        }
        if let Some(status) = store.status(&worker.id, &environment)?
            && (status.state == "preparing" || status.state == "pending")
        {
            result.push(status);
            continue;
        }
        let status = EnvironmentStatus {
            worker_id: worker.id,
            environment_id: id.clone(),
            revision: environment.revision.clone(),
            state: "pending".into(),
            error: None,
        };
        store.save_status(&status)?;
        result.push(status);
    }
    if result.is_empty() {
        return Err(invalid(format!(
            "no compatible worker: {}",
            reasons.join("; ")
        )));
    }
    store.schedule()?;
    for status in &mut result {
        *status = store
            .status(&status.worker_id, &environment)?
            .expect("saved status");
    }
    Ok(Json(result))
}
async fn worker_socket(State(state): State<Shared>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| socket_loop(state, socket))
}
async fn socket_loop(state: Shared, mut socket: WebSocket) {
    let hello = tokio::time::timeout(Duration::from_secs(15), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = hello else {
        return;
    };
    let Ok(WorkerMessage::Hello {
        worker_id,
        capabilities,
        max_jobs,
        attempts,
        environments,
        version,
    }) = serde_json::from_str(&text)
    else {
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connected = match state.lock() {
        Ok(mut store) => store.connect(
            worker_id.clone(),
            capabilities,
            max_jobs,
            attempts,
            environments,
            version,
            tx,
        ),
        Err(_) => return,
    };
    let session = match connected {
        Ok(session) => session,
        Err(error) => {
            eprintln!("worker hello rejected: {error:#}");
            return;
        }
    };
    let (mut sink, mut stream) = socket.split();
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    let mut last_message = Instant::now();
    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(outgoing) = outgoing else { break };
                let Ok(text) = serde_json::to_string(&outgoing) else { break };
                if sink.send(Message::Text(text.into())).await.is_err() { break; }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        last_message = Instant::now();
                        let result = match serde_json::from_str::<WorkerMessage>(&text) {
                            Ok(message) => match state.lock() { Ok(mut store) => store.receive(&worker_id, session, message), Err(_) => break },
                            Err(error) => Err(error.into()),
                        };
                        if let Err(error) = result { eprintln!("worker message rejected: {error:#}"); break; }
                    }
                    Some(Ok(Message::Ping(bytes))) => { last_message = Instant::now(); if sink.send(Message::Pong(bytes)).await.is_err() { break; } }
                    Some(Ok(Message::Pong(_))) => last_message = Instant::now(),
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => break,
                }
            }
            _ = ticker.tick() => {
                if last_message.elapsed() > Duration::from_secs(90) { break; }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
        }
    }
    if let Ok(mut store) = state.lock()
        && let Err(error) = store.disconnect(&worker_id, session)
    {
        eprintln!("worker disconnect: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().into(), "test-token-long-enough".into()).unwrap();
        (dir, store)
    }
    fn caps() -> Capabilities {
        Capabilities {
            os: "linux".into(),
            arch: "x86_64".into(),
            abi: "gnu".into(),
            cpus: 4,
            ..Default::default()
        }
    }
    fn spec() -> JobSpec {
        JobSpec {
            command: vec!["true".into()],
            ..Default::default()
        }
    }
    fn worker(store: &mut Store, id: &str) -> (Uuid, mpsc::UnboundedReceiver<HubMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = store
            .connect(id.into(), caps(), 1, vec![], vec![], String::new(), tx)
            .unwrap();
        (session, rx)
    }
    fn finish(store: &mut Store, job: &Job) {
        store
            .report(
                job.worker_id.as_deref().unwrap(),
                AttemptReport {
                    job_id: job.id.clone(),
                    attempt_id: job.attempt_id.clone(),
                    state: "succeeded".into(),
                    exit_code: Some(0),
                    error: None,
                    results: vec![],
                },
            )
            .unwrap();
        store.schedule().unwrap();
    }

    #[cfg(dashboard_assets)]
    #[tokio::test]
    async fn dashboard_serves_embedded_assets_but_not_unknown_api_paths() {
        let index = dashboard_index().await;
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(
            index.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let html = axum::body::to_bytes(index.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(html.starts_with(b"<!doctype html>"));

        let script = dashboard_asset(Path("assets/dashboard.js".into())).await;
        assert_eq!(script.status(), StatusCode::OK);
        assert_eq!(
            script.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );

        let unknown_api = dashboard_asset(Path("v1/not-a-route".into())).await;
        assert_eq!(unknown_api.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(dashboard_assets)]
    #[tokio::test]
    async fn dashboard_is_public_but_worker_api_requires_authentication() {
        use tower::ServiceExt;

        let (_dir, store) = test_store();
        let app = router(Arc::new(Mutex::new(store)));
        let home = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(home.status(), StatusCode::OK);

        let workers = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/workers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(workers.status(), StatusCode::UNAUTHORIZED);

        let unknown_api = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/not-a-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown_api.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn catalogue_rejects_reserved_environment_keys_in_every_case() {
        let (_dir, store) = test_store();
        let state = Arc::new(Mutex::new(store));
        for key in [
            "",
            "YLCC_JOB_ID",
            "ylcc_job_id",
            "YlCc_attempt_id",
            "bad=name",
            "bad\0name",
        ] {
            let environment = Environment {
                id: "native".into(),
                revision: "v1".into(),
                requirements: Requirements::default(),
                setup: vec![],
                verify: vec![],
                image: None,
                env: BTreeMap::from([(key.into(), "value".into())]),
            };
            let error = upsert_environment(State(state.clone()), Json(environment))
                .await
                .unwrap_err();
            assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
        let environment = Environment {
            id: "native".into(),
            revision: "v1".into(),
            requirements: Requirements::default(),
            setup: vec![],
            verify: vec![],
            image: None,
            env: BTreeMap::from([("PATH".into(), "tools".into())]),
        };
        assert!(
            upsert_environment(State(state), Json(environment))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn environment_delete_is_idempotent_and_waits_for_jobs() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let state = Arc::new(Mutex::new(store));
        let environment = Environment {
            id: "native".into(),
            revision: "v1".into(),
            requirements: Requirements::default(),
            setup: vec![],
            verify: vec![],
            image: None,
            env: BTreeMap::new(),
        };
        assert!(
            upsert_environment(State(state.clone()), Json(environment.clone()))
                .await
                .is_ok()
        );
        let job = lock(&state)
            .unwrap()
            .add_job(JobSpec {
                environment: Some("native".into()),
                ..spec()
            })
            .unwrap();
        let blocked = delete_environment(State(state.clone()), Path("native".into()))
            .await
            .unwrap_err();
        assert_eq!(blocked.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(job.state, "queued");
        let cancelled = cancel(State(state.clone()), Path(job.id.clone())).await.unwrap();
        assert_eq!(cancelled.0.state, "cancelled");
        for _ in 0..2 {
            assert_eq!(
                delete_environment(State(state.clone()), Path("native".into()))
                    .await
                    .unwrap(),
                StatusCode::NO_CONTENT
            );
        }
        {
            let mut store = lock(&state).unwrap();
            assert!(store.environments().unwrap().is_empty());
            let stale = EnvironmentStatus {
                worker_id: "one".into(),
                environment_id: "native".into(),
                revision: "v1".into(),
                state: "ready".into(),
                error: None,
            };
            assert!(store.receive_status("one", stale.clone()).is_err());
            let evicted = EnvironmentStatus { state: "evicted".into(), ..stale };
            assert!(store.receive_status("one", evicted).is_ok());
            let mut evicts = 0;
            while let Ok(message) = rx.try_recv() {
                evicts += matches!(message, HubMessage::Evict { ref environment_id } if environment_id == "native") as usize;
            }
            assert_eq!(evicts, 2, "each delete tells connected workers to free the files");
            let session = store.sessions["one"].id;
            let replayed = LogEntry {
                job_id: "environment:native:v1".into(),
                attempt_id: "environment:one:native:v1".into(),
                seq: 1,
                stream: "system".into(),
                text: "replayed".into(),
            };
            assert!(store
                .receive("one", session, WorkerMessage::Log { entry: replayed })
                .is_ok());
            assert!(store.statuses().unwrap().is_empty());
        }
        // A deleted id can be registered again.
        assert!(upsert_environment(State(state), Json(environment)).await.is_ok());
    }

    #[test]
    fn pause_drains_active_jobs_and_resume_assigns_queued_work() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let first = store.add_job(spec()).unwrap();
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
        let paused = store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(paused.paused);
        let second = store.add_job(spec()).unwrap();
        assert_eq!(second.state, "queued");
        assert_eq!(store.job(&first.id).unwrap().unwrap().state, "assigned");
        assert!(rx.try_recv().is_err());
        finish(&mut store, &first);
        assert_eq!(store.job(&second.id).unwrap().unwrap().state, "queued");
        assert!(rx.try_recv().is_err());
        store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(store.job(&second.id).unwrap().unwrap().state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
    }

    #[test]
    fn pause_also_defers_environment_preparation() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let environment = Environment {
            id: "rust".into(),
            revision: "v1".into(),
            setup: vec!["true".into()],
            ..Default::default()
        };
        store
            .db
            .execute(
                "INSERT INTO environments(id,data) VALUES(?1,?2)",
                params![environment.id, encode(&environment).unwrap()],
            )
            .unwrap();
        store
            .save_status(&EnvironmentStatus {
                worker_id: "one".into(),
                environment_id: environment.id.clone(),
                revision: environment.revision.clone(),
                state: "pending".into(),
                error: None,
            })
            .unwrap();
        let job = store
            .add_job(JobSpec {
                environment: Some(environment.id.clone()),
                ..spec()
            })
            .unwrap();
        assert_eq!(job.state, "queued");
        assert_eq!(
            store.status("one", &environment).unwrap().unwrap().state,
            "pending"
        );
        assert!(rx.try_recv().is_err());
        store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Prepare { .. }));
    }

    #[test]
    fn controls_survive_reconnect_restart_and_changing_worker_capacity() {
        let (dir, mut store) = test_store();
        let (tx, _rx) = mpsc::unbounded_channel();
        store
            .connect("one".into(), caps(), 4, vec![], vec![], String::new(), tx)
            .unwrap();
        let info = store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(true),
                    max_jobs: Some(3),
                },
            )
            .unwrap();
        assert_eq!(info.max_jobs, 3);
        drop(store);
        let mut store = Store::open(dir.path().into(), "test-token-long-enough".into()).unwrap();
        let restored = store.workers().unwrap().remove(0);
        assert!(restored.paused);
        assert!(!restored.online);
        assert_eq!(restored.max_jobs, 3);
        for (advertised, expected) in [(4, 3), (2, 2), (4, 3)] {
            let (tx, _rx) = mpsc::unbounded_channel();
            store
                .connect("one".into(), caps(), advertised, vec![], vec![], String::new(), tx)
                .unwrap();
            let info = store.workers().unwrap().remove(0);
            assert!(info.paused);
            assert_eq!(info.max_jobs, expected);
        }
        for invalid_limit in [0, 5, 257] {
            assert_eq!(
                store
                    .control_worker(
                        "one",
                        WorkerControl {
                            max_jobs: Some(invalid_limit),
                            ..Default::default()
                        }
                    )
                    .unwrap_err()
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            store
                .control_worker(
                    "missing",
                    WorkerControl {
                        paused: Some(true),
                        ..Default::default()
                    }
                )
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            store
                .control_worker("one", WorkerControl::default())
                .unwrap_err()
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn decreasing_concurrency_drains_without_cancelling_running_jobs() {
        let (_dir, mut store) = test_store();
        let (tx, mut rx) = mpsc::unbounded_channel();
        store
            .connect("one".into(), caps(), 2, vec![], vec![], String::new(), tx)
            .unwrap();
        let first = store.add_job(spec()).unwrap();
        let second = store.add_job(spec()).unwrap();
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        store
            .control_worker(
                "one",
                WorkerControl {
                    max_jobs: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
        let third = store.add_job(spec()).unwrap();
        assert_eq!(third.state, "queued");
        assert_eq!(store.job(&first.id).unwrap().unwrap().state, "assigned");
        assert_eq!(store.job(&second.id).unwrap().unwrap().state, "assigned");
        assert!(rx.try_recv().is_err());
        finish(&mut store, &first);
        assert_eq!(store.job(&third.id).unwrap().unwrap().state, "queued");
        finish(&mut store, &second);
        assert_eq!(store.job(&third.id).unwrap().unwrap().state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
        store
            .control_worker(
                "one",
                WorkerControl {
                    max_jobs: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(store.add_job(spec()).unwrap().state, "assigned");
    }

    #[test]
    fn priorities_schedule_highest_first_and_equal_priorities_stay_fifo() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let first = store.add_job(spec()).unwrap();
        let second = store
            .add_job(JobSpec {
                priority: 10,
                ..spec()
            })
            .unwrap();
        let third = store
            .add_job(JobSpec {
                priority: 10,
                ..spec()
            })
            .unwrap();
        let fourth = store.add_job(spec()).unwrap();
        store.set_priority(&fourth.id, 20).unwrap();
        assert_eq!(
            store.set_priority(&first.id, 101).unwrap_err().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            store.set_priority("missing", 0).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        let original_order: Vec<_> = store.jobs().unwrap().into_iter().map(|j| j.id).collect();
        assert_eq!(
            original_order,
            [
                first.id.clone(),
                second.id.clone(),
                third.id.clone(),
                fourth.id.clone()
            ]
        );
        store
            .control_worker(
                "one",
                WorkerControl {
                    paused: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        for expected in [&fourth, &second, &third, &first] {
            let HubMessage::Run { job, .. } = rx.try_recv().unwrap() else {
                panic!("expected run");
            };
            assert_eq!(job.id, expected.id);
            assert_eq!(
                store.set_priority(&job.id, 0).unwrap_err().0,
                StatusCode::CONFLICT
            );
            finish(&mut store, &job);
            assert_eq!(
                store.set_priority(&job.id, 0).unwrap_err().0,
                StatusCode::CONFLICT
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hub_data_is_private_and_directory_aliases_work() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let parent = tempfile::tempdir().unwrap();
        let data = parent.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alias = parent.path().join("alias");
        symlink(&data, &alias).unwrap();
        let store = Store::open(alias, "test-token-long-enough".into()).unwrap();
        assert_eq!(
            std::fs::metadata(&data).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&store.blobs)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert!(data.join("hub.sqlite").is_file());
    }

    #[test]
    fn windows_workers_register_and_receive_matching_jobs_but_unknown_os_is_rejected() {
        let (_dir, mut store) = test_store();
        let (tx, _rx) = mpsc::unbounded_channel();
        let error = store
            .connect(
                "unknown".into(),
                Capabilities {
                    os: "unknown".into(),
                    ..caps()
                },
                1,
                vec![],
                vec![],
                String::new(),
                tx,
            )
            .unwrap_err();
        assert!(error.to_string().contains("unsupported worker OS"));
        assert!(store.workers().unwrap().is_empty());

        let (tx, mut rx) = mpsc::unbounded_channel();
        store
            .connect(
                "windows".into(),
                Capabilities {
                    os: "windows".into(),
                    abi: "msvc".into(),
                    ..caps()
                },
                1,
                vec![],
                vec![],
                String::new(),
                tx,
            )
            .unwrap();
        let job = store
            .add_job(JobSpec {
                requirements: Requirements {
                    os: Some("windows".into()),
                    arch: Some("x86_64".into()),
                    ..Default::default()
                },
                ..spec()
            })
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(job.worker_id.as_deref(), Some("windows"));
        assert_eq!(job.state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
    }
    #[test]
    fn queues_busy_worker_and_rejects_incompatible_platform() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let first = store.add_job(spec()).unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(first.state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
        assert_eq!(
            store
                .add_job(spec())
                .unwrap_or_else(|e| panic!("{}", e.1))
                .state,
            "queued"
        );
        let bad = JobSpec {
            requirements: Requirements {
                os: Some("macos".into()),
                ..Default::default()
            },
            ..spec()
        };
        assert_eq!(
            store.add_job(bad).err().unwrap().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    #[test]
    fn disconnect_never_requeues_and_old_session_cannot_disconnect_replacement() {
        let (_dir, mut store) = test_store();
        let (old, _rx) = worker(&mut store, "one");
        let job = store.add_job(spec()).unwrap_or_else(|e| panic!("{}", e.1));
        store.disconnect("one", old).unwrap();
        assert_eq!(store.job(&job.id).unwrap().unwrap().state, "unknown");
        let (new, _rx) = worker(&mut store, "one");
        store.disconnect("one", old).unwrap();
        assert!(store.workers().unwrap()[0].online);
        assert!(store.receive("one", old, WorkerMessage::Heartbeat).is_err());
        store
            .receive(
                "one",
                new,
                WorkerMessage::Finished {
                    report: AttemptReport {
                        job_id: job.id.clone(),
                        attempt_id: job.attempt_id,
                        state: "succeeded".into(),
                        exit_code: Some(0),
                        error: None,
                        results: vec![],
                    },
                },
            )
            .unwrap();
        assert_eq!(store.job(&job.id).unwrap().unwrap().state, "succeeded");
    }
    #[test]
    fn logs_are_deduplicated_and_foreign_attempts_rejected() {
        let (_dir, mut store) = test_store();
        let (session, _rx) = worker(&mut store, "one");
        let job = store.add_job(spec()).unwrap_or_else(|e| panic!("{}", e.1));
        let entry = LogEntry {
            job_id: job.id.clone(),
            attempt_id: job.attempt_id.clone(),
            seq: 1,
            stream: "stdout".into(),
            text: "hello".into(),
        };
        for _ in 0..2 {
            store
                .receive(
                    "one",
                    session,
                    WorkerMessage::Log {
                        entry: entry.clone(),
                    },
                )
                .unwrap();
        }
        let count: u64 = store
            .db
            .query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert!(
            store
                .report(
                    "other",
                    AttemptReport {
                        job_id: job.id,
                        attempt_id: job.attempt_id,
                        state: "succeeded".into(),
                        exit_code: Some(0),
                        error: None,
                        results: vec![]
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn restarts_preserve_assignment_and_environment_snapshot() {
        let (dir, mut store) = test_store();
        let (_session, _rx) = worker(&mut store, "one");
        let job = store.add_job(spec()).unwrap_or_else(|e| panic!("{}", e.1));
        drop(store);
        let store = Store::open(dir.path().into(), "test-token-long-enough".into()).unwrap();
        let restored = store.job(&job.id).unwrap().unwrap();
        assert_eq!(restored.state, "unknown");
        assert_eq!(restored.attempt_id, job.attempt_id);
        assert!(!store.workers().unwrap()[0].online);
    }
    #[test]
    fn image_jobs_do_not_inherit_host_tools_or_metal() {
        let mut capabilities = caps();
        capabilities.os = "macos".into();
        capabilities.runtimes.push("docker".into());
        capabilities.tools.insert("rustc".into(), "1.96.1".into());
        capabilities.gpu.push("metal".into());
        let worker = WorkerInfo {
            id: "one".into(),
            capabilities,
            max_jobs: 1,
            paused: false,
            online: true,
            last_seen: now(),
            version: String::new(),
            update_error: None,
        };
        let mut job = spec();
        job.container = Some("example/image@sha256:abc".into());
        job.requirements.os = Some("linux".into());
        assert!(candidate(&worker, &job, None).is_empty());
        job.requirements
            .tools
            .insert("rustc".into(), "1.96.1".into());
        assert!(!candidate(&worker, &job, None).is_empty());
        job.requirements.tools.clear();
        job.requirements.gpu = Some("metal".into());
        assert!(!candidate(&worker, &job, None).is_empty());
    }
    #[test]
    fn environment_preparation_drains_and_failed_setup_is_not_retried() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let environment = Environment {
            id: "python".into(),
            revision: "v1".into(),
            requirements: Requirements {
                tools: BTreeMap::from([("python3".into(), "3.14.0".into())]),
                ..Default::default()
            },
            setup: vec!["true".into()],
            verify: vec!["true".into()],
            image: None,
            env: BTreeMap::new(),
        };
        store
            .db
            .execute(
                "INSERT INTO environments(id,data) VALUES(?1,?2)",
                params![environment.id, encode(&environment).unwrap()],
            )
            .unwrap();
        let first = store.add_job(spec()).unwrap_or_else(|e| panic!("{}", e.1));
        rx.try_recv().unwrap();
        let second = store
            .add_job(JobSpec {
                environment: Some(environment.id.clone()),
                ..spec()
            })
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(second.state, "queued");
        assert!(rx.try_recv().is_err());
        store
            .report(
                "one",
                AttemptReport {
                    job_id: first.id,
                    attempt_id: first.attempt_id,
                    state: "succeeded".into(),
                    exit_code: Some(0),
                    error: None,
                    results: vec![],
                },
            )
            .unwrap();
        store.schedule().unwrap();
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Prepare { .. }));
        store
            .receive_status(
                "one",
                EnvironmentStatus {
                    worker_id: "one".into(),
                    environment_id: environment.id,
                    revision: environment.revision,
                    state: "failed".into(),
                    error: Some("install failed".into()),
                },
            )
            .unwrap();
        store.schedule().unwrap();
        assert_eq!(store.job(&second.id).unwrap().unwrap().state, "failed");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn queued_cancel_survives_crash_before_state_update() {
        let (_dir, mut store) = test_store();
        let (_session, _rx) = worker(&mut store, "one");
        let _running = store.add_job(spec()).unwrap();
        let queued = store.add_job(spec()).unwrap();
        store
            .db
            .execute(
                "UPDATE jobs SET cancel_requested=1 WHERE id=?1",
                [&queued.id],
            )
            .unwrap();
        store.schedule().unwrap();
        assert_eq!(store.job(&queued.id).unwrap().unwrap().state, "cancelled");
    }

    #[tokio::test]
    async fn authenticated_http_verifies_and_streams_immutable_blobs() {
        let (_dir, store) = test_store();
        let state = Arc::new(Mutex::new(store));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let token = "test-token-long-enough";
        assert_eq!(
            client
                .get(format!("{address}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{address}/v1/workers"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let bytes = b"a frozen source snapshot\n";
        let hash = hex::encode(Sha256::digest(bytes));
        let url = format!("{address}/v1/blobs/{hash}");
        assert_eq!(
            client
                .put(&url)
                .bearer_auth(token)
                .body("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            client
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        for _ in 0..2 {
            assert_eq!(
                client
                    .put(&url)
                    .bearer_auth(token)
                    .body(bytes.to_vec())
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CREATED
            );
        }
        let response = client.get(&url).bearer_auth(token).send().await.unwrap();
        assert_eq!(response.content_length(), Some(bytes.len() as u64));
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
        let missing: MissingBlobs = client
            .post(format!("{address}/v1/blobs/missing"))
            .bearer_auth(token)
            .json(&MissingBlobs { hashes: vec![hash] })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(missing.hashes.is_empty());
        task.abort();
    }
}

#[cfg(not(dashboard_assets))]
async fn dashboard_asset(Path(_path): Path<String>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}
