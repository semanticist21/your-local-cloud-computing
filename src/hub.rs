use crate::model::*;
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    Extension,
    extract::{
        DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
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
    /// The scoped token the worker authenticated with, so revoking it closes the session.
    token: Option<String>,
}
/// Audit rows kept; older events are pruned so a busy pool cannot fill the disk.
const AUDIT_RETENTION: i64 = 10_000;
/// Worker-caused audit events (connect, rejected) allowed per token per hour.
const WORKER_AUDIT_PER_HOUR: u32 = 20;
struct Store {
    db: Connection,
    blobs: PathBuf,
    token: String,
    sessions: HashMap<String, Session>,
    /// Active scoped tokens by SHA-256 of their secret.
    tokens: HashMap<String, TokenInfo>,
    /// When each token's `last_used_at` was last written, to bound writes from polling clients.
    token_used: HashMap<String, u64>,
    /// Hourly worker audit budget per actor: window start, events written, suppression noted.
    worker_audit: HashMap<String, (u64, u32, bool)>,
    /// Last audited connect signature per worker id, so steady reconnects are not re-logged.
    connect_seen: HashMap<String, String>,
    audit_inserts: u64,
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
/// Worker resource samples older than this are deleted as new ones arrive.
const METRICS_RETENTION: u64 = 7 * 24 * 60 * 60;
fn active(state: &str) -> bool {
    matches!(state, "assigned" | "running" | "cancelling" | "unknown")
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn token_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}
/// The token file the Hub was started with: full access, kept for existing single-token pools.
fn pool_principal() -> Principal {
    Principal {
        name: "pool".into(),
        role: Role::Admin,
        token_id: None,
        worker_id: None,
    }
}
fn audit_key(principal: &Principal) -> String {
    principal
        .token_id
        .clone()
        .unwrap_or_else(|| format!("name:{}", principal.name))
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
            CREATE TABLE IF NOT EXISTS logs (job TEXT NOT NULL, attempt TEXT NOT NULL, seq INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(job, attempt, seq));
            CREATE TABLE IF NOT EXISTS tokens (id TEXT PRIMARY KEY, hash TEXT NOT NULL UNIQUE, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS audit (seq INTEGER PRIMARY KEY AUTOINCREMENT, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS metrics (worker TEXT NOT NULL, at INTEGER NOT NULL, cpu REAL NOT NULL, memory_used INTEGER NOT NULL, memory_total INTEGER NOT NULL, load REAL NOT NULL, disk REAL NOT NULL, PRIMARY KEY(worker, at)) WITHOUT ROWID;")?;
        let mut store = Self {
            db,
            blobs: data.join("blobs"),
            token,
            sessions: HashMap::new(),
            tokens: HashMap::new(),
            token_used: HashMap::new(),
            worker_audit: HashMap::new(),
            connect_seen: HashMap::new(),
            audit_inserts: 0,
        };
        {
            let mut statement = store.db.prepare("SELECT hash, data FROM tokens")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (hash, data) = row?;
                let info: TokenInfo = serde_json::from_str(&data)?;
                if info.revoked_at.is_none() {
                    store.tokens.insert(hash, info);
                }
            }
        }
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
        self.db.execute("DELETE FROM metrics WHERE worker=?1", [id])?;
        Ok(())
    }
    /// Averages samples from the last `range` seconds into at most about 300 points.
    fn metrics(&self, worker: &str, range: u64) -> Result<Vec<WorkerMetrics>> {
        let range = range.clamp(60, METRICS_RETENTION);
        let step = (range / 300).max(15) as i64;
        let mut statement = self.db.prepare(
            "SELECT at/?3*?3, avg(cpu), avg(memory_used), max(memory_total), avg(load), avg(disk)
             FROM metrics WHERE worker=?1 AND at>=?2 GROUP BY at/?3 ORDER BY 1",
        )?;
        let rows = statement.query_map(params![worker, (now() - range) as i64, step], |r| {
            Ok(WorkerMetrics {
                at: r.get::<_, i64>(0)? as u64,
                cpu_percent: r.get(1)?,
                memory_used: r.get::<_, f64>(2)? as u64,
                memory_total: r.get::<_, i64>(3)? as u64,
                load: r.get(4)?,
                disk_percent: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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

    /// Maps a bearer secret to its identity; the pool token is compared in constant time and
    /// scoped tokens are found by the SHA-256 of the secret, which reveals nothing by timing.
    fn resolve(&mut self, bearer: &str) -> Option<Principal> {
        if bearer.len() == self.token.len()
            && bearer
                .bytes()
                .zip(self.token.bytes())
                .fold(0u8, |acc, (left, right)| acc | left ^ right)
                == 0
        {
            return Some(pool_principal());
        }
        let hash = token_hash(bearer);
        let info = self.tokens.get_mut(&hash)?;
        let principal = Principal {
            name: info.name.clone(),
            role: info.role,
            token_id: Some(info.id.clone()),
            worker_id: info.worker_id.clone(),
        };
        let time = now();
        info.last_used_at = Some(time);
        if self
            .token_used
            .get(&info.id)
            .is_none_or(|written| time.saturating_sub(*written) >= 60)
        {
            self.token_used.insert(info.id.clone(), time);
            if let Ok(data) = encode(&*info)
                && let Err(error) = self.db.execute(
                    "UPDATE tokens SET data=?2 WHERE id=?1",
                    params![info.id, data],
                )
            {
                eprintln!("hub: token last-used update failed: {error:#}");
            }
        }
        Some(principal)
    }
    fn create_token(&mut self, request: TokenRequest, by: &Principal) -> ApiResult<CreatedToken> {
        identifier(&request.name).map_err(invalid)?;
        if request.name == "pool" {
            return Err(invalid("token name pool is reserved for the Hub's token file"));
        }
        match (request.role, &request.worker_id) {
            (Role::Worker, Some(worker)) => identifier(worker).map_err(invalid)?,
            (Role::Worker, None) => return Err(invalid("worker tokens require worker_id")),
            (_, Some(_)) => return Err(invalid("only worker tokens are bound to a worker_id")),
            (_, None) => {}
        }
        if self.tokens.values().any(|token| token.name == request.name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "an active token already has this name; revoke it or choose another".into(),
            ));
        }
        let secret = format!(
            "ylcc_{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let info = TokenInfo {
            id: format!("t{}", &Uuid::new_v4().simple().to_string()[..12]),
            name: request.name,
            role: request.role,
            worker_id: request.worker_id,
            created_at: now(),
            created_by: by.name.clone(),
            last_used_at: None,
            revoked_at: None,
        };
        let hash = token_hash(&secret);
        self.db
            .execute(
                "INSERT INTO tokens(id,hash,data) VALUES(?1,?2,?3)",
                params![info.id, hash, encode(&info)?],
            )
            .map_err(anyhow::Error::from)?;
        self.tokens.insert(hash, info.clone());
        self.record(
            by,
            "token.create",
            &info.id,
            Some(format!(
                "{} {}{}",
                info.name,
                info.role,
                info.worker_id
                    .as_deref()
                    .map(|worker| format!(" worker={worker}"))
                    .unwrap_or_default()
            )),
        );
        Ok(CreatedToken { token: info, secret })
    }
    fn list_tokens(&self) -> Result<Vec<TokenInfo>> {
        let mut tokens: Vec<TokenInfo> = self.rows("SELECT data FROM tokens ORDER BY rowid")?;
        // In-memory use times are fresher than the throttled database copy.
        for token in &mut tokens {
            if let Some(active) = self.tokens.values().find(|active| active.id == token.id) {
                token.last_used_at = active.last_used_at;
            }
        }
        Ok(tokens)
    }
    /// Revokes a token and closes worker sessions opened with it; repeating it is harmless.
    fn revoke_token(&mut self, id: &str, by: &Principal) -> ApiResult<TokenInfo> {
        let mut info = self
            .one::<TokenInfo>("SELECT data FROM tokens WHERE id=?1", id)?
            .ok_or_else(|| missing("token"))?;
        if info.revoked_at.is_some() {
            return Ok(info);
        }
        info.revoked_at = Some(now());
        self.db
            .execute(
                "UPDATE tokens SET data=?2 WHERE id=?1",
                params![info.id, encode(&info)?],
            )
            .map_err(anyhow::Error::from)?;
        self.tokens.retain(|_, token| token.id != id);
        self.token_used.remove(id);
        self.worker_audit.remove(id);
        let sessions: Vec<(String, Uuid)> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.token.as_deref() == Some(id))
            .map(|(worker, session)| (worker.clone(), session.id))
            .collect();
        for (worker, session) in sessions {
            self.disconnect(&worker, session)?;
        }
        self.record(by, "token.revoke", &info.id, Some(info.name.clone()));
        Ok(info)
    }

    fn set_dashboard_password(&mut self, password: &str, by: &Principal) -> ApiResult<()> {
        if password.len() != 64 || !password.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid("password must be a 64-character SHA-256 hex digest"));
        }
        let hash = token_hash(password);
        if self.tokens.contains_key(&hash) {
            return Err(invalid("this password collides with an existing credential; choose another"));
        }
        let tx = self.db.transaction().map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM tokens WHERE id=?1", params!["dashboard-password"])
            .map_err(anyhow::Error::from)?;
        let info = TokenInfo {
            id: "dashboard-password".into(),
            name: "dashboard-password".into(),
            role: Role::Admin,
            worker_id: None,
            created_at: now(),
            created_by: by.name.clone(),
            last_used_at: None,
            revoked_at: None,
        };
        tx.execute(
            "INSERT INTO tokens(id,hash,data) VALUES(?1,?2,?3)",
            params![info.id, hash, encode(&info)?],
        ).map_err(anyhow::Error::from)?;
        tx.commit().map_err(anyhow::Error::from)?;
        self.tokens.retain(|_, token| token.id != "dashboard-password");
        self.token_used.remove("dashboard-password");
        self.tokens.insert(hash, info.clone());
        self.record(by, "dashboard_password.set", "dashboard-password", None);
        Ok(())
    }

    fn delete_dashboard_password(&mut self, by: &Principal) -> ApiResult<()> {
        let tx = self.db.transaction().map_err(anyhow::Error::from)?;
        tx.execute("DELETE FROM tokens WHERE id=?1", params!["dashboard-password"])
            .map_err(anyhow::Error::from)?;
        tx.commit().map_err(anyhow::Error::from)?;
        self.tokens.retain(|_, token| token.id != "dashboard-password");
        self.token_used.remove("dashboard-password");
        self.record(by, "dashboard_password.delete", "dashboard-password", None);
        Ok(())
    }

    fn has_dashboard_password(&self) -> bool {
        self.tokens.values().any(|t| t.id == "dashboard-password")
    }
    fn audit(
        &mut self,
        principal: &Principal,
        action: &str,
        target: &str,
        detail: Option<String>,
    ) -> Result<()> {
        let event = AuditEvent {
            seq: 0,
            at: now(),
            actor: principal.name.clone(),
            actor_token_id: principal.token_id.clone(),
            role: principal.role,
            action: action.into(),
            target: target.into(),
            detail,
        };
        self.db
            .execute("INSERT INTO audit(data) VALUES(?1)", [encode(&event)?])?;
        self.audit_inserts += 1;
        if self.audit_inserts.is_multiple_of(100) {
            self.db.execute(
                "DELETE FROM audit WHERE seq <= (SELECT MAX(seq) FROM audit) - ?1",
                [AUDIT_RETENTION],
            )?;
        }
        Ok(())
    }
    /// Audits after a change is already committed: a failed audit write is logged rather than
    /// reported, so a client never retries (and duplicates) a change that took effect.
    fn record(&mut self, principal: &Principal, action: &str, target: &str, detail: Option<String>) {
        if let Err(error) = self.audit(principal, action, target, detail) {
            eprintln!("hub: audit {action} {target} failed: {error:#}");
        }
    }
    /// Audits an event a worker connection can trigger at will, within a per-token hourly budget,
    /// so one worker token cannot push older events out of the retention window.
    fn audit_worker(
        &mut self,
        principal: &Principal,
        action: &str,
        target: &str,
        detail: Option<String>,
    ) -> Result<()> {
        let time = now();
        // Scoped tokens share one budget whatever id they claim; the pool token (admin, and
        // often every worker's credential in older pools) gets one per worker id.
        let key = match &principal.token_id {
            Some(_) => audit_key(principal),
            None => format!("{}:{target}", audit_key(principal)),
        };
        let budget = self
            .worker_audit
            .entry(key)
            .or_insert((time, 0, false));
        if time.saturating_sub(budget.0) >= 3600 {
            *budget = (time, 0, false);
        }
        if budget.1 >= WORKER_AUDIT_PER_HOUR {
            if !budget.2 {
                budget.2 = true;
                return self.audit(
                    principal,
                    "worker.audit_suppressed",
                    target,
                    Some(format!("more than {WORKER_AUDIT_PER_HOUR} worker events this hour")),
                );
            }
            return Ok(());
        }
        budget.1 += 1;
        self.audit(principal, action, target, detail)
    }
    fn audit_events(&self, limit: usize, before: Option<i64>) -> Result<Vec<AuditEvent>> {
        let mut statement = self.db.prepare(
            "SELECT seq, data FROM audit WHERE seq < ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![before.unwrap_or(i64::MAX), limit.clamp(1, 1000) as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (seq, data) = row?;
            let mut event: AuditEvent = serde_json::from_str(&data)?;
            event.seq = seq;
            Ok(event)
        })
        .collect()
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
    fn add_job(&mut self, spec: JobSpec, submitted_by: Option<String>) -> ApiResult<Job> {
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
            submitted_by,
            started_at: None,
            last_output_at: None,
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
        principal: &Principal,
    ) -> Result<Uuid> {
        identifier(&id)?;
        match principal.role {
            Role::Requester => bail!("requester tokens cannot connect workers"),
            Role::Worker if principal.worker_id.as_deref() != Some(id.as_str()) => bail!(
                "worker token is bound to worker {}",
                principal.worker_id.as_deref().unwrap_or("(none)")
            ),
            _ => {}
        }
        // The token was valid at the upgrade; re-check under the lock in case it was revoked
        // while the hello was pending.
        if let Some(token) = &principal.token_id
            && !self.tokens.values().any(|active| &active.id == token)
        {
            bail!("token was revoked")
        }
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
        self.sessions.insert(
            id.clone(),
            Session {
                id: session,
                tx,
                token: principal.token_id.clone(),
            },
        );
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
                    job.started_at = Some(now());
                    job.last_output_at = None;
                    self.save_job(&job)?;
                }
            }
            WorkerMessage::Log { entry } => {
                let job = if let Some(environment) = entry.job_id.strip_prefix("environment:") {
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
                    None
                } else {
                    Some(self.assigned(worker, &entry.job_id, &entry.attempt_id)?)
                };
                if entry.seq > i64::MAX as u64
                    || !matches!(entry.stream.as_str(), "stdout" | "stderr" | "system")
                    || entry.text.len() > 1024 * 1024
                {
                    bail!("invalid log entry")
                }
                let inserted = self.db.execute(
                    "INSERT OR IGNORE INTO logs(job,attempt,seq,data) VALUES(?1,?2,?3,?4)",
                    params![
                        entry.job_id,
                        entry.attempt_id,
                        entry.seq as i64,
                        encode(&entry)?
                    ],
                )? == 1;
                // Throttled so streaming output does not rewrite the job on every chunk; replays are skipped.
                if let Some(mut job) = job.filter(|job| {
                    inserted
                        && entry.stream != "system"
                        && active(&job.state)
                        && job.last_output_at.is_none_or(|at| now() >= at + 5)
                }) {
                    job.last_output_at = Some(now());
                    self.save_job(&job)?;
                }
            }
            WorkerMessage::Finished { report } => self.report(worker, report)?,
            WorkerMessage::Environment { status } => self.receive_status(worker, status)?,
            WorkerMessage::Heartbeat => {}
            WorkerMessage::Metrics { metrics: m } => {
                let at = now() as i64;
                self.db.execute(
                    "INSERT OR REPLACE INTO metrics VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![worker, at, m.cpu_percent, m.memory_used as i64, m.memory_total as i64, m.load, m.disk_percent],
                )?;
                self.db.execute(
                    "DELETE FROM metrics WHERE worker=?1 AND at<?2",
                    params![worker, at - METRICS_RETENTION as i64],
                )?;
            }
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
    if spec
        .requirements
        .worker
        .as_ref()
        .is_some_and(|id| id != &worker.id)
    {
        reasons.push("not the requested worker".into());
    }
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
    // Routing only: the worker enforces its policy again before running anything.
    if let Some(policy) = &worker.capabilities.policy
        && let Err(reason) = policy.check_job(spec, environment)
    {
        reasons.push(reason);
    }
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
    // Every /v1 route belongs to exactly one role group; a route outside these groups would be
    // unauthenticated, so add new routes to the narrowest group that fits.
    let admin = Router::new()
        .route("/v1/workers/{id}", axum::routing::delete(delete_worker))
        .route("/v1/workers/{id}/control", post(control_worker))
        .route("/v1/workers/{id}/update", post(update_worker))
        .route("/v1/environments/{id}", axum::routing::delete(delete_environment))
        .route("/v1/tokens", get(tokens).post(create_token))
        .route("/v1/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/v1/dashboard-password", get(get_dashboard_password).post(set_dashboard_password).delete(delete_dashboard_password))
        .route("/v1/audit", get(audit))
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_only));
    let requester = Router::new()
        .route("/v1/workers", get(workers))
        .route("/v1/workers/{id}/metrics", get(worker_metrics))
        .route("/v1/blobs/missing", post(missing_blobs))
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
        .route("/v1/environments/{id}/prepare", post(prepare))
        .route("/v1/environments/{id}/logs", get(environment_logs))
        .route_layer(middleware::from_fn_with_state(state.clone(), requester_only));
    let blobs = Router::new()
        .route("/v1/blobs/{hash}", put(put_blob).get(get_blob))
        .route("/v1/whoami", get(whoami))
        .route_layer(middleware::from_fn_with_state(state.clone(), any_role));
    let worker = Router::new()
        .route("/v1/worker/ws", get(worker_socket))
        .route_layer(middleware::from_fn_with_state(state.clone(), worker_only));
    let protected = admin.merge(requester).merge(blobs).merge(worker);
    Router::new()
        .route("/", get(dashboard_index))
        .route("/{*path}", get(dashboard_asset))
        .route(
            "/health",
            get(|| async {
                Json(serde_json::json!({
                    "status":"ok",
                    "version":crate::VERSION,
                    "capabilities":["job_priority", "worker_control", "scoped_tokens", "audit_log", "command_policy", "job_worker", "worker_metrics"]
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
async fn admin_only(State(state): State<Shared>, request: axum::extract::Request, next: Next) -> Response {
    authorize(state, request, next, &[Role::Admin]).await
}
async fn requester_only(State(state): State<Shared>, request: axum::extract::Request, next: Next) -> Response {
    authorize(state, request, next, &[Role::Admin, Role::Requester]).await
}
async fn worker_only(State(state): State<Shared>, request: axum::extract::Request, next: Next) -> Response {
    authorize(state, request, next, &[Role::Admin, Role::Worker]).await
}
async fn any_role(State(state): State<Shared>, request: axum::extract::Request, next: Next) -> Response {
    authorize(state, request, next, &[Role::Admin, Role::Requester, Role::Worker]).await
}
/// Resolves the bearer token to a `Principal` for handlers, or answers 401 (unknown token) or
/// 403 (known token whose role cannot use this route).
async fn authorize(
    state: Shared,
    mut request: axum::extract::Request,
    next: Next,
    allowed: &[Role],
) -> Response {
    let principal = {
        let Ok(mut store) = state.lock() else {
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
            .and_then(|value| store.resolve(value))
    };
    let Some(principal) = principal else {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "valid Bearer token required".into(),
        )
        .into_response();
    };
    if !allowed.contains(&principal.role) {
        return ApiError(
            StatusCode::FORBIDDEN,
            format!("this token's role ({}) cannot use this endpoint", principal.role),
        )
        .into_response();
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}
async fn whoami(Extension(principal): Extension<Principal>) -> Json<Principal> {
    Json(principal)
}
async fn tokens(State(state): State<Shared>) -> ApiResult<Json<Vec<TokenInfo>>> {
    Ok(Json(lock(&state)?.list_tokens()?))
}
async fn create_token(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<TokenRequest>,
) -> ApiResult<Json<CreatedToken>> {
    Ok(Json(lock(&state)?.create_token(request, &principal)?))
}
async fn revoke_token(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<TokenInfo>> {
    Ok(Json(lock(&state)?.revoke_token(&id, &principal)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardPasswordRequest {
    password: String,
}

async fn get_dashboard_password(
    State(state): State<Shared>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({ "set": lock(&state)?.has_dashboard_password() })))
}

async fn set_dashboard_password(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<DashboardPasswordRequest>,
) -> ApiResult<StatusCode> {
    lock(&state)?.set_dashboard_password(&request.password, &principal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_dashboard_password(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<StatusCode> {
    lock(&state)?.delete_dashboard_password(&principal)?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    limit: Option<usize>,
    before: Option<i64>,
}
async fn audit(
    State(state): State<Shared>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEvent>>> {
    Ok(Json(
        lock(&state)?.audit_events(query.limit.unwrap_or(200), query.before)?,
    ))
}
async fn workers(State(state): State<Shared>) -> ApiResult<Json<Vec<WorkerInfo>>> {
    Ok(Json(lock(&state)?.workers()?))
}
#[derive(Deserialize)]
struct MetricsQuery {
    #[serde(default = "default_metrics_range")]
    range: u64,
}
fn default_metrics_range() -> u64 {
    3600
}
async fn worker_metrics(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(query): Query<MetricsQuery>,
) -> ApiResult<Json<Vec<WorkerMetrics>>> {
    Ok(Json(lock(&state)?.metrics(&id, query.range)?))
}
async fn delete_worker(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let mut guard = lock(&state)?;
    // Deleting a missing worker succeeds so retries are idempotent.
    if guard.jobs().unwrap_or_default().into_iter().any(|job| {
        (job.state == "assigned" || job.state == "running") && job.worker_id.as_deref() == Some(&id)
    }) {
        return Err(invalid("worker has active jobs; drain it first"));
    }
    guard.delete_worker(&id)?;
    guard.record(&principal, "worker.delete", &id, None);
    Ok(StatusCode::NO_CONTENT)
}

async fn control_worker(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(control): Json<WorkerControl>,
) -> ApiResult<Json<WorkerInfo>> {
    let mut store = lock(&state)?;
    let detail = encode(&control)?;
    let worker = store.control_worker(&id, control)?;
    store.record(&principal, "worker.control", &id, Some(detail));
    Ok(Json(worker))
}
async fn update_worker(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
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
    if session.tx.send(HubMessage::Update { version: version.clone() }).is_err() {
        return Err(ApiError(StatusCode::CONFLICT, "worker connection closed".into()));
    }
    worker.update_error = None;
    store.save_worker(&worker)?;
    store.record(&principal, "worker.update", &id, Some(version));
    Ok(Json(worker))
}
async fn set_priority(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<PriorityRequest>,
) -> ApiResult<Json<Job>> {
    let mut store = lock(&state)?;
    let unchanged = store.job(&id)?.is_some_and(|job| job.spec.priority == request.priority);
    let job = store.set_priority(&id, request.priority)?;
    if unchanged {
        return Ok(Json(job));
    }
    store.record(&principal, "job.priority", &id, Some(request.priority.to_string()));
    Ok(Json(job))
}
async fn jobs(State(state): State<Shared>) -> ApiResult<Json<Vec<Job>>> {
    Ok(Json(lock(&state)?.jobs()?))
}
async fn job(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Job>> {
    Ok(Json(lock(&state)?.job(&id)?.ok_or_else(|| missing("job"))?))
}
async fn submit(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Json(spec): Json<JobSpec>,
) -> ApiResult<Json<Job>> {
    let mut store = lock(&state)?;
    let job = store.add_job(spec, Some(principal.name.clone()))?;
    let mut detail = job.spec.command.first().cloned().unwrap_or_default();
    if let Some(image) = &job.spec.container {
        detail.push_str(&format!(" container={image}"));
    }
    if let Some(environment) = &job.spec.environment {
        detail.push_str(&format!(" environment={environment}"));
    }
    store.record(&principal, "job.submit", &job.id, Some(detail));
    Ok(Json(job))
}
#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    after: u64,
}
async fn logs(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<Vec<LogEntry>>> {
    let store = lock(&state)?;
    if store.job(&id)?.is_none() {
        return Err(missing("job"));
    }
    let mut statement = store
        .db
        .prepare("SELECT data FROM logs WHERE job=?1 AND seq>?2 ORDER BY seq")
        .map_err(anyhow::Error::from)?;
    let mut entries = vec![];
    for row in statement
        .query_map(params![id, query.after.min(i64::MAX as u64) as i64], |r| {
            r.get::<_, String>(0)
        })
        .map_err(anyhow::Error::from)?
    {
        entries.push(
            serde_json::from_str(&row.map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?,
        );
    }
    Ok(Json(entries))
}
async fn cancel(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<Json<Job>> {
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
        store.record(&principal, "job.cancel", &id, None);
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
async fn delete_environment(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    identifier(&id).map_err(invalid)?;
    let mut store = lock(&state)?;
    store.delete_environment(&id)?;
    store.record(&principal, "environment.delete", &id, None);
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
    Extension(principal): Extension<Principal>,
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
    let inserted = store
        .db
        .execute(
            "INSERT OR IGNORE INTO environment_revisions(id,revision,data) VALUES(?1,?2,?3)",
            params![environment.id, environment.revision, encode(&environment)?],
        )
        .map_err(anyhow::Error::from)?;
    if inserted > 0 {
        store.record(
            &principal,
            "environment.upsert",
            &format!("{}@{}", environment.id, environment.revision),
            None,
        );
    }
    store.db.execute("INSERT INTO environments(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![environment.id, encode(&environment)?]).map_err(anyhow::Error::from)?;
    store.schedule()?;
    Ok(Json(environment))
}
async fn prepare(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
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
    store.record(
        &principal,
        "environment.prepare",
        &format!("{id}@{}", environment.revision),
        Some(
            result
                .iter()
                .map(|status| status.worker_id.as_str())
                .collect::<Vec<_>>()
                .join(","),
        ),
    );
    for status in &mut result {
        *status = store
            .status(&status.worker_id, &environment)?
            .expect("saved status");
    }
    Ok(Json(result))
}
async fn worker_socket(
    State(state): State<Shared>,
    Extension(principal): Extension<Principal>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| socket_loop(state, socket, principal))
}
async fn socket_loop(state: Shared, mut socket: WebSocket, principal: Principal) {
    let hello = tokio::time::timeout(Duration::from_secs(15), socket.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = hello else {
        return;
    };
    let (worker_id, capabilities, max_jobs, attempts, environments, version) =
        match serde_json::from_str(&text) {
            Ok(WorkerMessage::Hello {
                worker_id,
                capabilities,
                max_jobs,
                attempts,
                environments,
                version,
            }) => (worker_id, capabilities, max_jobs, attempts, environments, version),
            Ok(_) => {
                eprintln!("worker connection rejected: first message was not hello");
                return;
            }
            Err(error) => {
                eprintln!("worker hello rejected: {error:#}");
                return;
            }
        };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let session = {
        let Ok(mut store) = state.lock() else { return };
        let signature = format!(
            "{}|{}|{version}",
            principal.name,
            principal.token_id.as_deref().unwrap_or("")
        );
        let session = match store.connect(
            worker_id.clone(),
            capabilities,
            max_jobs,
            attempts,
            environments,
            version.clone(),
            tx,
            &principal,
        ) {
            Ok(session) => {
                if store.connect_seen.get(&worker_id) != Some(&signature) {
                    store.connect_seen.insert(worker_id.clone(), signature);
                    let mut detail = format!("version={version}");
                    if principal.role == Role::Admin {
                        detail.push_str(" (admin token used as a worker credential)");
                    }
                    if let Err(error) =
                        store.audit_worker(&principal, "worker.connect", &worker_id, Some(detail))
                    {
                        eprintln!("hub: audit failed: {error:#}");
                    }
                }
                session
            }
            Err(error) => {
                eprintln!("worker hello rejected: {error:#}");
                if let Err(error) = store.audit_worker(
                    &principal,
                    "worker.rejected",
                    &worker_id,
                    Some(format!("{error:#}")),
                ) {
                    eprintln!("hub: audit failed: {error:#}");
                }
                return;
            }
        };
        session
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
            .connect(id.into(), caps(), 1, vec![], vec![], String::new(), tx, &pool_principal())
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
            let error = upsert_environment(State(state.clone()), Extension(pool_principal()), Json(environment))
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
            upsert_environment(State(state), Extension(pool_principal()), Json(environment))
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
            upsert_environment(State(state.clone()), Extension(pool_principal()), Json(environment.clone()))
                .await
                .is_ok()
        );
        let job = lock(&state)
            .unwrap()
            .add_job(JobSpec {
                environment: Some("native".into()),
                ..spec()
            }, None)
            .unwrap();
        let blocked = delete_environment(State(state.clone()), Extension(pool_principal()), Path("native".into()))
            .await
            .unwrap_err();
        assert_eq!(blocked.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(job.state, "queued");
        let cancelled = cancel(State(state.clone()), Extension(pool_principal()), Path(job.id.clone())).await.unwrap();
        assert_eq!(cancelled.0.state, "cancelled");
        for _ in 0..2 {
            assert_eq!(
                delete_environment(State(state.clone()), Extension(pool_principal()), Path("native".into()))
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
        assert!(upsert_environment(State(state), Extension(pool_principal()), Json(environment)).await.is_ok());
    }

    #[test]
    fn pause_drains_active_jobs_and_resume_assigns_queued_work() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let first = store.add_job(spec(), None).unwrap();
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
        let second = store.add_job(spec(), None).unwrap();
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
            }, None)
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
            .connect("one".into(), caps(), 4, vec![], vec![], String::new(), tx, &pool_principal())
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
                .connect("one".into(), caps(), advertised, vec![], vec![], String::new(), tx, &pool_principal())
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
            .connect("one".into(), caps(), 2, vec![], vec![], String::new(), tx, &pool_principal())
            .unwrap();
        let first = store.add_job(spec(), None).unwrap();
        let second = store.add_job(spec(), None).unwrap();
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
        let third = store.add_job(spec(), None).unwrap();
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
        assert_eq!(store.add_job(spec(), None).unwrap().state, "assigned");
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
        let first = store.add_job(spec(), None).unwrap();
        let second = store
            .add_job(JobSpec {
                priority: 10,
                ..spec()
            }, None)
            .unwrap();
        let third = store
            .add_job(JobSpec {
                priority: 10,
                ..spec()
            }, None)
            .unwrap();
        let fourth = store.add_job(spec(), None).unwrap();
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
                tx, &pool_principal()
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
                tx, &pool_principal()
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
            }, None)
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(job.worker_id.as_deref(), Some("windows"));
        assert_eq!(job.state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
    }
    #[test]
    fn queues_busy_worker_and_rejects_incompatible_platform() {
        let (_dir, mut store) = test_store();
        let (_session, mut rx) = worker(&mut store, "one");
        let first = store.add_job(spec(), None).unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(first.state, "assigned");
        assert!(matches!(rx.try_recv().unwrap(), HubMessage::Run { .. }));
        assert_eq!(
            store
                .add_job(spec(), None)
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
            store.add_job(bad, None).err().unwrap().0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    #[test]
    fn disconnect_never_requeues_and_old_session_cannot_disconnect_replacement() {
        let (_dir, mut store) = test_store();
        let (old, _rx) = worker(&mut store, "one");
        let job = store.add_job(spec(), None).unwrap_or_else(|e| panic!("{}", e.1));
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
        let job = store.add_job(spec(), None).unwrap_or_else(|e| panic!("{}", e.1));
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
        let job = store.add_job(spec(), None).unwrap_or_else(|e| panic!("{}", e.1));
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
    fn worker_metrics_are_stored_bucketed_and_pruned() {
        let (_dir, mut store) = test_store();
        let (session, _rx) = worker(&mut store, "one");
        let old = (now() - METRICS_RETENTION - 60) as i64;
        store
            .db
            .execute("INSERT INTO metrics VALUES('one',?1,1,1,1,1,1)", [old])
            .unwrap();
        let metrics = WorkerMetrics { cpu_percent: 40.0, memory_used: 2, memory_total: 8, ..Default::default() };
        store.receive("one", session, WorkerMessage::Metrics { metrics }).unwrap();
        let total: i64 = store.db.query_row("SELECT count(*) FROM metrics", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 1, "samples older than the retention are deleted");
        // Two samples inside one 15 s bucket average into one point.
        let bucket = (now() as i64 - 600) / 15 * 15;
        for (offset, cpu) in [(1, 10.0), (2, 30.0)] {
            store.db.execute("INSERT INTO metrics VALUES('one',?1,?2,2,8,0,0)", params![bucket + offset, cpu]).unwrap();
        }
        let samples = store.metrics("one", 3600).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!((samples[0].at as i64, samples[0].cpu_percent), (bucket, 20.0));
        assert_eq!((samples[1].cpu_percent, samples[1].memory_total), (40.0, 8));
        store.delete_worker("one").unwrap();
        assert!(store.metrics("one", 3600).unwrap().is_empty());
    }
    #[test]
    fn worker_requirement_selects_only_that_worker() {
        let worker = WorkerInfo {
            id: "one".into(),
            capabilities: caps(),
            max_jobs: 1,
            paused: false,
            online: true,
            last_seen: now(),
            version: String::new(),
            update_error: None,
        };
        let mut job = spec();
        job.requirements.worker = Some("one".into());
        assert!(candidate(&worker, &job, None).is_empty());
        job.requirements.worker = Some("two".into());
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
        let first = store.add_job(spec(), None).unwrap_or_else(|e| panic!("{}", e.1));
        rx.try_recv().unwrap();
        let second = store
            .add_job(JobSpec {
                environment: Some(environment.id.clone()),
                ..spec()
            }, None)
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
        let _running = store.add_job(spec(), None).unwrap();
        let queued = store.add_job(spec(), None).unwrap();
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

    fn requester() -> TokenRequest {
        TokenRequest { name: "laptop".into(), role: Role::Requester, worker_id: None }
    }
    fn bound(worker: &str) -> TokenRequest {
        TokenRequest { name: format!("{worker}-token"), role: Role::Worker, worker_id: Some(worker.into()) }
    }

    #[test]
    fn scoped_tokens_resolve_until_revoked_and_store_only_hashes() {
        let (dir, mut store) = test_store();
        assert_eq!(store.resolve("test-token-long-enough"), Some(pool_principal()));
        assert_eq!(store.resolve("nope"), None);
        let created = store.create_token(requester(), &pool_principal()).unwrap();
        assert!(created.secret.starts_with("ylcc_") && created.secret.len() == 69);
        let principal = store.resolve(&created.secret).unwrap();
        assert_eq!(principal.role, Role::Requester);
        assert_eq!(principal.token_id.as_deref(), Some(created.token.id.as_str()));
        let stored: String = store
            .db
            .query_row("SELECT hash || data FROM tokens", [], |row| row.get(0))
            .unwrap();
        assert!(!stored.contains(&created.secret));
        assert!(store.create_token(requester(), &pool_principal()).is_err(), "duplicate active name");
        store.revoke_token(&created.token.id, &pool_principal()).unwrap();
        assert_eq!(store.resolve(&created.secret), None);
        assert!(store.revoke_token(&created.token.id, &pool_principal()).is_ok(), "idempotent");
        // Revocation survives a Hub restart.
        drop(store);
        let mut store = Store::open(dir.path().into(), "test-token-long-enough".into()).unwrap();
        assert_eq!(store.resolve(&created.secret), None);
        assert!(store.list_tokens().unwrap()[0].revoked_at.is_some());
    }

    #[test]
    fn token_requests_are_validated_and_pool_is_reserved() {
        let (_dir, mut store) = test_store();
        let by = pool_principal();
        let reserved = TokenRequest { name: "pool".into(), ..requester() };
        assert!(store.create_token(reserved, &by).is_err());
        let unbound = TokenRequest { worker_id: None, ..bound("w") };
        assert!(store.create_token(unbound, &by).is_err());
        let misbound = TokenRequest { worker_id: Some("w".into()), ..requester() };
        assert!(store.create_token(misbound, &by).is_err());
        let invalid_name = TokenRequest { name: "../x".into(), ..requester() };
        assert!(store.create_token(invalid_name, &by).is_err());
    }

    #[test]
    fn worker_tokens_connect_only_as_their_worker() {
        let (_dir, mut store) = test_store();
        let created = store.create_token(bound("builder"), &pool_principal()).unwrap();
        let principal = store.resolve(&created.secret).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let error = store
            .connect("other".into(), caps(), 1, vec![], vec![], String::new(), tx, &principal)
            .unwrap_err();
        assert!(error.to_string().contains("bound to worker builder"));
        let (tx, _rx) = mpsc::unbounded_channel();
        store
            .connect("builder".into(), caps(), 1, vec![], vec![], String::new(), tx, &principal)
            .unwrap();
        let laptop = store.create_token(requester(), &pool_principal()).unwrap();
        let laptop = store.resolve(&laptop.secret).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(store
            .connect("x".into(), caps(), 1, vec![], vec![], String::new(), tx, &laptop)
            .is_err());
    }

    #[test]
    fn revoking_a_worker_token_disconnects_it_and_blocks_a_pending_hello() {
        let (_dir, mut store) = test_store();
        let created = store.create_token(bound("builder"), &pool_principal()).unwrap();
        let principal = store.resolve(&created.secret).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        store
            .connect("builder".into(), caps(), 1, vec![], vec![], String::new(), tx, &principal)
            .unwrap();
        let job = store.add_job(spec(), None).unwrap();
        assert!(matches!(rx.try_recv(), Ok(HubMessage::Run { .. })));
        store.revoke_token(&created.token.id, &pool_principal()).unwrap();
        assert!(!store.sessions.contains_key("builder"));
        assert!(!store.workers().unwrap()[0].online);
        assert_eq!(store.job(&job.id).unwrap().unwrap().state, "unknown");
        // A socket authenticated before the revoke cannot complete its hello afterwards.
        let (tx, _rx) = mpsc::unbounded_channel();
        let error = store
            .connect("builder".into(), caps(), 1, vec![], vec![], String::new(), tx, &principal)
            .unwrap_err();
        assert!(error.to_string().contains("revoked"));
    }

    #[test]
    fn audit_records_actions_and_throttles_worker_events_per_token() {
        let (_dir, mut store) = test_store();
        let by = pool_principal();
        let created = store.create_token(bound("builder"), &by).unwrap();
        let principal = store.resolve(&created.secret).unwrap();
        for attempt in 0..(WORKER_AUDIT_PER_HOUR + 10) {
            store
                .audit_worker(&principal, "worker.rejected", &format!("id-{attempt}"), None)
                .unwrap();
        }
        let events = store.audit_events(1000, None).unwrap();
        let rejected = events.iter().filter(|e| e.action == "worker.rejected").count();
        assert_eq!(rejected, WORKER_AUDIT_PER_HOUR as usize);
        assert_eq!(events.iter().filter(|e| e.action == "worker.audit_suppressed").count(), 1);
        let created_event = events.iter().find(|e| e.action == "token.create").unwrap();
        assert_eq!(created_event.actor, "pool");
        assert_eq!(created_event.target, created.token.id);
        assert_eq!(events[0].seq, events.iter().map(|e| e.seq).max().unwrap(), "newest first");
        let older = store.audit_events(5, Some(events[0].seq)).unwrap();
        assert!(older.iter().all(|e| e.seq < events[0].seq) && older.len() == 5);
    }

    #[test]
    fn audit_prunes_to_retention() {
        let (_dir, mut store) = test_store();
        for _ in 0..(AUDIT_RETENTION + 150) {
            store.audit(&pool_principal(), "job.cancel", "x", None).unwrap();
        }
        let count: i64 = store.db.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0)).unwrap();
        assert!(count <= AUDIT_RETENTION + 100, "{count}");
    }

    async fn call(app: &Router, method: &str, uri: &str, token: Option<&str>, body: &str) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if !body.is_empty() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = app.clone().oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn roles_gate_endpoints_and_submissions_record_the_token_name() {
        let (_dir, mut store) = test_store();
        let laptop = store.create_token(requester(), &pool_principal()).unwrap().secret;
        let builder = store.create_token(bound("builder"), &pool_principal()).unwrap().secret;
        worker(&mut store, "w1");
        let state = Arc::new(Mutex::new(store));
        let app = router(state.clone());
        let pool = Some("test-token-long-enough");
        let hash = "0".repeat(64);
        let blob = format!("/v1/blobs/{hash}");
        for (method, uri, token, expected) in [
            ("GET", "/v1/workers", None, StatusCode::UNAUTHORIZED),
            ("GET", "/v1/workers", Some("wrong"), StatusCode::UNAUTHORIZED),
            ("GET", "/v1/workers", pool, StatusCode::OK),
            ("GET", "/v1/workers", Some(laptop.as_str()), StatusCode::OK),
            ("GET", "/v1/workers", Some(builder.as_str()), StatusCode::FORBIDDEN),
            ("GET", "/v1/jobs", Some(builder.as_str()), StatusCode::FORBIDDEN),
            ("DELETE", "/v1/workers/w1", Some(laptop.as_str()), StatusCode::FORBIDDEN),
            ("DELETE", "/v1/environments/x", Some(laptop.as_str()), StatusCode::FORBIDDEN),
            ("GET", "/v1/tokens", Some(laptop.as_str()), StatusCode::FORBIDDEN),
            ("GET", "/v1/audit", Some(laptop.as_str()), StatusCode::FORBIDDEN),
            ("GET", "/v1/tokens", pool, StatusCode::OK),
            ("GET", "/v1/audit?limit=5", pool, StatusCode::OK),
            ("GET", "/v1/audit?bogus=1", pool, StatusCode::BAD_REQUEST),
            ("GET", blob.as_str(), Some(builder.as_str()), StatusCode::NOT_FOUND),
            ("GET", blob.as_str(), Some(laptop.as_str()), StatusCode::NOT_FOUND),
            ("GET", "/v1/worker/ws", Some(laptop.as_str()), StatusCode::FORBIDDEN),
            ("GET", "/v1/whoami", Some(builder.as_str()), StatusCode::OK),
        ] {
            let (status, body) = call(&app, method, uri, token, "").await;
            assert_eq!(status, expected, "{method} {uri} with {token:?}: {body}");
        }
        let (status, body) = call(&app, "GET", "/v1/whoami", Some(&laptop), "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "laptop");
        assert_eq!(body["role"], "requester");
        let (status, body) = call(&app, "GET", "/v1/workers", Some(&builder), "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("role (worker)"));

        // The submitter comes from the token, never from the request body.
        let (status, job) = call(&app, "POST", "/v1/jobs", Some(&laptop), r#"{"command":["true"],"environment":null,"container":null,"timeout_secs":null,"submitted_by":"someone-else"}"#).await;
        assert_eq!(status, StatusCode::OK, "{job}");
        assert_eq!(job["submitted_by"], "laptop");
        let events = state.lock().unwrap().audit_events(10, None).unwrap();
        let submit = events.iter().find(|e| e.action == "job.submit").unwrap();
        assert_eq!((submit.actor.as_str(), submit.target.as_str()), ("laptop", job["id"].as_str().unwrap()));

        // Token management round trip through the API.
        let (status, created) = call(&app, "POST", "/v1/tokens", pool, r#"{"name":"ci","role":"requester"}"#).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let secret = created["secret"].as_str().unwrap().to_string();
        let id = created["token"]["id"].as_str().unwrap().to_string();
        assert_eq!(call(&app, "GET", "/v1/jobs", Some(&secret), "").await.0, StatusCode::OK);
        let (status, revoked) = call(&app, "DELETE", &format!("/v1/tokens/{id}"), pool, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(revoked["revoked_at"].is_u64());
        assert_eq!(call(&app, "GET", "/v1/jobs", Some(&secret), "").await.0, StatusCode::UNAUTHORIZED);
        let (_, listed) = call(&app, "GET", "/v1/tokens", pool, "").await;
        assert!(listed.as_array().unwrap().iter().all(|t| t.get("secret").is_none() && t.get("hash").is_none()));
    }

    #[tokio::test]
    async fn hub_routes_around_worker_policies_and_rejects_jobs_no_worker_allows() {
        let (_dir, mut store) = test_store();
        let mut restricted = caps();
        restricted.policy = Some(crate::policy::Policy::parse("version: 1\ncommands: [[\"true\"]]\nenvironments: []\n").unwrap());
        let (tx, mut strict_rx) = mpsc::unbounded_channel();
        store
            .connect("strict".into(), restricted, 1, vec![], vec![], String::new(), tx, &pool_principal())
            .unwrap();
        let denied = JobSpec { command: vec!["sh".into(), "-c".into(), "x".into()], ..spec() };
        let error = store.add_job(denied.clone(), None).unwrap_err();
        assert!(error.1.contains("strict: command [\"sh\",\"-c\",\"x\"] is not allowed by worker policy"), "{}", error.1);
        let allowed = store.add_job(JobSpec { command: vec!["true".into()], ..spec() }, None).unwrap();
        assert!(matches!(strict_rx.try_recv(), Ok(HubMessage::Run { job, .. }) if job.id == allowed.id));
        let (_open, mut open_rx) = worker(&mut store, "open");
        let routed = store.add_job(denied, None).unwrap();
        assert!(matches!(open_rx.try_recv(), Ok(HubMessage::Run { job, .. }) if job.id == routed.id));
        assert!(strict_rx.try_recv().is_err());

        // Explicit preparation checks only the environment against each worker's policy.
        let state = Arc::new(Mutex::new(store));
        let environment = Environment { id: "native".into(), revision: "1".into(), setup: vec!["true".into()], ..Default::default() };
        let _ = upsert_environment(State(state.clone()), Extension(pool_principal()), Json(environment)).await.unwrap();
        let prepared = prepare(State(state.clone()), Extension(pool_principal()), Path("native".into()), Json(PrepareRequest::default()))
            .await
            .unwrap();
        assert_eq!(prepared.0.iter().map(|s| s.worker_id.as_str()).collect::<Vec<_>>(), ["open"]);
        let error = prepare(State(state), Extension(pool_principal()), Path("native".into()), Json(PrepareRequest { worker_id: Some("strict".into()) }))
            .await
            .unwrap_err();
        assert!(error.1.contains("environment native"), "{}", error.1);
    }
}

#[cfg(not(dashboard_assets))]
async fn dashboard_asset(Path(_path): Path<String>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}
