use crate::model::*;
use crate::policy::Policy;
#[cfg(windows)]
#[path = "worker_windows.rs"]
mod windows;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{RwLock, Semaphore, broadcast, watch},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    report: AttemptReport,
    seq: u64,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_job: Option<String>,
}
struct Attempt {
    dir: PathBuf,
    journal: Mutex<Journal>,
    cancel: watch::Sender<bool>,
}
struct State {
    hub: String,
    token: String,
    data: PathBuf,
    id: String,
    client: reqwest::Client,
    capabilities: Capabilities,
    max_jobs: usize,
    slots: Arc<Semaphore>,
    drain: RwLock<()>,
    attempts: Mutex<HashMap<String, Arc<Attempt>>>,
    environments: Mutex<BTreeMap<String, EnvironmentStatus>>,
    image_paths: Mutex<HashMap<String, String>>,
    /// Cache keys currently moved into a running attempt.
    caches: Mutex<HashSet<String>>,
    /// Fixed checkout directories currently used by an attempt.
    workspaces: Mutex<HashSet<String>>,
    events: broadcast::Sender<WorkerMessage>,
    /// The operator's allowlist from `--policy`; `None` runs everything the Hub assigns.
    policy: Option<Policy>,
    /// Report host resource samples to Hubs that accept them (off with `--no-metrics`).
    metrics: bool,
}

#[derive(Debug)]
struct UncertainCleanup(String);
impl std::fmt::Display for UncertainCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UncertainCleanup {}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "invalid identifier"
    );
    crate::platform::filesystem_relative(value)?;
    Ok(())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = crate::platform::create_private_file(&tmp)?;
    serde_json::to_writer(&mut file, value)?;
    file.sync_all()?;
    drop(file);
    crate::platform::replace_file(&tmp, path)?;
    if let Some(parent) = path.parent() {
        crate::platform::sync_directory(parent)?;
    }
    Ok(())
}
pub(crate) fn private_dir(path: &Path) -> Result<()> {
    crate::platform::private_dir(path)
        .with_context(|| format!("create private directory {}", path.display()))
}

impl Attempt {
    fn report(&self) -> AttemptReport {
        self.journal.lock().unwrap().report.clone()
    }
    fn update(&self, change: impl FnOnce(&mut Journal)) -> Result<AttemptReport> {
        let mut journal = self.journal.lock().unwrap();
        change(&mut journal);
        atomic_json(&self.dir.join("journal.json"), &*journal)?;
        Ok(journal.report.clone())
    }
    fn log(&self, state: &State, stream: &str, text: String) -> Result<()> {
        let mut journal = self.journal.lock().unwrap();
        journal.seq += 1;
        let entry = LogEntry {
            job_id: journal.report.job_id.clone(),
            attempt_id: journal.report.attempt_id.clone(),
            seq: journal.seq,
            stream: stream.into(),
            text,
        };
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.dir.join("logs.jsonl"))?;
        serde_json::to_writer(&mut file, &entry)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        // The append-only log owns the sequence; journal need not be rewritten for every chunk.
        let _ = state.events.send(WorkerMessage::Log { entry });
        Ok(())
    }
}
fn env_key(id: &str, revision: &str) -> String {
    format!("{id}/{revision}")
}
impl State {
    fn environment_dir(&self, environment: &Environment) -> Result<PathBuf> {
        identifier(&environment.id)?;
        identifier(&environment.revision)?;
        Ok(self
            .data
            .join("environments")
            .join(&environment.id)
            .join(&environment.revision))
    }
    fn emit(&self, message: WorkerMessage) {
        let _ = self.events.send(message);
    }
    fn environment_status(
        &self,
        environment: &Environment,
        status: &str,
        error: Option<String>,
    ) -> Result<()> {
        let mut value = EnvironmentStatus {
            worker_id: self.id.clone(),
            environment_id: environment.id.clone(),
            revision: environment.revision.clone(),
            state: status.into(),
            error,
        };
        let persisted = (|| {
            let dir = self.environment_dir(environment)?;
            private_dir(&dir)?;
            atomic_json(&dir.join("status.json"), &value)
        })();
        if let Err(error) = &persisted {
            if status != "failed" {
                return persisted;
            }
            // A disk failure must not leave the Hub draining this worker forever.
            value.error = Some(format!(
                "{}; could not persist environment failure: {error:#}",
                value
                    .error
                    .as_deref()
                    .unwrap_or("environment preparation failed")
            ));
        }
        self.environments.lock().unwrap().insert(
            env_key(&environment.id, &environment.revision),
            value.clone(),
        );
        self.emit(WorkerMessage::Environment { status: value });
        persisted
    }
    fn hello(&self) -> WorkerMessage {
        WorkerMessage::Hello {
            worker_id: self.id.clone(),
            capabilities: self.capabilities.clone(),
            max_jobs: self.max_jobs,
            attempts: self
                .attempts
                .lock()
                .unwrap()
                .values()
                .map(|a| a.report())
                .collect(),
            environments: self
                .environments
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect(),
            version: crate::VERSION.into(),
        }
    }
}

pub async fn run(
    hub: String,
    token_file: PathBuf,
    data_dir: PathBuf,
    id: String,
    max_jobs: usize,
    policy: Option<Policy>,
    metrics: bool,
) -> Result<()> {
    identifier(&id)?;
    ensure!(max_jobs > 0, "max-jobs must be positive");
    crate::client::validate_hub(&hub)?;
    let token = crate::client::read_token(&token_file)?;
    private_dir(&data_dir)?;
    let data = crate::platform::command_path(&fs::canonicalize(data_dir)?);
    let lock_path = data.join("worker.lock");
    let mut lock_options = OpenOptions::new();
    lock_options.create(true).truncate(false).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        lock_options.share_mode(0);
    }
    let _lock_file = lock_options
        .open(&lock_path)
        .with_context(|| format!("open worker lock file {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        ensure!(
            unsafe { libc::flock(_lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another worker is using this data directory"
        );
    }
    for child in ["attempts", "blobs", "caches", "environments", "workspaces"] {
        private_dir(&data.join(child))?;
    }
    // Background cache pruning: keep disk usage bounded without interrupting active work.
    let prune_data = data.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        loop {
            interval.tick().await;
            let data = prune_data.clone();
            let _ = tokio::task::spawn_blocking(move || prune_worker_cache_root(&data)).await;
        }
    });
    let initial = data.clone();
    tokio::task::spawn_blocking(move || prune_worker_cache_root(&initial)).await.ok();
    let mut capabilities = probe().await?;
    if let Some(policy) = &policy {
        eprintln!("worker policy: {}", policy.summary());
        for warning in &policy.warnings {
            eprintln!("warning: worker policy: {warning}");
        }
        capabilities.policy = Some(policy.clone());
    }
    let (events, _) = broadcast::channel(4096);
    let state = Arc::new(State {
        hub: hub.trim_end_matches('/').into(),
        token,
        data,
        id,
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        capabilities,
        max_jobs,
        slots: Arc::new(Semaphore::new(max_jobs)),
        drain: RwLock::new(()),
        attempts: Mutex::new(HashMap::new()),
        environments: Mutex::new(BTreeMap::new()),
        image_paths: Mutex::new(HashMap::new()),
        caches: Mutex::new(HashSet::new()),
        workspaces: Mutex::new(HashSet::new()),
        events,
        policy,
        metrics,
    });
    load_state(&state).await?;
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(previous_binary(&exe)); // left by the last self-update
    }
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                collect_in_background(&state).await;
            }
        });
    }
    let mut retry = 1;
    loop {
        let started = tokio::time::Instant::now();
        match connection(state.clone()).await {
            // A Hub that refuses the hello closes at once; only a lasting session resets backoff.
            Ok(()) if started.elapsed() > Duration::from_secs(60) => retry = 1,
            Ok(()) => eprintln!(
                "worker connection closed by the Hub; if this repeats, check the Hub log (a worker token is bound to one --id and stops working once revoked); reconnecting in {retry}s"
            ),
            Err(error) => {
                let status = error.downcast_ref::<tokio_tungstenite::tungstenite::Error>().and_then(|error| match error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => Some(response.status()),
                    _ => None,
                });
                match status.map(|status| status.as_u16()) {
                    Some(401) => eprintln!("worker connection: the Hub rejected the token (401); it is wrong or revoked; reconnecting in {retry}s"),
                    Some(403) => eprintln!("worker connection: this token's role cannot connect workers (403); use a worker or admin token; reconnecting in {retry}s"),
                    _ => eprintln!("worker connection: {error:#}; reconnecting in {retry}s"),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(retry)).await;
        retry = (retry * 2).min(30);
    }
}

async fn load_state(state: &Arc<State>) -> Result<()> {
    for entry in fs::read_dir(state.data.join("attempts"))? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let path = dir.join("journal.json");
        if !path.exists() {
            continue;
        }
        let mut journal: Journal = serde_json::from_reader(File::open(&path)?)?;
        identifier(&journal.report.attempt_id)?;
        ensure!(
            dir.file_name().and_then(|s| s.to_str()) == Some(&journal.report.attempt_id),
            "attempt directory identity mismatch"
        );
        let logs = dir.join("logs.jsonl");
        if logs.exists() {
            let mut good_bytes = 0_u64;
            for line in BufReader::new(File::open(&logs)?).split(b'\n') {
                let line = line?;
                match serde_json::from_slice::<LogEntry>(&line) {
                    Ok(log) => {
                        ensure!(
                            log.attempt_id == journal.report.attempt_id
                                && log.job_id == journal.report.job_id,
                            "log identity mismatch"
                        );
                        journal.seq = journal.seq.max(log.seq);
                        good_bytes += line.len() as u64 + 1;
                    }
                    Err(_) => break,
                }
            }
            OpenOptions::new()
                .write(true)
                .open(&logs)?
                .set_len(good_bytes)?;
        }
        if !matches!(
            journal.report.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "unknown"
        ) {
            // A process can outlive its worker. A PID is not enough proof to reattach or kill it safely.
            journal.report.state = "unknown".into();
            journal.report.error = Some("worker restarted before completion; inspect the original process before resubmitting".into());
            atomic_json(&path, &journal)?;
        }
        let (cancel, _) = watch::channel(false);
        state.attempts.lock().unwrap().insert(
            journal.report.attempt_id.clone(),
            Arc::new(Attempt {
                dir,
                journal: Mutex::new(journal),
                cancel,
            }),
        );
    }
    for env in fs::read_dir(state.data.join("environments"))? {
        let path = env?.path();
        if !path.is_dir() {
            continue;
        }
        for revision in fs::read_dir(path)? {
            let path = revision?.path().join("status.json");
            if !path.exists() {
                continue;
            }
            let mut status: EnvironmentStatus = serde_json::from_reader(File::open(&path)?)?;
            identifier(&status.environment_id)?;
            identifier(&status.revision)?;
            if status.state == "preparing" {
                status.state = "failed".into();
                status.error =
                    Some("worker restarted during preparation; explicitly prepare again".into());
                atomic_json(&path, &status)?;
            }
            if status.worker_id == state.id {
                state
                    .environments
                    .lock()
                    .unwrap()
                    .insert(env_key(&status.environment_id, &status.revision), status);
            }
        }
    }
    let statuses: Vec<_> = state
        .environments
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    for status in statuses
        .into_iter()
        .filter(|status| status.state == "ready")
    {
        let dir = state
            .data
            .join("environments")
            .join(&status.environment_id)
            .join(&status.revision);
        let result = async {
            let definition: Environment =
                serde_json::from_reader(File::open(dir.join("definition.json"))?)?;
            // Environments prepared before a policy existed stay usable only if it allows them.
            if let Some(policy) = &state.policy {
                policy.check_environment(&definition).map_err(anyhow::Error::msg)?;
            }
            let image = if definition.image.is_some() {
                Some(resolved_image(state, &definition)?)
            } else {
                None
            };
            let logger = environment_logger(state, &definition)?;
            verify_environment(state, &definition, image.as_deref(), &logger, None).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let mut status = status;
            status.state = "failed".into();
            status.error = Some(format!("startup environment verification: {error:#}"));
            atomic_json(&dir.join("status.json"), &status)?;
            state
                .environments
                .lock()
                .unwrap()
                .insert(env_key(&status.environment_id, &status.revision), status);
        }
    }
    Ok(())
}

async fn connection(state: Arc<State>) -> Result<()> {
    let mut url = reqwest::Url::parse(&state.hub)?;
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(
                    url.host_str(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                )),
        "use HTTPS for non-loopback hubs"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "hub URL must not contain credentials, query or fragment"
    );
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        _ => bail!("hub URL must be HTTP(S)"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid hub URL"))?;
    url.set_path(&format!(
        "{}/v1/worker/ws",
        url.path().trim_end_matches('/')
    ));
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", state.token).parse()?);
    let mut events = state.events.subscribe();
    let (socket, _) = connect_async(request).await?;
    let (mut sink, mut source) = socket.split();
    sink.send(Message::Text(serde_json::to_string(&state.hello())?.into()))
        .await?;
    let attempts: Vec<_> = state.attempts.lock().unwrap().values().cloned().collect();
    for attempt in attempts {
        let logs = attempt.dir.join("logs.jsonl");
        if logs.exists() {
            let file = {
                let _journal = attempt.journal.lock().unwrap();
                let file = File::open(logs)?;
                let size = file.metadata()?.len();
                file.take(size)
            };
            for line in BufReader::new(file).lines() {
                let line = line?;
                // A write cut short (for example by a full disk) must not block every reconnect.
                let Ok(entry) = serde_json::from_str::<LogEntry>(&line) else {
                    eprintln!("skipped unreadable log line in {}", attempt.dir.display());
                    continue;
                };
                sink.send(Message::Text(
                    serde_json::to_string(&WorkerMessage::Log { entry })?.into(),
                ))
                .await?;
            }
        }
        let report = attempt.report();
        if matches!(
            report.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "unknown"
        ) {
            sink.send(Message::Text(
                serde_json::to_string(&WorkerMessage::Finished { report })?.into(),
            ))
            .await?;
        }
    }
    let statuses: Vec<_> = state
        .environments
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    for status in statuses {
        let logs = state
            .data
            .join("environments")
            .join(status.environment_id)
            .join(status.revision)
            .join("logs.jsonl");
        if !logs.exists() {
            continue;
        }
        let file = File::open(logs)?;
        let limit = file.metadata()?.len();
        for line in BufReader::new(file.take(limit)).lines() {
            let line = line?;
            // A live preparation may be in the middle of its final append; its event is already subscribed.
            if let Ok(entry) = serde_json::from_str::<LogEntry>(&line) {
                sink.send(Message::Text(
                    serde_json::to_string(&WorkerMessage::Log { entry })?.into(),
                ))
                .await?;
            }
        }
    }
    // Older Hubs reject unknown messages and drop the connection, so ask before sending samples.
    let mut sampler = (state.metrics && hub_accepts_metrics(&state).await).then(|| Sampler { cpu: cpu_ticks() });
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await; // The first tick is immediate; the CPU baseline needs time to accumulate.
    loop {
        tokio::select! {
            value = source.next() => match value {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<HubMessage>(&text) {
                    Err(error) => eprintln!("ignored unsupported hub message: {error}"),
                    Ok(HubMessage::Run { job, environment }) => accept(state.clone(), *job, environment)?,
                    Ok(HubMessage::Cancel { job_id, attempt_id }) => {
                        let state = state.clone();
                        tokio::spawn(async move { if let Err(error) = request_cancel(&state, &job_id, &attempt_id).await { eprintln!("cancel reconciliation: {error:#}"); } });
                    }
                    Ok(HubMessage::Prepare { environment }) => {
                        let state = state.clone(); tokio::spawn(async move { if let Err(e) = prepare(&state, &environment, true).await { eprintln!("environment preparation: {e:#}"); } });
                    }
                    Ok(HubMessage::Evict { environment_id }) => {
                        let state = state.clone();
                        tokio::spawn(async move { if let Err(e) = evict_environment(&state, &environment_id).await { eprintln!("environment eviction: {e:#}"); } });
                    }
                    Ok(HubMessage::Update { version }) if state.policy.as_ref().is_some_and(|policy| !policy.self_update) => {
                        eprintln!("self-update to {version} refused by worker policy");
                        state.emit(WorkerMessage::UpdateFailed { error: "self-update is disabled by this worker's policy (self_update: false)".into() });
                    }
                    Ok(HubMessage::Update { version }) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(error) = self_update(&state, &version).await {
                                eprintln!("self-update to {version}: {error:#}");
                                state.emit(WorkerMessage::UpdateFailed { error: format!("{error:#}") });
                            }
                        });
                    }
                },
                Some(Ok(Message::Ping(bytes))) => sink.send(Message::Pong(bytes)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(error.into()),
                _ => {},
            },
            event = events.recv() => match event {
                Ok(message) => sink.send(Message::Text(serde_json::to_string(&message)?.into())).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => bail!("event stream lagged; reconnect to replay durable logs"),
                Err(_) => return Ok(()),
            },
            _ = heartbeat.tick() => {
                let message = match sampler.as_mut() {
                    Some(sampler) => WorkerMessage::Metrics { metrics: sampler.sample(&state.data) },
                    None => WorkerMessage::Heartbeat,
                };
                sink.send(Message::Text(serde_json::to_string(&message)?.into())).await?
            }
        }
    }
}

async fn hub_accepts_metrics(state: &State) -> bool {
    let health = async {
        let response = state.client.get(format!("{}/health", state.hub)).send().await?;
        response.error_for_status()?.json::<serde_json::Value>().await
    };
    health.await.is_ok_and(|health| {
        health["capabilities"]
            .as_array()
            .is_some_and(|capabilities| capabilities.iter().any(|c| c == "worker_metrics"))
    })
}

/// CPU usage is the change in cumulative ticks between two samples.
struct Sampler {
    cpu: Option<(u64, u64)>,
}
impl Sampler {
    /// A value the OS cannot report is sent as 0.
    fn sample(&mut self, data: &Path) -> WorkerMetrics {
        let cpu = cpu_ticks();
        let cpu_percent = match (self.cpu, cpu) {
            (Some((busy, total)), Some((busy2, total2))) if total2 > total => {
                busy2.saturating_sub(busy) as f64 * 100.0 / (total2 - total) as f64
            }
            _ => 0.0,
        };
        self.cpu = cpu;
        let (memory_used, memory_total) = memory_usage().unwrap_or_default();
        WorkerMetrics {
            at: 0,
            cpu_percent,
            memory_used,
            memory_total,
            load: load_average().unwrap_or_default(),
            disk_percent: crate::platform::disk_usage(data).map_or(0.0, |used| used * 100.0),
        }
    }
}

/// Cumulative (busy, total) CPU ticks since boot across all cores.
#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<(u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let ticks: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal; guest time is already inside user.
    let total: u64 = ticks.iter().take(8).sum();
    let idle = ticks.get(3)? + ticks.get(4).unwrap_or(&0);
    Some((total - idle, total))
}
#[cfg(target_os = "macos")]
fn cpu_ticks() -> Option<(u64, u64)> {
    let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    #[allow(deprecated)] // libc points to the mach2 crate; the call itself is current.
    let status = unsafe {
        libc::host_statistics(libc::mach_host_self(), libc::HOST_CPU_LOAD_INFO, (&raw mut info).cast(), &mut count)
    };
    (status == libc::KERN_SUCCESS).then(|| {
        let [user, system, idle, nice] = info.cpu_ticks.map(u64::from);
        (user + system + nice, user + system + idle + nice)
    })
}
#[cfg(windows)]
fn cpu_ticks() -> Option<(u64, u64)> {
    windows::cpu_ticks()
}

/// (used, total) physical memory in bytes, counted as the OS's own monitor does.
#[cfg(target_os = "linux")]
fn memory_usage() -> Option<(u64, u64)> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        let line = meminfo.lines().find(|line| line.starts_with(name))?;
        Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    };
    let total = field("MemTotal:")?;
    Some((total.saturating_sub(field("MemAvailable:")?), total))
}
#[cfg(target_os = "macos")]
fn memory_usage() -> Option<(u64, u64)> {
    let mut info: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    #[allow(deprecated)]
    let status = unsafe {
        libc::host_statistics64(libc::mach_host_self(), libc::HOST_VM_INFO64, (&raw mut info).cast(), &mut count)
    };
    if status != libc::KERN_SUCCESS {
        return None;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let mut total = 0u64;
    let mut size = std::mem::size_of::<u64>();
    unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&raw mut total).cast(), &mut size, std::ptr::null_mut(), 0) };
    // Activity Monitor's "Memory Used": app memory + wired + compressed.
    let pages = u64::from(info.internal_page_count).saturating_sub(info.purgeable_count.into())
        + u64::from(info.wire_count)
        + u64::from(info.compressor_page_count);
    Some((pages * page, total))
}
#[cfg(windows)]
fn memory_usage() -> Option<(u64, u64)> {
    windows::memory_usage()
}

#[cfg(target_os = "linux")]
fn load_average() -> Option<f64> {
    fs::read_to_string("/proc/loadavg").ok()?.split_whitespace().next()?.parse().ok()
}
#[cfg(target_os = "macos")]
fn load_average() -> Option<f64> {
    let mut load = 0.0;
    (unsafe { libc::getloadavg(&mut load, 1) } == 1).then_some(load)
}
#[cfg(windows)]
fn load_average() -> Option<f64> {
    None // Windows has no load average.
}

fn accept(state: Arc<State>, job: Job, environment: Option<Environment>) -> Result<()> {
    identifier(&job.id)?;
    identifier(&job.attempt_id)?;
    job.spec.validate()?;
    if let Some(environment) = &environment {
        state.environment_dir(environment)?;
    }
    let mut attempts = state.attempts.lock().unwrap();
    if let Some(existing) = attempts.get(&job.attempt_id) {
        ensure!(
            existing.report().job_id == job.id,
            "attempt belongs to another job"
        );
        let report = existing.report();
        if matches!(
            report.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "unknown"
        ) {
            state.emit(WorkerMessage::Finished { report });
        }
        return Ok(());
    }
    let dir = state.data.join("attempts").join(&job.attempt_id);
    private_dir(&dir)?;
    let report = AttemptReport {
        job_id: job.id.clone(),
        attempt_id: job.attempt_id.clone(),
        state: "assigned".into(),
        exit_code: None,
        error: None,
        results: vec![],
    };
    let journal = Journal {
        report,
        seq: 0,
        pid: None,
        container: None,
        runtime: None,
        windows_job: None,
    };
    atomic_json(&dir.join("journal.json"), &journal)?;
    atomic_json(&dir.join("job.json"), &job.spec)?;
    let (cancel, receiver) = watch::channel(false);
    let attempt = Arc::new(Attempt {
        dir,
        journal: Mutex::new(journal),
        cancel,
    });
    attempts.insert(job.attempt_id.clone(), attempt.clone());
    drop(attempts);
    tokio::spawn(async move {
        let result = run_attempt(
            &state,
            &attempt,
            &job,
            environment.as_ref(),
            receiver.clone(),
        )
        .await;
        let outcome = attempt.update(|journal| {
            let uncertain = result
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<UncertainCleanup>().is_some());
            if !uncertain {
                journal.pid = None;
                journal.container = None;
                journal.runtime = None;
                journal.windows_job = None;
            }
            match result {
                Ok((exit, outputs)) => {
                    journal.report.exit_code = Some(exit);
                    journal.report.results = outputs;
                    journal.report.state = if exit == 0 { "succeeded" } else { "failed" }.into();
                }
                Err(error) => {
                    journal.report.state = if uncertain {
                        "unknown"
                    } else if *receiver.borrow() {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    journal.report.error = Some(format!("{error:#}"));
                }
            }
        });
        match outcome {
            Ok(report) => {
                let finished = report.state != "unknown";
                state.emit(WorkerMessage::Finished { report });
                // Outputs are uploaded and caches moved back; an unknown attempt may still run.
                if finished {
                    let dir = attempt.dir.clone();
                    let _ = tokio::task::spawn_blocking(move || release_attempt_files(&dir)).await;
                }
                collect_in_background(&state).await;
            }
            Err(error) => eprintln!("cannot persist attempt outcome: {error:#}"),
        }
    });
    Ok(())
}

fn cancelled(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(!*cancel.borrow(), "job cancelled");
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_windows_inputs(inputs: &[FileEntry]) -> Result<()> {
    let mut spellings = BTreeMap::new();
    for input in inputs {
        let mut prefix = String::new();
        for component in input.path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if let Some(previous) = spellings.insert(prefix.to_uppercase(), prefix.clone()) {
                ensure!(
                    previous == prefix,
                    "input paths collide on Windows: {previous} and {prefix}"
                );
            }
        }
    }
    Ok(())
}

fn expanded_env(
    environment: Option<&Environment>,
    job: Option<&JobSpec>,
    dir: &Path,
    image_path: Option<&str>,
) -> BTreeMap<String, String> {
    let image = image_path.is_some();
    let env_dir = if image {
        "/ylcc-env".into()
    } else {
        dir.to_string_lossy().into_owned()
    };
    let base_path = image_path
        .map(str::to_owned)
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    let mut values = BTreeMap::new();
    let separator = if cfg!(windows) && !image { ';' } else { ':' };
    values.insert(
        "PATH".into(),
        if base_path.is_empty() {
            format!("{env_dir}/bin")
        } else {
            format!("{env_dir}/bin{separator}{base_path}")
        },
    );
    if image {
        values.insert("HOME".into(), env_dir.clone());
        values.insert("XDG_CACHE_HOME".into(), format!("{env_dir}/cache"));
    }
    for source in [environment.map(|e| &e.env), job.map(|j| &j.env)]
        .into_iter()
        .flatten()
    {
        for (key, value) in source {
            let path = values
                .get("PATH")
                .cloned()
                .unwrap_or_else(|| base_path.clone());
            values.insert(
                key.clone(),
                value
                    .replace("${YLCC_ENV_DIR}", &env_dir)
                    .replace("${PATH}", &path),
            );
        }
    }
    values.insert("YLCC_ENV_DIR".into(), env_dir);
    values
}

fn normalize_image_id(value: &str) -> Result<String> {
    let hash = value.trim().strip_prefix("sha256:").unwrap_or(value.trim());
    validate_hash(hash).context("runtime returned invalid image ID")?;
    Ok(format!("sha256:{hash}"))
}
async fn image_path(state: &State, image: &str) -> Result<String> {
    ensure!(
        image.starts_with("sha256:"),
        "image metadata must be keyed by immutable local image ID"
    );
    if let Some(path) = state.image_paths.lock().unwrap().get(image).cloned() {
        return Ok(path);
    }
    let mut command = Command::new(runtime(state)?);
    command.args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Env}}",
        image,
    ]);
    let values: Option<Vec<String>> = serde_json::from_str(
        &capture(command)
            .await
            .context("inspect actual image PATH")?,
    )?;
    let path = values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.strip_prefix("PATH=").map(str::to_owned))
        .next_back()
        .unwrap_or_default();
    state
        .image_paths
        .lock()
        .unwrap()
        .insert(image.into(), path.clone());
    Ok(path)
}
fn runtime(state: &State) -> Result<&str> {
    state.capabilities.runtimes.first().map(String::as_str).context("no usable Docker or Podman runtime; install/configure one under the worker service account")
}
fn image_arch(arch: &str) -> Result<&str> {
    match arch {
        "aarch64" | "arm64" => Ok("arm64"),
        "x86_64" | "amd64" => Ok("amd64"),
        _ => bail!("unsupported image architecture {arch}"),
    }
}
#[allow(clippy::too_many_arguments)]
fn image_command(
    state: &State,
    image: &str,
    command: &[String],
    dir: &Path,
    env_dir: &Path,
    values: &BTreeMap<String, String>,
    name: &str,
    gpu: Option<&str>,
) -> Result<Command> {
    ensure!(!command.is_empty(), "empty image command");
    let runtime = runtime(state)?;
    let mut process = Command::new(runtime);
    process.args([
        "run",
        "--rm",
        "--init",
        "--name",
        name,
        "--platform",
        &format!("linux/{}", image_arch(&state.capabilities.arch)?),
        "--workdir",
        "/workspace",
    ]);
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        if runtime == "podman" && uid != 0 {
            process.args(["--userns", "keep-id"]);
        }
        process.args(["--user", &format!("{uid}:{gid}")]);
    }
    for (source, target) in [(dir, "/workspace"), (env_dir, "/ylcc-env")] {
        let source = source.to_str().context("non-UTF8 mount path")?;
        ensure!(!source.contains(','), "container bind path contains comma");
        let label = if runtime == "podman" {
            if target == "/workspace" {
                ",relabel=private"
            } else {
                ",relabel=shared"
            }
        } else {
            ""
        };
        process.args([
            "--mount",
            &format!("type=bind,src={source},dst={target}{label}"),
        ]);
    }
    if let Some(gpu) = gpu {
        match (runtime, gpu) {
            ("docker", "cuda") => {
                process.args(["--gpus", "all"]);
            }
            ("podman", "cuda") => {
                process.args(["--device", "nvidia.com/gpu=all"]);
            }
            (_, "rocm") => {
                process.args(["--device", "/dev/kfd", "--device", "/dev/dri"]);
            }
            _ => bail!("GPU backend {gpu} cannot be passed to a Linux container"),
        }
    }
    for (key, value) in values {
        process.args(["--env", &format!("{key}={value}")]);
    }
    process.args(["--entrypoint", &command[0], image]);
    process.args(&command[1..]);
    Ok(process)
}

async fn pipe_logs(
    mut reader: impl AsyncRead + Unpin,
    state: Arc<State>,
    attempt: Arc<Attempt>,
    stream: &'static str,
) -> Result<()> {
    let mut bytes = vec![0; 8192];
    loop {
        let n = reader.read(&mut bytes).await?;
        if n == 0 {
            break;
        }
        attempt.log(
            &state,
            stream,
            String::from_utf8_lossy(&bytes[..n]).into_owned(),
        )?;
    }
    Ok(())
}
#[cfg(unix)]
fn process_group_absent(pgid: u32) -> Result<bool> {
    ensure!(
        pgid > 1 && pgid <= i32::MAX as u32,
        "invalid saved process group"
    );
    #[cfg(unix)]
    {
        if unsafe { libc::kill(-(pgid as i32), 0) } == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(true);
        }
        // Darwin reports EPERM for zombie-only groups until they are reaped; this is not absence.
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EPERM) {
            return Ok(false);
        }
        Err(error).context("cannot inspect process group")
    }
    #[cfg(not(unix))]
    bail!("process group inspection requires Unix")
}
#[cfg(unix)]
fn signal_group(pgid: u32, signal: i32) -> Result<()> {
    ensure!(pgid > 1 && pgid <= i32::MAX as u32, "invalid process group");
    #[cfg(unix)]
    {
        if unsafe { libc::kill(-(pgid as i32), signal) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH)
                && !(cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EPERM))
            {
                return Err(error).context("signal process group");
            }
        }
    }
    Ok(())
}
async fn container_absent(runtime: &str, name: &str) -> Result<bool> {
    let mut inspect = Command::new(runtime);
    inspect.args(["container", "inspect", "--format", "{{.Id}}", name]);
    if capture(inspect).await.is_ok() {
        return Ok(false);
    }
    // A failed inspect can mean a dead daemon. A successful listing is required to prove absence.
    let mut list = Command::new(runtime);
    list.args(["ps", "-a", "--format", "{{.Names}}"]);
    let names = capture(list)
        .await
        .context("container absence cannot be confirmed while the runtime is unavailable")?;
    Ok(!names
        .split(|c: char| c.is_whitespace() || c == ',')
        .any(|value| value == name))
}
async fn request_cancel(state: &Arc<State>, job_id: &str, attempt_id: &str) -> Result<()> {
    let attempt = state.attempts.lock().unwrap().get(attempt_id).cloned();
    let Some(attempt) = attempt else {
        // `accept` journals every attempt before it runs and attempts are never pruned, so an
        // unrecorded attempt never ran here. Confirm the cancel so the Hub releases its slot.
        identifier(job_id)?;
        identifier(attempt_id)?;
        state.emit(WorkerMessage::Finished {
            report: AttemptReport {
                job_id: job_id.into(),
                attempt_id: attempt_id.into(),
                state: "cancelled".into(),
                exit_code: None,
                error: Some("cancelled; this worker never accepted the attempt".into()),
                results: vec![],
            },
        });
        return Ok(());
    };
    ensure!(
        attempt.report().job_id == job_id,
        "cancel job identity mismatch"
    );
    if attempt.cancel.send(true).is_ok() {
        return Ok(());
    }
    let journal = attempt.journal.lock().unwrap().clone();
    if matches!(
        journal.report.state.as_str(),
        "succeeded" | "failed" | "cancelled"
    ) {
        state.emit(WorkerMessage::Finished {
            report: journal.report,
        });
        return Ok(());
    }
    let evidence = async {
        #[cfg(unix)]
        {
            let pgid = journal.pid.context("no process-group evidence was saved; fence the original execution before resolving this attempt")?;
            ensure!(process_group_absent(pgid)?, "saved process group {pgid} still exists; inspect/fence the original execution and retry cancel (saved PIDs are never signalled after restart)");
        }
        #[cfg(windows)]
        {
            let name = journal.windows_job.as_deref().context("no Windows job evidence was saved; fence the original execution before resolving this attempt")?;
            ensure!(windows::absent(name)?, "saved Windows job still has active processes; inspect/fence the original execution and retry cancel (saved PIDs are never signalled after restart)");
        }
        if let Some(name) = &journal.container {
            let runtime = journal.runtime.as_deref().context("container runtime was not saved; inspect/fence the original container before resolving this attempt")?;
            ensure!(container_absent(runtime, name).await?, "container {name} still exists; stop/remove the original container and retry cancel");
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    let report = attempt.update(|journal| match &evidence {
        Ok(()) => {
            journal.report.state = "cancelled".into();
            journal.report.error = Some(
                "cancelled after confirming the original execution and container are absent".into(),
            );
            journal.pid = None;
            journal.container = None;
            journal.runtime = None;
            journal.windows_job = None;
        }
        Err(error) => {
            journal.report.state = "unknown".into();
            journal.report.error = Some(format!("cancellation remains unresolved: {error:#}"));
        }
    })?;
    state.emit(WorkerMessage::Finished { report });
    Ok(())
}
/// Time between SIGTERM and SIGKILL, so a cancelled or timed-out command's traps can clean up.
#[cfg(unix)]
const STOP_GRACE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 30 });
#[cfg(unix)]
async fn stop_process(
    child: &mut tokio::process::Child,
    pgid: u32,
    container: Option<(&str, &str)>,
) -> Result<()> {
    let result = async {
        signal_group(pgid, libc::SIGTERM)?;
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        loop {
            let _ = child.try_wait()?;
            if process_group_absent(pgid)? {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // The leader may already have exited. The saved group still owns all remaining children.
                signal_group(pgid, libc::SIGKILL)?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .context("process leader did not stop")??;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !process_group_absent(pgid)? {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "process group {pgid} remains after SIGKILL"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let container_result = async {
        if let Some((runtime, name)) = container {
            let mut cleanup = Command::new(runtime);
            cleanup
                .args(["rm", "-f", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let _ = tokio::time::timeout(Duration::from_secs(15), cleanup.status()).await;
            ensure!(
                container_absent(runtime, name).await?,
                "container {name} remains after cleanup"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    result.and(container_result).map_err(|error| {
        anyhow::Error::new(UncertainCleanup(format!(
            "execution cleanup could not be confirmed: {error:#}"
        )))
    })
}
#[cfg(windows)]
async fn stop_process(
    child: &mut tokio::process::Child,
    job: &windows::Job,
    container: Option<(&str, &str)>,
) -> Result<()> {
    let execution = async {
        job.terminate()?;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("command process did not stop")??;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while job.active()? != 0 {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Windows job still has active descendants after termination"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let container_result = async {
        if let Some((runtime, name)) = container {
            let mut cleanup = Command::new(runtime);
            cleanup
                .args(["rm", "-f", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let _ = tokio::time::timeout(Duration::from_secs(15), cleanup.status()).await;
            ensure!(
                Box::pin(container_absent(runtime, name)).await?,
                "container {name} remains after cleanup"
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    execution.and(container_result).map_err(|error| {
        anyhow::Error::new(UncertainCleanup(format!(
            "execution cleanup could not be confirmed: {error:#}"
        )))
    })
}

async fn execute(
    state: &Arc<State>,
    attempt: &Arc<Attempt>,
    mut command: Command,
    mut cancel: watch::Receiver<bool>,
    timeout: u64,
    container: Option<(String, String)>,
) -> Result<i32> {
    cancelled(&cancel)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(unix)]
    let mut child = command.spawn().context("start command")?;
    #[cfg(windows)]
    let (mut child, windows_job, suspended_thread) = windows::spawn(&mut command).await?;
    let pgid = child.id().context("spawned process has no PID")?;
    if let Err(error) = attempt.update(|journal| {
        journal.pid = Some(pgid);
        journal.container = container.as_ref().map(|(_, name)| name.clone());
        journal.runtime = container.as_ref().map(|(runtime, _)| runtime.clone());
        #[cfg(windows)]
        {
            journal.windows_job = Some(windows_job.name.clone());
        }
    }) {
        stop_process(
            &mut child,
            #[cfg(unix)]
            pgid,
            #[cfg(windows)]
            &windows_job,
            container.as_ref().map(|(r, n)| (r.as_str(), n.as_str())),
        )
        .await?;
        return Err(error);
    }
    #[cfg(windows)]
    if let Err(error) = suspended_thread.resume() {
        stop_process(
            &mut child,
            &windows_job,
            container.as_ref().map(|(r, n)| (r.as_str(), n.as_str())),
        )
        .await?;
        return Err(error);
    }
    let stdout = tokio::spawn(pipe_logs(
        child.stdout.take().unwrap(),
        state.clone(),
        attempt.clone(),
        "stdout",
    ));
    let stderr = tokio::spawn(pipe_logs(
        child.stderr.take().unwrap(),
        state.clone(),
        attempt.clone(),
        "stderr",
    ));
    let outcome = tokio::select! {
        result = child.wait() => result.map_err(anyhow::Error::from),
        _ = cancel.changed() => Err(anyhow::anyhow!("job cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(timeout)) => Err(anyhow::anyhow!("command exceeded timeout of {timeout}s")),
    };
    let cleanup = stop_process(
        &mut child,
        #[cfg(unix)]
        pgid,
        #[cfg(windows)]
        &windows_job,
        container.as_ref().map(|(r, n)| (r.as_str(), n.as_str())),
    )
    .await;
    let logs = async {
        stdout.await??;
        stderr.await??;
        Ok::<_, anyhow::Error>(())
    };
    let logs = tokio::time::timeout(Duration::from_secs(5), logs)
        .await
        .context("command descendants retained output pipes");
    cleanup?;
    logs??;
    let status = outcome?;
    Ok(status.code().unwrap_or(128 + signal_number(&status)))
}
fn signal_number(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        0
    }
}

async fn run_attempt(
    state: &Arc<State>,
    attempt: &Arc<Attempt>,
    job: &Job,
    environment: Option<&Environment>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(i32, Vec<FileEntry>)> {
    cancelled(&cancel)?;
    // Before any preparation, pull or input download: a refused job touches nothing.
    if let Some(policy) = &state.policy {
        policy.check_job(&job.spec, environment).map_err(anyhow::Error::msg)?;
        if let Some(environment) = environment {
            // A ready environment ran the setup in its definition.json; that is what was checked.
            let definition = state.environment_dir(environment)?.join("definition.json");
            if definition.exists() {
                let existing: Environment = serde_json::from_reader(File::open(&definition)?)?;
                ensure!(
                    serde_json::to_value(&existing)? == serde_json::to_value(environment)?,
                    "prepared environment {} differs from the definition the Hub sent; delete the environment and prepare it again",
                    environment.id
                );
            }
        }
    }
    for path in job
        .spec
        .inputs
        .iter()
        .map(|input| input.path.as_str())
        .chain(job.spec.outputs.iter().map(String::as_str))
    {
        crate::platform::filesystem_relative(path)?;
    }
    #[cfg(windows)]
    validate_windows_inputs(&job.spec.inputs)?;
    if let Some(environment) = environment {
        ensure!(
            environment.image.is_some() || job.spec.container.is_none(),
            "native environments cannot be combined with an ad-hoc container; register an image environment instead"
        );
        let ready = state
            .environments
            .lock()
            .unwrap()
            .get(&env_key(&environment.id, &environment.revision))
            .is_some_and(|e| e.state == "ready");
        if !ready {
            prepare(state, environment, false).await?;
        }
    }
    let (_slot, _drain) = loop {
        let slot = tokio::select! { slot = state.slots.acquire() => slot?, _ = cancel.changed() => bail!("job cancelled") };
        let drain = tokio::select! { guard = state.drain.read() => guard, _ = cancel.changed() => bail!("job cancelled") };
        // Garbage collection may have evicted the environment while this attempt waited.
        match environment {
            Some(environment) if !state.environments.lock().unwrap().contains_key(&env_key(&environment.id, &environment.revision)) => {
                drop((drain, slot));
                prepare(state, environment, false).await?;
            }
            _ => break (slot, drain),
        }
    };
    cancelled(&cancel)?;
    let env_dir = if let Some(environment) = environment {
        state.environment_dir(environment)?
    } else {
        attempt.dir.join("environment")
    };
    private_dir(&env_dir)?;
    let mut image = if let Some(environment) = environment.filter(|e| e.image.is_some()) {
        Some(resolved_image(state, environment)?)
    } else {
        job.spec.container.clone()
    };
    if environment.is_none()
        && let Some(reference) = &image
    {
        image = Some(resolve_job_image(state, attempt, reference, cancel.clone()).await?);
    }
    if let Some(environment) = environment {
        let ready = state
            .environments
            .lock()
            .unwrap()
            .get(&env_key(&environment.id, &environment.revision))
            .is_some_and(|status| status.state == "ready");
        ensure!(
            ready,
            "environment is no longer ready; inspect its status and explicitly prepare it again"
        );
        if let Err(error) = verify_environment(
            state,
            environment,
            image.as_deref(),
            attempt,
            Some(cancel.clone()),
        )
        .await
        {
            if !*cancel.borrow() {
                state.environment_status(
                    environment,
                    "failed",
                    Some(format!("pre-job environment verification: {error:#}")),
                )?;
            }
            return Err(error).context("environment drift detected before execution");
        }
        let _ = fs::write(env_dir.join(LAST_USED), b"");
    }
    // Declared before the cache lease so caches are moved back before the checkout is removed.
    let workspace = Workspace::claim(state, &job.spec, &attempt.dir)?;
    let workdir = workspace.dir.clone();
    fs::create_dir(&workdir).context("create fresh attempt checkout")?;
    for input in &job.spec.inputs {
        cancelled(&cancel)?;
        let blob = ensure_blob(state, &input.hash, Some(input.size)).await?;
        let dest = workdir.join(&input.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut destination = crate::platform::create_private_file(&dest)?;
        std::io::copy(&mut File::open(blob)?, &mut destination)?;
        crate::platform::set_mode(&destination, input.mode)?;
    }
    let path = if let Some(image) = &image {
        Some(image_path(state, image).await?)
    } else {
        None
    };
    let mut values = expanded_env(environment, Some(&job.spec), &env_dir, path.as_deref());
    values.insert("YLCC_JOB_ID".into(), job.id.clone());
    values.insert("YLCC_ATTEMPT_ID".into(), job.attempt_id.clone());
    if image.is_none() {
        // A per-attempt temp directory is removed with the checkout, like a CI runner's temp.
        let temp = attempt.dir.join("tmp");
        private_dir(&temp)?;
        let temp = temp.to_string_lossy().into_owned();
        for name in ["TMPDIR", "TMP", "TEMP"] {
            values.insert(name.into(), temp.clone());
        }
    }
    if let Some(image) = image.as_deref() {
        ensure!(
            job.spec
                .requirements
                .os
                .as_deref()
                .is_none_or(|os| os == "linux"),
            "containers execute Linux, not the native host OS"
        );
        ensure!(
            job.spec
                .requirements
                .arch
                .as_deref()
                .is_none_or(|arch| normalize_arch(arch) == state.capabilities.arch),
            "image execution architecture mismatch"
        );
        if environment.is_none() {
            ensure!(
                job.spec.requirements.tools.is_empty()
                    && job.spec.requirements.abi.is_none()
                    && job.spec.requirements.build_target.is_none(),
                "image tool/ABI/target requirements need a verified environment"
            );
        }
        let context = Some((&**state, image, workdir.as_path(), env_dir.as_path()));
        verify_tools(&job.spec.requirements, &values, context).await?;
        if !job.spec.requirements.cpu_features.is_empty() {
            let command = image_command(
                state,
                image,
                &["cat".into(), "/proc/cpuinfo".into()],
                &workdir,
                &env_dir,
                &values,
                &format!("ylcc-cpu-{}", uuid::Uuid::new_v4()),
                None,
            )?;
            let text = capture(command).await?;
            let flags: Vec<_> = text
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("flags")
                        .or_else(|| line.strip_prefix("Features"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, rest)| rest)
                })
                .flat_map(str::split_whitespace)
                .collect();
            ensure!(
                job.spec
                    .requirements
                    .cpu_features
                    .iter()
                    .all(|feature| flags.contains(&feature.as_str())),
                "CPU features unavailable in actual container runtime"
            );
        }
        verify_container_mount(state, image, &workdir, &env_dir, &values).await?;
        if let Some(gpu) = &job.spec.requirements.gpu {
            verify_image_gpu(state, image, &workdir, &env_dir, &values, gpu).await?;
        }
    } else {
        verify_tools(&job.spec.requirements, &values, None).await?;
        let mut base = job.spec.requirements.clone();
        base.tools.clear();
        base.build_target = None;
        ensure!(
            state.capabilities.mismatch(&base).is_empty(),
            "worker no longer satisfies requirements"
        );
    }
    cancelled(&cancel)?;
    // Held until outputs are uploaded, so outputs inside a cached directory are collected first.
    let _cache = CacheLease::claim(state, &job.spec, &workdir)?;
    let mut container = None;
    let command = if let Some(image) = image.as_ref() {
        let name = format!("ylcc-{}", job.attempt_id);
        let process = image_command(
            state,
            image,
            &job.spec.command,
            &workdir,
            &env_dir,
            &values,
            &name,
            job.spec.requirements.gpu.as_deref(),
        )?;
        container = Some((runtime(state)?.into(), name));
        process
    } else {
        let mut process = Command::new(&job.spec.command[0]);
        process
            .args(&job.spec.command[1..])
            .current_dir(&workdir)
            .envs(&values);
        process
    };
    attempt.update(|journal| journal.report.state = "running".into())?;
    state.emit(WorkerMessage::Started {
        job_id: job.id.clone(),
        attempt_id: job.attempt_id.clone(),
    });
    let exit = execute(
        state,
        attempt,
        command,
        cancel.clone(),
        job.spec.timeout_secs.unwrap_or(24 * 60 * 60),
        container,
    )
    .await;
    if exit
        .as_ref()
        .is_err_and(|error| error.downcast_ref::<UncertainCleanup>().is_some())
    {
        return exit.map(|exit| (exit, vec![])); // The command may still be writing its outputs.
    }
    let exit = exit.and_then(|exit| cancelled(&cancel).map(|()| exit));
    let uploaded = upload_outputs(
        state,
        &workdir,
        &job.spec.outputs,
        exit.as_ref().is_ok_and(|e| *e == 0),
    )
    .await;
    match exit {
        Ok(exit) => Ok((exit, uploaded?)),
        // Outputs written before a failure, timeout or cancellation still help diagnose it.
        Err(error) => {
            match uploaded {
                Ok(results) => {
                    attempt.update(|journal| journal.report.results = results)?;
                }
                Err(upload) => attempt.log(
                    state,
                    "system",
                    format!("outputs not collected: {upload:#}\n"),
                )?,
            }
            Err(error)
        }
    }
}

/// Uploads declared outputs; unless `required`, a missing output is skipped.
async fn upload_outputs(
    state: &State,
    workdir: &Path,
    outputs: &[String],
    required: bool,
) -> Result<Vec<FileEntry>> {
    let mut files = BTreeMap::new();
    for output in outputs {
        if !required
            && fs::symlink_metadata(workdir.join(output))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            continue;
        }
        collect_output(workdir, &workdir.join(output), &mut files)?;
    }
    let mut results = vec![];
    for (_, entry) in files {
        upload_blob(state, &entry.hash, &workdir.join(&entry.path)).await?;
        results.push(entry);
    }
    Ok(results)
}

/// Tasks with caches check out into a fixed directory per cache scope, as CI runners do:
/// build outputs such as Cargo's `target` embed absolute paths and break under a new path.
/// The directory is still fresh for every attempt; an attempt that finds it busy uses its own.
struct Workspace<'a> {
    state: &'a State,
    key: Option<String>,
    dir: PathBuf,
}

impl<'a> Workspace<'a> {
    fn claim(state: &'a State, spec: &JobSpec, attempt: &Path) -> Result<Self> {
        let mut workspace = Self { state, key: None, dir: attempt.join("work") };
        let Some(scope) = spec.cache_scope.as_deref().filter(|_| !spec.cache.is_empty()) else {
            return Ok(workspace);
        };
        let key = hex::encode(Sha256::digest(format!(
            "{scope}\0{}\0{}",
            spec.environment.as_deref().unwrap_or_default(),
            spec.container.as_deref().unwrap_or_default()
        )))[..16]
            .to_owned();
        if !state.workspaces.lock().unwrap().insert(key.clone()) {
            eprintln!("workspace {key} is in use by another attempt; using the attempt directory");
            return Ok(workspace);
        }
        workspace.dir = state.data.join("workspaces").join(&key);
        workspace.key = Some(key);
        remove_tree(&workspace.dir)?; // left behind by a crash
        Ok(workspace)
    }
}

impl Drop for Workspace<'_> {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            if let Err(error) = remove_tree(&self.dir) {
                eprintln!("workspace cleanup: {error:#}");
            }
            self.state.workspaces.lock().unwrap().remove(key);
        }
    }
}

/// Moves persistent cache directories into a fresh checkout and back afterwards.
/// A key already in use by another attempt runs without its cache instead of waiting.
struct CacheLease<'a> {
    state: &'a State,
    workdir: PathBuf,
    entries: Vec<(String, String)>,
}

impl<'a> CacheLease<'a> {
    fn claim(state: &'a State, spec: &JobSpec, workdir: &Path) -> Result<Self> {
        let mut lease = Self {
            state,
            workdir: workdir.into(),
            entries: Vec::new(),
        };
        let Some(scope) = spec.cache_scope.as_deref() else {
            return Ok(lease);
        };
        for path in &spec.cache {
            crate::platform::filesystem_relative(path)?;
            if fs::symlink_metadata(workdir.join(path)).is_ok() {
                continue; // The submitted inputs already provide this path.
            }
            // A different toolchain or image never reuses another's dependency or build output.
            // "fixed" marks caches built in a fixed workspace; earlier ones embed attempt paths.
            let key = hex::encode(Sha256::digest(format!(
                "fixed\0{scope}\0{}\0{}\0{path}",
                spec.environment.as_deref().unwrap_or_default(),
                spec.container.as_deref().unwrap_or_default()
            )));
            if !state.caches.lock().unwrap().insert(key.clone()) {
                eprintln!("cache {path} is in use by another attempt; running without it");
                continue;
            }
            lease.entries.push((key.clone(), path.clone()));
            let stored = state.data.join("caches").join(&key);
            if stored.exists() {
                let work = workdir.join(path);
                if let Some(parent) = work.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&stored, &work).with_context(|| format!("restore cache {path}"))?;
            }
        }
        Ok(lease)
    }
}

impl Drop for CacheLease<'_> {
    fn drop(&mut self) {
        for (key, path) in &self.entries {
            // Only keep a real directory reached without symlinks, so a job cannot redirect the move.
            let mut current = self.workdir.clone();
            let real = path.split('/').all(|part| {
                current.push(part);
                fs::symlink_metadata(&current)
                    .is_ok_and(|m| m.is_dir() && !crate::platform::is_link(&m))
            });
            let stored = self.state.data.join("caches").join(key);
            if real
                && !stored.exists()
                && let Err(error) = fs::rename(&current, &stored)
            {
                eprintln!("save cache {path}: {error}");
            }
            let _ = fs::write(cache_marker(&stored), b"");
            self.state.caches.lock().unwrap().remove(key);
        }
    }
}

async fn resolve_job_image(
    state: &Arc<State>,
    attempt: &Arc<Attempt>,
    image: &str,
    cancel: watch::Receiver<bool>,
) -> Result<String> {
    let runtime = runtime(state)?;
    let inspect = || {
        let mut command = Command::new(runtime);
        command.args(["image", "inspect", "--format", "{{.Id}}", image]);
        command
    };
    let id = match capture(inspect()).await {
        Ok(id) => id,
        Err(_) => {
            let mut pull = Command::new(runtime);
            pull.args([
                "pull",
                "--platform",
                &format!("linux/{}", image_arch(&state.capabilities.arch)?),
                image,
            ]);
            ensure!(
                execute(state, attempt, pull, cancel, 3600, None).await? == 0,
                "image pull failed"
            );
            capture(inspect()).await?
        }
    };
    normalize_image_id(&id)
}

async fn verify_container_mount(
    state: &State,
    image: &str,
    workdir: &Path,
    envdir: &Path,
    values: &BTreeMap<String, String>,
) -> Result<()> {
    let sentinel = format!(".ylcc-mount-{}", uuid::Uuid::new_v4());
    let content = uuid::Uuid::new_v4().to_string();
    fs::write(workdir.join(&sentinel), &content)?;
    let result = async {
        let command = image_command(state, image, &["cat".into(), format!("/workspace/{sentinel}")], workdir, envdir, values, &format!("ylcc-mount-{}", uuid::Uuid::new_v4()), None)?;
        ensure!(capture(command).await? == content, "container bind does not reference the worker checkout; use identical absolute paths on worker and runtime host"); Ok::<_,anyhow::Error>(())
    }.await;
    fs::remove_file(workdir.join(sentinel))?;
    result
}

async fn ensure_blob(state: &State, hash: &str, size: Option<u64>) -> Result<PathBuf> {
    validate_hash(hash)?;
    let path = state.data.join("blobs").join(hash);
    if path.exists() {
        verify_file(&path, hash, size)?;
        return Ok(path);
    }
    let response = state
        .client
        .get(format!("{}/v1/blobs/{hash}", state.hub))
        .bearer_auth(&state.token)
        .send()
        .await?
        .error_for_status()?;
    let temp = path.with_extension(format!("{}.part", uuid::Uuid::new_v4()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        let mut stream = response.bytes_stream();
        let mut digest = Sha256::new();
        let mut written = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            written += chunk.len() as u64;
            ensure!(
                size.is_none_or(|size| written <= size),
                "blob exceeds declared size"
            );
            digest.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        ensure!(
            hex::encode(digest.finalize()) == hash,
            "download SHA-256 mismatch"
        );
        ensure!(
            size.is_none_or(|size| written == size),
            "download size mismatch"
        );
        tokio::fs::rename(&temp, &path).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result?;
    Ok(path)
}
fn verify_file(path: &Path, expected: &str, size: Option<u64>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !crate::platform::is_link(&metadata),
        "blob is not a regular file"
    );
    ensure!(
        size.is_none_or(|size| size == metadata.len()),
        "blob size mismatch"
    );
    ensure!(file_hash(path)? == expected, "cached blob SHA-256 mismatch");
    Ok(())
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let size = file.read(&mut bytes)?;
        if size == 0 {
            break;
        }
        hash.update(&bytes[..size]);
    }
    Ok(hex::encode(hash.finalize()))
}
async fn upload_blob(state: &State, hash: &str, path: &Path) -> Result<()> {
    verify_file(path, hash, None)?;
    let file = tokio::fs::File::open(path).await?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    state
        .client
        .put(format!("{}/v1/blobs/{hash}", state.hub))
        .bearer_auth(&state.token)
        .body(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
fn collect_output(
    root: &Path,
    path: &Path,
    result: &mut BTreeMap<String, FileEntry>,
) -> Result<()> {
    let relative = path.strip_prefix(root)?;
    let wire_path = crate::platform::relative_path(relative)?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        cursor.push(part);
        ensure!(
            !crate::platform::is_link(&fs::symlink_metadata(&cursor)?),
            "output contains symlink: {}",
            relative.display()
        );
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        children.sort();
        for child in children {
            collect_output(root, &child, result)?;
        }
    } else {
        ensure!(metadata.is_file(), "output is not a regular file");
        let mode = crate::platform::mode(&metadata);
        let path_string = wire_path;
        result.insert(
            path_string.clone(),
            FileEntry {
                path: path_string,
                hash: file_hash(path)?,
                size: metadata.len(),
                mode,
            },
        );
    }
    Ok(())
}

async fn capture(mut command: Command) -> Result<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(not(windows))]
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .context("probe timed out")??;
    #[cfg(windows)]
    let output = {
        let (mut child, job, thread) = windows::spawn(&mut command).await?;
        if let Err(error) = thread.resume() {
            stop_process(&mut child, &job, None).await?;
            return Err(error);
        }
        let stdout = child.stdout.take().context("probe stdout unavailable")?;
        let stderr = child.stderr.take().context("probe stderr unavailable")?;
        let read_pipe = |mut pipe: tokio::process::ChildStdout| async move {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let stdout = tokio::spawn(read_pipe(stdout));
        let stderr = tokio::spawn(async move {
            let mut pipe = stderr;
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        });
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
        let cleanup = stop_process(&mut child, &job, None).await;
        cleanup?;
        let pipes = tokio::time::timeout(Duration::from_secs(5), async {
            Ok::<_, anyhow::Error>((stdout.await??, stderr.await??))
        })
        .await
        .context("probe descendants retained output pipes")??;
        std::process::Output {
            status: status.context("probe timed out")??,
            stdout: pipes.0,
            stderr: pipes.1,
        }
    };
    ensure!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        Ok(String::from_utf8_lossy(&output.stderr).trim().into())
    } else {
        Ok(stdout.trim().into())
    }
}

fn native_recipe(script: &str) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
        command.arg(format!("$ErrorActionPreference = 'Stop'; & {{ {script}\n}}; if (-not $?) {{ exit 1 }}; if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}"));
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command.args(["-eu", "-c", script]);
        command
    }
}
fn version_token(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            word.trim_start_matches('v')
                .trim_matches(|c: char| c == '(' || c == ')' || c == ',' || c == '"')
        })
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()) && word.contains('.'))
        .unwrap_or(value.lines().next().unwrap_or(""))
        .into()
}
async fn tool_version(
    tool: &str,
    values: &BTreeMap<String, String>,
    image: Option<(&State, &str, &Path, &Path)>,
) -> Result<String> {
    if let Some(sdk) = tool.strip_prefix("sdk:") {
        ensure!(image.is_none(), "Apple SDK requirements need native macOS");
        ensure!(
            !sdk.is_empty()
                && sdk
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid SDK name"
        );
        let mut command = Command::new("xcrun");
        command
            .args(["--sdk", sdk, "--show-sdk-version"])
            .envs(values);
        return capture(command).await;
    }
    ensure!(
        !tool.is_empty()
            && tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_+.-".contains(&b)),
        "invalid tool executable name"
    );
    let arg = match tool {
        "xcodebuild" | "java" | "javac" | "emulator" => "-version",
        "adb" => "version",
        _ => "--version",
    };
    let command = if let Some((state, image, workdir, envdir)) = image {
        image_command(
            state,
            image,
            &[tool.into(), arg.into()],
            workdir,
            envdir,
            values,
            &format!("ylcc-probe-{}", uuid::Uuid::new_v4()),
            None,
        )?
    } else {
        let executable = if cfg!(windows) {
            match tool {
                "npm" => "npm.cmd",
                "gradle" => "gradle.bat",
                _ => tool,
            }
        } else {
            tool
        };
        let mut process = Command::new(executable);
        if tool == "powershell" {
            process.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ]);
        } else {
            process.arg(arg);
        }
        process.envs(values);
        process
    };
    let text = capture(command).await?;
    ensure!(!text.is_empty(), "{tool} returned no version");
    Ok(version_token(&text))
}
async fn verify_tools(
    requirements: &Requirements,
    values: &BTreeMap<String, String>,
    image: Option<(&State, &str, &Path, &Path)>,
) -> Result<()> {
    for (tool, expected) in &requirements.tools {
        let actual = tool_version(tool, values, image)
            .await
            .with_context(|| format!("required tool {tool}"))?;
        ensure!(
            expected == "*" || expected == &actual,
            "required {tool} {expected}, found {actual}"
        );
    }
    if let Some(target) = &requirements.build_target {
        let command = if let Some((state, image, workdir, envdir)) = image {
            image_command(
                state,
                image,
                &["rustc".into(), "-vV".into()],
                workdir,
                envdir,
                values,
                &format!("ylcc-target-{}", uuid::Uuid::new_v4()),
                None,
            )?
        } else {
            let mut process = Command::new("rustc");
            process.arg("-vV").envs(values);
            process
        };
        let text = capture(command).await?;
        ensure!(
            text.lines()
                .any(|line| line.strip_prefix("host: ") == Some(target.as_str())),
            "build target {target} lacks a verified native linker/SDK; cross-compilation is not advertised"
        );
        if let Some((state, image, workdir, envdir)) = image {
            let prefix = format!("/ylcc-env/.target-probe-{}", uuid::Uuid::new_v4());
            let script = format!(
                "trap 'rm -f {prefix}.rs {prefix}' EXIT; printf 'fn main() {{}}\\n' > {prefix}.rs; rustc --crate-name ylcc_probe {prefix}.rs -o {prefix}; {prefix}"
            );
            let command = image_command(
                state,
                image,
                &["sh".into(), "-eu".into(), "-c".into(), script],
                workdir,
                envdir,
                values,
                &format!("ylcc-link-{}", uuid::Uuid::new_v4()),
                None,
            )?;
            capture(command)
                .await
                .context("image target compiler/linker execution probe")?;
        } else {
            let dir = tempfile::tempdir()?;
            let source = dir.path().join("probe.rs");
            let binary = dir
                .path()
                .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
            fs::write(&source, "fn main() {}\n")?;
            let mut compiler = Command::new("rustc");
            compiler.arg(source).arg("-o").arg(&binary).envs(values);
            capture(compiler)
                .await
                .context("target compiler/linker probe")?;
            let mut command = Command::new(binary);
            command.envs(values);
            capture(command)
                .await
                .context("native target execution probe")?;
        }
    }
    Ok(())
}
fn resolved_image(state: &State, environment: &Environment) -> Result<String> {
    serde_json::from_reader(File::open(
        state.environment_dir(environment)?.join("image.json"),
    )?)
    .context("environment has no verified local image; prepare it first")
}
fn environment_logger(state: &State, environment: &Environment) -> Result<Arc<Attempt>> {
    let dir = state.environment_dir(environment)?;
    private_dir(&dir)?;
    let seq = if dir.join("logs.jsonl").exists() {
        BufReader::new(File::open(dir.join("logs.jsonl"))?)
            .lines()
            .filter_map(|line| {
                line.ok()
                    .and_then(|line| serde_json::from_str::<LogEntry>(&line).ok())
            })
            .map(|entry| entry.seq)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let id = format!("environment:{}:{}", environment.id, environment.revision);
    let attempt_id = format!(
        "environment:{}:{}:{}",
        state.id, environment.id, environment.revision
    );
    let (cancel, _) = watch::channel(false);
    Ok(Arc::new(Attempt {
        dir,
        cancel,
        journal: Mutex::new(Journal {
            report: AttemptReport {
                job_id: id,
                attempt_id,
                state: "preparing".into(),
                exit_code: None,
                error: None,
                results: vec![],
            },
            seq,
            pid: None,
            container: None,
            runtime: None,
            windows_job: None,
        }),
    }))
}
async fn verify_environment(
    state: &Arc<State>,
    environment: &Environment,
    image: Option<&str>,
    logger: &Arc<Attempt>,
    cancel: Option<watch::Receiver<bool>>,
) -> Result<()> {
    let dir = state.environment_dir(environment)?;
    let path = if let Some(image) = image {
        Some(image_path(state, image).await?)
    } else {
        None
    };
    let values = expanded_env(Some(environment), None, &dir, path.as_deref());
    let context = image.map(|image| (&**state, image, dir.as_path(), dir.as_path()));
    verify_tools(&environment.requirements, &values, context).await?;
    if let Some(image) = image {
        if let Some(expected) = &environment.requirements.abi {
            let script = "if getconf GNU_LIBC_VERSION >/dev/null 2>&1; then printf gnu; elif ldd --version 2>&1 | grep -qi musl; then printf musl; else exit 1; fi";
            let command = image_command(
                state,
                image,
                &["sh".into(), "-c".into(), script.into()],
                &dir,
                &dir,
                &values,
                &format!("ylcc-abi-{}", uuid::Uuid::new_v4()),
                None,
            )?;
            let actual = capture(command).await?;
            ensure!(
                &actual == expected,
                "image ABI is {actual}, expected {expected}"
            );
        }
        if let Some(gpu) = &environment.requirements.gpu {
            verify_image_gpu(state, image, &dir, &dir, &values, gpu).await?;
        }
    }
    for script in &environment.verify {
        logger.log(state, "system", "verifying environment\n".into())?;
        let (keepalive, receiver) = watch::channel(false);
        let receiver = cancel.clone().unwrap_or(receiver);
        let mut container = None;
        let command = if let Some(image) = image {
            let name = format!("ylcc-verify-{}", uuid::Uuid::new_v4());
            let command = image_command(
                state,
                image,
                &["sh".into(), "-eu".into(), "-c".into(), script.clone()],
                &dir,
                &dir,
                &values,
                &name,
                environment.requirements.gpu.as_deref(),
            )?;
            container = Some((runtime(state)?.into(), name));
            command
        } else {
            let mut command = native_recipe(script);
            command.current_dir(&dir).envs(&values);
            command
        };
        let result = execute(state, logger, command, receiver, 600, container).await;
        drop(keepalive);
        ensure!(result? == 0, "environment verification failed");
    }
    Ok(())
}

async fn verify_image_gpu(
    state: &State,
    image: &str,
    workdir: &Path,
    envdir: &Path,
    values: &BTreeMap<String, String>,
    backend: &str,
) -> Result<()> {
    let script = match backend {
        "cuda" => {
            r#"import ctypes as c
l=c.CDLL('libcuda.so.1')
assert l.cuInit(0)==0
n=c.c_int(); assert l.cuDeviceGetCount(c.byref(n))==0 and n.value>0
ctx=c.c_void_p(); l.cuCtxCreate_v2.argtypes=[c.POINTER(c.c_void_p),c.c_uint,c.c_int]
assert l.cuCtxCreate_v2(c.byref(ctx),0,0)==0
p=c.c_uint64(); l.cuMemAlloc_v2.argtypes=[c.POINTER(c.c_uint64),c.c_size_t]
assert l.cuMemAlloc_v2(c.byref(p),4)==0
l.cuMemFree_v2.argtypes=[c.c_uint64]; assert l.cuMemFree_v2(p)==0
l.cuCtxDestroy_v2.argtypes=[c.c_void_p]; assert l.cuCtxDestroy_v2(ctx)==0
print('ready')"#
        }
        "rocm" => {
            r#"import ctypes as c
l=c.CDLL('libamdhip64.so')
assert l.hipInit(0)==0
n=c.c_int(); assert l.hipGetDeviceCount(c.byref(n))==0 and n.value>0
p=c.c_void_p(); l.hipMalloc.argtypes=[c.POINTER(c.c_void_p),c.c_size_t]
assert l.hipMalloc(c.byref(p),4)==0
l.hipFree.argtypes=[c.c_void_p]; assert l.hipFree(p)==0
print('ready')"#
        }
        _ => bail!("GPU backend {backend} is unavailable in Linux containers"),
    };
    let command = image_command(
        state,
        image,
        &["python3".into(), "-c".into(), script.into()],
        workdir,
        envdir,
        values,
        &format!("ylcc-gpu-{}", uuid::Uuid::new_v4()),
        Some(backend),
    )?;
    ensure!(capture(command).await.context("image GPU access probe requires python3 and usable driver libraries/devices in the actual image")? == "ready", "image GPU access verification failed");
    Ok(())
}
async fn prepare(state: &Arc<State>, environment: &Environment, explicit: bool) -> Result<()> {
    let _drain = state.drain.write().await;
    if !explicit
        && let Some(status) = state
            .environments
            .lock()
            .unwrap()
            .get(&env_key(&environment.id, &environment.revision))
    {
        if status.state == "ready" {
            return Ok(());
        }
        ensure!(
            status.state != "failed",
            "environment preparation failed; inspect environment logs and explicitly prepare again"
        );
    }
    let result = async {
        if let Some(policy) = &state.policy {
            policy.check_environment(environment).map_err(anyhow::Error::msg)?;
        }
        let dir = state.environment_dir(environment)?;
        private_dir(&dir)?;
        private_dir(&dir.join("bin"))?;
        let definition = dir.join("definition.json");
        if definition.exists() {
            let existing: Environment = serde_json::from_reader(File::open(&definition)?)?;
            ensure!(
                serde_json::to_value(existing)? == serde_json::to_value(environment)?,
                "environment revision is immutable; choose a new revision"
            );
        } else {
            atomic_json(&definition, environment)?;
        }
        let logger = environment_logger(state, environment)?;
        state.environment_status(environment, "preparing", None)?;
        let mut requirements = environment.requirements.clone(); requirements.tools.clear(); requirements.build_target = None;
        if environment.image.is_some() { requirements.os = None; requirements.abi = None; }
        ensure!(state.capabilities.mismatch(&requirements).is_empty(), "worker does not satisfy environment platform requirements");
        let mut image_id = None;
        if let Some(image) = &environment.image {
            ensure!(environment.setup.is_empty(), "image setup scripts are not persistent; build a custom image externally or use a native setup recipe");
            ensure!(environment.requirements.os.as_deref().is_none_or(|os| os == "linux"), "image environments execute Linux");
            let runtime = runtime(state)?; let (cancel, receiver) = watch::channel(false);
            let mut command = Command::new(runtime); command.args(["pull", "--platform", &format!("linux/{}", image_arch(&state.capabilities.arch)?), image]);
            let exit = execute(state, &logger, command, receiver, 3600, None).await?; drop(cancel); ensure!(exit == 0, "image pull failed");
            let mut inspect = Command::new(runtime); inspect.args(["image", "inspect", "--format", "{{.Id}}", image]);
            image_id = Some(normalize_image_id(&capture(inspect).await?)?);
        } else {
            let values = expanded_env(Some(environment), None, &dir, None);
            for script in &environment.setup {
                logger.log(state, "system", "preparing environment\n".into())?;
                let mut command = native_recipe(script); command.current_dir(&dir).envs(&values);
                let (cancel, receiver) = watch::channel(false);
                let exit = execute(state, &logger, command, receiver, 3600, None).await?; drop(cancel); ensure!(exit == 0, "environment setup failed");
            }
        }
        verify_environment(state, environment, image_id.as_deref(), &logger, None).await?;
        if let Some(image) = image_id { atomic_json(&dir.join("image.json"), &image)?; }
        state.environment_status(environment, "ready", None)?;
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Err(error) = &result
        && let Err(persistence) =
            state.environment_status(environment, "failed", Some(format!("{error:#}")))
    {
        return Err(result.unwrap_err()).context(format!(
            "could not persist environment failure: {persistence:#}"
        ));
    }
    result
}

pub async fn probe() -> Result<Capabilities> {
    let os = std::env::consts::OS.to_owned();
    let arch = normalize_arch(std::env::consts::ARCH);
    #[cfg(windows)]
    ensure!(
        windows::native_arch()? == arch,
        "worker binary runs under architecture translation; install the native {} binary",
        windows::native_arch()?
    );
    #[cfg(unix)]
    {
        let mut uname = Command::new("uname");
        uname.arg("-m");
        if let Ok(kernel_arch) = capture(uname).await {
            ensure!(
                normalize_arch(&kernel_arch) == arch,
                "worker binary runs under architecture translation; install the native {} binary",
                normalize_arch(&kernel_arch)
            );
        }
    }
    if os == "macos" {
        let mut translated = Command::new("sysctl");
        translated.args(["-n", "sysctl.proc_translated"]);
        ensure!(
            capture(translated).await.unwrap_or_default() != "1",
            "worker binary runs under Rosetta; install the native arm64 binary"
        );
    }
    let abi = if os == "macos" {
        "darwin"
    } else if os == "windows" {
        "msvc"
    } else {
        // A static musl YLCC binary can run workloads on a glibc host.
        let mut getconf = Command::new("getconf");
        getconf.arg("GNU_LIBC_VERSION");
        let mut ldd = Command::new("ldd");
        ldd.arg("--version");
        host_linux_abi(
            &capture(getconf).await.unwrap_or_default(),
            &capture(ldd).await.unwrap_or_default(),
        )
    }
    .into();
    let mut tools = BTreeMap::new();
    let values = BTreeMap::new();
    for name in [
        "git",
        "rustc",
        "cargo",
        "node",
        "npm",
        "python3",
        "uv",
        "mise",
        "cc",
        "clang",
        "cmake",
        "make",
        "swift",
        "xcodebuild",
        "docker",
        "podman",
        "nvcc",
        "java",
        "javac",
        "gradle",
        "adb",
        "emulator",
        "powershell",
        "python",
        "bun",
        "zig",
        "protoc",
        "pkg-config",
    ] {
        if let Ok(version) = tool_version(name, &values, None).await {
            tools.insert(name.into(), version);
        }
    }
    let mut rust = Command::new("rustc");
    rust.arg("-vV");
    if let Ok(version) = capture(rust).await
        && let Some(host) = version.lines().find_map(|line| line.strip_prefix("host: "))
        && let Ok(dir) = tempfile::tempdir()
    {
        let source = dir.path().join("probe.rs");
        let binary = dir
            .path()
            .join(format!("probe{}", std::env::consts::EXE_SUFFIX));
        if fs::write(&source, "fn main() {}\n").is_ok() {
            let mut compiler = Command::new("rustc");
            compiler.arg(&source).arg("-o").arg(&binary);
            if capture(compiler).await.is_ok() && capture(Command::new(binary)).await.is_ok() {
                tools.insert(format!("target:{host}"), "ready".into());
            }
        }
    }
    if os == "macos" {
        for sdk in ["macosx", "iphoneos", "iphonesimulator"] {
            let mut command = Command::new("xcrun");
            command.args(["--sdk", sdk, "--show-sdk-version"]);
            if let Ok(version) = capture(command).await {
                tools.insert(format!("sdk:{sdk}"), version);
            }
        }
    }
    let mut runtimes = vec![];
    for runtime in ["podman", "docker"] {
        let mut command = Command::new(runtime);
        command.arg("info");
        if capture(command).await.is_ok() {
            runtimes.push(runtime.into());
        }
    }
    let mut cpu_features = vec![];
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo")
        && let Some(flags) = cpuinfo.lines().find_map(|line| {
            line.strip_prefix("flags")
                .or_else(|| line.strip_prefix("Features"))
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, flags)| flags)
        })
    {
        cpu_features.extend(flags.split_whitespace().map(String::from));
    }
    #[cfg(target_arch = "x86_64")]
    {
        for (name, present) in [
            ("sse2", std::is_x86_feature_detected!("sse2")),
            ("sse4_2", std::is_x86_feature_detected!("sse4.2")),
            ("avx", std::is_x86_feature_detected!("avx")),
            ("avx2", std::is_x86_feature_detected!("avx2")),
            ("avx512f", std::is_x86_feature_detected!("avx512f")),
            ("aes", std::is_x86_feature_detected!("aes")),
            ("fma", std::is_x86_feature_detected!("fma")),
        ] {
            if present {
                cpu_features.push(name.into());
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        for (name, present) in [
            ("neon", std::arch::is_aarch64_feature_detected!("neon")),
            ("aes", std::arch::is_aarch64_feature_detected!("aes")),
            ("sha2", std::arch::is_aarch64_feature_detected!("sha2")),
            ("crc", std::arch::is_aarch64_feature_detected!("crc")),
            ("lse", std::arch::is_aarch64_feature_detected!("lse")),
            ("fp16", std::arch::is_aarch64_feature_detected!("fp16")),
            (
                "dotprod",
                std::arch::is_aarch64_feature_detected!("dotprod"),
            ),
        ] {
            if present {
                cpu_features.push(name.into());
            }
        }
    }
    cpu_features.sort();
    cpu_features.dedup();
    #[cfg(windows)]
    let memory_bytes = windows::memory_bytes()?;
    #[cfg(not(windows))]
    let memory_bytes = if os == "linux" {
        fs::read_to_string("/proc/meminfo")
            .unwrap_or_default()
            .lines()
            .find_map(|line| {
                line.strip_prefix("MemTotal:")
                    .and_then(|line| line.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or(0)
            * 1024
    } else {
        let mut command = Command::new("sysctl");
        command.args(["-n", "hw.memsize"]);
        capture(command)
            .await
            .unwrap_or_default()
            .parse()
            .unwrap_or(0)
    };
    let containerized = std::env::var("YLCC_CONTAINERIZED").is_ok_and(|value| value == "1")
        || Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists();
    let mut gpu = vec![];
    #[cfg(target_os = "linux")]
    {
        if tokio::task::spawn_blocking(cuda_accessible)
            .await
            .unwrap_or(false)
        {
            gpu.push("cuda".into());
        }
        if tokio::task::spawn_blocking(rocm_accessible)
            .await
            .unwrap_or(false)
        {
            gpu.push("rocm".into());
        }
    }
    if os == "macos" && !containerized {
        let script = r##"import Metal
import MetalPerformanceShaders
guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue(), let buffer = device.makeBuffer(length: 4), let library = try? device.makeLibrary(source: "#include <metal_stdlib>\nusing namespace metal; kernel void probe(device uint *p [[buffer(0)]]) { p[0] = 42; }", options: nil), let function = library.makeFunction(name: "probe"), let pipeline = try? device.makeComputePipelineState(function: function), let command = queue.makeCommandBuffer(), let encoder = command.makeComputeCommandEncoder() else { fatalError("Metal unavailable") }
encoder.setComputePipelineState(pipeline); encoder.setBuffer(buffer, offset: 0, index: 0); encoder.dispatchThreads(MTLSize(width: 1, height: 1, depth: 1), threadsPerThreadgroup: MTLSize(width: 1, height: 1, depth: 1)); encoder.endEncoding(); command.commit(); command.waitUntilCompleted()
guard command.status == .completed && buffer.contents().load(as: UInt32.self) == 42 else { fatalError("Metal command failed") }
print(MPSSupportsMTLDevice(device) ? "metal mps" : "metal")"##;
        let mut command = Command::new("xcrun");
        command.args(["swift", "-e", script]);
        if let Ok(backends) = capture(command).await {
            gpu.extend(backends.split_whitespace().map(String::from));
        }
    }
    Ok(Capabilities {
        os,
        arch,
        abi,
        tools,
        gpu,
        cpu_features,
        runtimes,
        cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        memory_bytes,
        containerized,
        policy: None,
    })
}

fn host_linux_abi(getconf: &str, ldd: &str) -> &'static str {
    if getconf.starts_with("glibc ") || ldd.contains("GNU libc") || ldd.contains("GLIBC") {
        "gnu"
    } else if ldd.contains("musl") {
        "musl"
    } else {
        "unknown"
    }
}

#[cfg(target_os = "linux")]
unsafe fn symbol<T: Copy>(library: *mut libc::c_void, name: &std::ffi::CStr) -> Option<T> {
    let pointer = unsafe { libc::dlsym(library, name.as_ptr()) };
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute_copy(&pointer) })
    }
}
#[cfg(target_os = "linux")]
fn cuda_accessible() -> bool {
    unsafe {
        let library = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if library.is_null() {
            return false;
        }
        let result = (|| {
            let init: unsafe extern "C" fn(u32) -> i32 = symbol(library, c"cuInit")?;
            let count: unsafe extern "C" fn(*mut i32) -> i32 =
                symbol(library, c"cuDeviceGetCount")?;
            let create: unsafe extern "C" fn(*mut *mut libc::c_void, u32, i32) -> i32 =
                symbol(library, c"cuCtxCreate_v2")?;
            let allocate: unsafe extern "C" fn(*mut u64, usize) -> i32 =
                symbol(library, c"cuMemAlloc_v2")?;
            let free: unsafe extern "C" fn(u64) -> i32 = symbol(library, c"cuMemFree_v2")?;
            let destroy: unsafe extern "C" fn(*mut libc::c_void) -> i32 =
                symbol(library, c"cuCtxDestroy_v2")?;
            let mut devices = 0;
            if init(0) != 0 || count(&mut devices) != 0 || devices < 1 {
                return None;
            }
            let mut context = std::ptr::null_mut();
            if create(&mut context, 0, 0) != 0 {
                return None;
            }
            let mut memory = 0;
            let allocated = allocate(&mut memory, 4) == 0;
            if allocated {
                free(memory);
            }
            destroy(context);
            Some(allocated)
        })()
        .unwrap_or(false);
        libc::dlclose(library);
        result
    }
}
#[cfg(target_os = "linux")]
fn rocm_accessible() -> bool {
    unsafe {
        let library = libc::dlopen(
            c"libamdhip64.so".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return false;
        }
        let result = (|| {
            let init: unsafe extern "C" fn(u32) -> i32 = symbol(library, c"hipInit")?;
            let count: unsafe extern "C" fn(*mut i32) -> i32 =
                symbol(library, c"hipGetDeviceCount")?;
            let allocate: unsafe extern "C" fn(*mut *mut libc::c_void, usize) -> i32 =
                symbol(library, c"hipMalloc")?;
            let free: unsafe extern "C" fn(*mut libc::c_void) -> i32 = symbol(library, c"hipFree")?;
            let mut devices = 0;
            if init(0) != 0 || count(&mut devices) != 0 || devices < 1 {
                return None;
            }
            let mut memory = std::ptr::null_mut();
            let allocated = allocate(&mut memory, 4) == 0;
            if allocated {
                free(memory);
            }
            Some(allocated)
        })()
        .unwrap_or(false);
        libc::dlclose(library);
        result
    }
}

const LAST_USED: &str = "last-used";
/// Retention follows common CI and cluster practice: GitHub Actions evicts caches unused for
/// 7 days, and the kubelet frees images least recently used first once disk use passes 85%,
/// until it is back under 80%, sparing anything used within a minimum age so consecutive
/// jobs on a disk filled by something else do not thrash.
const ATTEMPT_RETENTION: Duration = Duration::from_secs(3 * 24 * 3600);
const UNUSED_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
const MINIMUM_AGE: Duration = Duration::from_secs(3600);
const DISK_HIGH: f64 = 0.85;
const DISK_LOW: f64 = 0.80;
/// A self-updated worker exits with this status so its service manager starts the new binary.
pub const RESTART_EXIT_CODE: i32 = 75;
const RELEASES: &str = "https://github.com/semanticist21/your-local-cloud-computing/releases/download";

fn cache_marker(stored: &Path) -> PathBuf {
    stored.with_extension("used")
}
fn previous_binary(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}
fn modified(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).and_then(|m| m.modified()).ok()
}

/// Removes a file or tree, including one a job left read-only (Go module caches, read-only
/// Windows files). Links are removed, never followed.
fn remove_tree(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.is_dir() || crate::platform::is_link(&metadata) {
        return match fs::remove_file(path).or_else(|_| fs::remove_dir(path)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error).with_context(|| format!("remove {}", path.display())),
            _ => Ok(()),
        };
    }
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    make_writable(path);
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error).with_context(|| format!("remove {}", path.display())),
        _ => Ok(()),
    }
}
fn make_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else { return };
    if crate::platform::is_link(&metadata) {
        return;
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    if metadata.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o700);
        let _ = fs::set_permissions(path, permissions);
    }
    #[cfg(windows)]
    {
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
    if metadata.is_dir() && let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

/// A finished attempt keeps only its journal, job spec and logs.
fn release_attempt_files(dir: &Path) {
    for name in ["work", "tmp", "environment"] {
        if let Err(error) = remove_tree(&dir.join(name)) {
            eprintln!("attempt cleanup: {error:#}");
        }
    }
}

async fn collect_in_background(state: &Arc<State>) {
    let state = state.clone();
    let usage = |state: &State| crate::platform::disk_usage(&state.data);
    match tokio::task::spawn_blocking(move || collect_garbage(&state, SystemTime::now(), &usage)).await {
        Ok(Err(error)) => eprintln!("garbage collection: {error:#}"),
        Err(error) => eprintln!("garbage collection: {error}"),
        Ok(Ok(())) => {}
    }
}

enum Reclaimable {
    Blob,
    Cache,
    Environment(Option<EnvironmentStatus>),
}

/// Runs only while the worker is idle (it holds the exclusive drain lock), so nothing it
/// removes is in use. Expired entries always go; under disk pressure, oldest first.
fn collect_garbage(state: &State, now: SystemTime, usage: &dyn Fn(&State) -> Result<f64>) -> Result<()> {
    let Ok(_drain) = state.drain.try_write() else {
        return Ok(());
    };
    let attempts: Vec<_> = state.attempts.lock().unwrap().iter().map(|(id, attempt)| (id.clone(), attempt.clone())).collect();
    if attempts.iter().any(|(_, attempt)| matches!(attempt.report().state.as_str(), "assigned" | "running")) {
        return Ok(());
    }
    let age = |time: Option<SystemTime>| time.and_then(|time| now.duration_since(time).ok()).unwrap_or_default();
    for (id, attempt) in &attempts {
        // An unknown attempt keeps its records so it can still be reconciled or cancelled.
        if !matches!(attempt.report().state.as_str(), "succeeded" | "failed" | "cancelled") {
            continue;
        }
        release_attempt_files(&attempt.dir);
        if age(modified(&attempt.dir.join("journal.json"))) > ATTEMPT_RETENTION {
            remove_tree(&attempt.dir)?;
            state.attempts.lock().unwrap().remove(id);
        }
    }
    for entry in fs::read_dir(state.data.join("workspaces"))? {
        remove_tree(&entry?.path())?; // none is in use while idle
    }
    let mut candidates = vec![];
    for entry in fs::read_dir(state.data.join("blobs"))? {
        let path = entry?.path();
        candidates.push((modified(&path), path, Reclaimable::Blob));
    }
    for entry in fs::read_dir(state.data.join("caches"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "used") {
            if !path.with_extension("").exists() {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        candidates.push((modified(&cache_marker(&path)).or(modified(&path)), path, Reclaimable::Cache));
    }
    for environment in fs::read_dir(state.data.join("environments"))? {
        let environment = environment?.path();
        if !environment.is_dir() {
            continue;
        }
        for revision in fs::read_dir(&environment)? {
            let path = revision?.path();
            let used = modified(&path.join(LAST_USED)).max(modified(&path.join("status.json"))).or(modified(&path));
            let key = path.strip_prefix(state.data.join("environments"))?.to_string_lossy().replace('\\', "/");
            let status = state.environments.lock().unwrap().get(&key).cloned();
            candidates.push((used, path, Reclaimable::Environment(status)));
        }
    }
    candidates.sort_by_key(|candidate| candidate.0);
    let mut pressure = usage(state)? > DISK_HIGH;
    for (used, path, kind) in candidates {
        let unused = age(used);
        if unused <= MINIMUM_AGE || (!pressure && unused <= UNUSED_RETENTION) {
            break;
        }
        remove_tree(&path)?;
        match kind {
            Reclaimable::Blob => {}
            Reclaimable::Cache => {
                let _ = fs::remove_file(cache_marker(&path));
            }
            Reclaimable::Environment(status) => {
                if let Some(parent) = path.parent() {
                    let _ = fs::remove_dir(parent); // only once its last revision is gone
                }
                if let Some(mut status) = status {
                    state.environments.lock().unwrap().remove(&env_key(&status.environment_id, &status.revision));
                    status.state = "evicted".into();
                    status.error = None;
                    state.emit(WorkerMessage::Environment { status });
                }
            }
        }
        eprintln!("reclaimed {}", path.display());
        if pressure {
            pressure = usage(state)? > DISK_LOW;
        }
    }
    Ok(())
}

async fn evict_environment(state: &Arc<State>, id: &str) -> Result<()> {
    identifier(id)?;
    let _drain = state.drain.write().await;
    state.environments.lock().unwrap().retain(|_, status| status.environment_id != id);
    let dir = state.data.join("environments").join(id);
    tokio::task::spawn_blocking(move || remove_tree(&dir)).await?
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status().with_context(|| format!("download {url}"))?;
    Ok(response.bytes().await?.to_vec())
}

/// Replaces this worker's binary with a published release verified against its SHA256SUMS,
/// waits until no attempt is active, and exits so the service manager restarts it.
async fn self_update(state: &Arc<State>, version: &str) -> Result<()> {
    ensure!(
        version.len() <= 64 && version.strip_prefix('v').is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')),
        "invalid release tag"
    );
    ensure!(!state.capabilities.containerized, "container workers update by recreating the container from the new image");
    let exe = crate::platform::command_path(&std::env::current_exe()?.canonicalize()?);
    let dir = exe.parent().context("worker binary has no directory")?;
    let staging = tempfile::Builder::new().prefix(".ylcc-update-").tempdir_in(dir).with_context(|| {
        format!("the worker account cannot write {}; install the worker binary in a directory it owns", dir.display())
    })?;
    let (os, arch) = (&state.capabilities.os, &state.capabilities.arch);
    let (archive, binary) = if os == "windows" { (format!("ylcc-{os}-{arch}.zip"), "ylcc.exe") } else { (format!("ylcc-{os}-{arch}.tar.gz"), "ylcc") };
    let base = std::env::var("YLCC_RELEASE_BASE_URL").unwrap_or_else(|_| RELEASES.into());
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
    let sums = String::from_utf8(download(&client, &format!("{base}/{version}/SHA256SUMS")).await?)?;
    let expected = sums
        .lines()
        .find_map(|line| line.split_once(char::is_whitespace).filter(|(_, name)| name.trim_start_matches([' ', '*']) == archive).map(|(hash, _)| hash.to_owned()))
        .with_context(|| format!("SHA256SUMS of {version} lists no {archive}"))?;
    let bytes = download(&client, &format!("{base}/{version}/{archive}")).await?;
    ensure!(hex::encode(Sha256::digest(&bytes)) == expected, "{archive} does not match SHA256SUMS");
    let archive_path = staging.path().join(&archive);
    fs::write(&archive_path, &bytes)?;
    let extracted = Command::new("tar").arg("-xf").arg(&archive_path).arg("-C").arg(staging.path()).status().await?;
    ensure!(extracted.success(), "cannot extract {archive}");
    let new = staging.path().join(binary);
    let output = Command::new(&new).arg("--version").output().await.context("run the downloaded binary")?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(output.status.success() && reported == format!("ylcc {}", &version[1..]), "downloaded binary reports {reported:?}, not {version}");
    // The Hub stops assigning work to a paused worker; wait out anything already accepted.
    let _drain = loop {
        let drain = state.drain.write().await;
        if !state.attempts.lock().unwrap().values().any(|a| matches!(a.report().state.as_str(), "assigned" | "running")) {
            break drain;
        }
        drop(drain);
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let previous = previous_binary(&exe);
    let _ = fs::remove_file(&previous);
    fs::rename(&exe, &previous).context("move the running binary aside")?;
    if let Err(error) = fs::rename(&new, &exe) {
        let _ = fs::rename(&previous, &exe);
        return Err(error).context("install the new binary");
    }
    drop(staging);
    eprintln!("updated to {version}; exiting for the service manager to restart");
    std::process::exit(RESTART_EXIT_CODE)
}

fn prune_worker_cache_root(data_dir: &std::path::Path) {
    const MAX_CACHE_BYTES: u64 = 100 * 1024 * 1024 * 1024;
    const TTL_SECS: u64 = 7 * 24 * 60 * 60;
    let root = data_dir.join("caches");
    if !root.exists() {
        return;
    }
    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue; };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    files.push((path, meta.len(), modified));
                }
            }
        }
    }
    let now = std::time::SystemTime::now();
    let ttl = std::time::Duration::from_secs(TTL_SECS);
    let mut remaining = Vec::new();
    for (path, size, modified) in files {
        if now.duration_since(modified).unwrap_or_default() > ttl {
            let _ = std::fs::remove_file(&path);
        } else {
            remaining.push((path, size, modified));
        }
    }
    let mut total: u64 = remaining.iter().map(|(_, size, _)| *size).sum();
    remaining.sort_by_key(|item| item.2);
    for (path, size, _) in remaining {
        if total <= MAX_CACHE_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    // Best-effort empty-directory cleanup.
    let _ = std::fs::remove_dir_all(&root.join("tmp")).ok();
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn private_directories_preserve_data_and_skip_redundant_chmod() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("worker");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(directory.join("retained"), "keep").unwrap();
        private_dir(&directory).unwrap();
        let before = fs::metadata(&directory).unwrap();
        assert_eq!(before.mode() & 0o7777, 0o700);
        std::thread::sleep(Duration::from_millis(20));
        private_dir(&directory).unwrap();
        let after = fs::metadata(&directory).unwrap();
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec())
        );
        assert_eq!(
            fs::read_to_string(directory.join("retained")).unwrap(),
            "keep"
        );
        let link = temporary.path().join("link");
        symlink(&directory, &link).unwrap();
        private_dir(&link).unwrap();
        let file_link = temporary.path().join("file-link");
        symlink(directory.join("retained"), &file_link).unwrap();
        assert!(private_dir(&file_link).is_err());
        if unsafe { libc::geteuid() } != 0 {
            let root = fs::metadata("/").unwrap();
            let error = private_dir(Path::new("/")).unwrap_err();
            assert!(format!("{error:#}").contains("must belong"));
            assert_eq!(fs::metadata("/").unwrap().mode(), root.mode());
            let foreign_link = temporary.path().join("foreign-link");
            symlink("/", &foreign_link).unwrap();
            assert!(private_dir(&foreign_link).is_err());
            assert_eq!(fs::metadata("/").unwrap().mode(), root.mode());
        }
    }
    #[test]
    fn docker_and_podman_image_ids_normalize_to_the_same_digest() {
        let hash = "a".repeat(64);
        let expected = format!("sha256:{hash}");
        assert_eq!(normalize_image_id(&hash).unwrap(), expected);
        assert_eq!(
            normalize_image_id(&format!("{expected}\n")).unwrap(),
            expected
        );
        for invalid in ["", "short", "sha512:abc", &"A".repeat(64)] {
            assert!(normalize_image_id(invalid).is_err());
        }
    }
    #[test]
    fn host_abi_is_independent_of_ylcc_linkage() {
        assert_eq!(host_linux_abi("glibc 2.43", ""), "gnu");
        assert_eq!(host_linux_abi("", "musl libc (x86_64)"), "musl");
        assert_eq!(host_linux_abi("", ""), "unknown");
    }
    fn state_at(data: &Path) -> Arc<State> {
        for path in [
            data.to_path_buf(),
            data.join("attempts"),
            data.join("blobs"),
            data.join("caches"),
            data.join("environments"),
            data.join("workspaces"),
        ] {
            private_dir(&path).unwrap();
        }
        let (events, _) = broadcast::channel(100);
        Arc::new(State {
            hub: "http://127.0.0.1:1".into(),
            token: "test-token".into(),
            data: data.into(),
            id: "test".into(),
            client: reqwest::Client::new(),
            capabilities: Capabilities {
                os: std::env::consts::OS.into(),
                arch: normalize_arch(std::env::consts::ARCH),
                ..Default::default()
            },
            max_jobs: 1,
            slots: Arc::new(Semaphore::new(1)),
            drain: RwLock::new(()),
            attempts: Mutex::new(HashMap::new()),
            environments: Mutex::new(BTreeMap::new()),
            image_paths: Mutex::new(HashMap::new()),
            caches: Mutex::new(HashSet::new()),
            workspaces: Mutex::new(HashSet::new()),
            events,
            policy: None,
        metrics: false,
        })
    }
    #[test]
    fn podman_relabels_owned_mounts_without_disabling_selinux() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = state_at(temp.path());
        Arc::get_mut(&mut state).unwrap().capabilities.runtimes = vec!["podman".into()];
        let command = image_command(
            &state,
            "image",
            &["true".into()],
            &temp.path().join("work"),
            &temp.path().join("env"),
            &BTreeMap::new(),
            "test",
            None,
        )
        .unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|s| s == "--init"));
        assert!(
            args.iter()
                .any(|s| s.ends_with("dst=/workspace,relabel=private"))
        );
        assert!(
            args.iter()
                .any(|s| s.ends_with("dst=/ylcc-env,relabel=shared"))
        );
        assert!(!args.iter().any(|s| s.contains("label=disable")));
    }
    fn attempt_at(data: &Path, id: &str) -> Arc<Attempt> {
        let dir = data.join("attempts").join(id);
        private_dir(&dir).unwrap();
        let (cancel, _) = watch::channel(false);
        let journal = Journal {
            report: AttemptReport {
                job_id: "job".into(),
                attempt_id: id.into(),
                state: "assigned".into(),
                exit_code: None,
                error: None,
                results: vec![],
            },
            seq: 0,
            pid: None,
            container: None,
            runtime: None,
            windows_job: None,
        };
        atomic_json(&dir.join("journal.json"), &journal).unwrap();
        Arc::new(Attempt {
            dir,
            journal: Mutex::new(journal),
            cancel,
        })
    }
    #[test]
    fn garbage_collection_frees_finished_and_unused_files_only_when_idle() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let mut events = state.events.subscribe();
        let attempt = attempt_at(temp.path(), "done");
        attempt.update(|journal| journal.report.state = "succeeded".into()).unwrap();
        state.attempts.lock().unwrap().insert("done".into(), attempt.clone());
        let locked = attempt.dir.join("work/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("file"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        }
        fs::create_dir(attempt.dir.join("tmp")).unwrap();
        let blob = temp.path().join("blobs").join("a".repeat(64));
        fs::write(&blob, "blob").unwrap();
        let cache = temp.path().join("caches/key");
        fs::create_dir(&cache).unwrap();
        fs::write(cache_marker(&cache), "").unwrap();
        let environment = temp.path().join("environments/env/rev");
        fs::create_dir_all(&environment).unwrap();
        fs::write(environment.join(LAST_USED), "").unwrap();
        let status = EnvironmentStatus {
            worker_id: "test".into(),
            environment_id: "env".into(),
            revision: "rev".into(),
            state: "ready".into(),
            error: None,
        };
        state.environments.lock().unwrap().insert(env_key("env", "rev"), status);

        let idle = |_: &State| Ok(0.5);
        collect_garbage(&state, SystemTime::now(), &idle).unwrap();
        assert!(!attempt.dir.join("work").exists() && !attempt.dir.join("tmp").exists());
        assert!(attempt.dir.join("journal.json").exists());
        assert!(blob.exists() && cache.exists() && environment.exists());

        let later = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        let busy = attempt_at(temp.path(), "busy");
        state.attempts.lock().unwrap().insert("busy".into(), busy.clone());
        collect_garbage(&state, later, &idle).unwrap();
        assert!(blob.exists() && attempt.dir.exists(), "an active attempt blocks collection");

        busy.update(|journal| journal.report.state = "unknown".into()).unwrap();
        collect_garbage(&state, later, &idle).unwrap();
        assert!(!attempt.dir.exists() && !state.attempts.lock().unwrap().contains_key("done"));
        assert!(busy.dir.exists(), "an unknown attempt stays reconcilable");
        assert!(!blob.exists() && !cache.exists() && !cache_marker(&cache).exists());
        assert!(!temp.path().join("environments/env").exists());
        assert!(state.environments.lock().unwrap().is_empty());
        let WorkerMessage::Environment { status } = events.try_recv().unwrap() else {
            panic!("expected an eviction status")
        };
        assert_eq!(status.state, "evicted");
    }
    #[test]
    fn disk_pressure_evicts_least_recently_used_until_below_the_low_mark() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let blobs = temp.path().join("blobs");
        let now = SystemTime::now();
        for (name, hours) in [("old", 4), ("mid", 3), ("new", 2)] {
            let file = File::create(blobs.join(name)).unwrap();
            file.set_modified(now - Duration::from_secs(hours * 3600)).unwrap();
        }
        let usage = |state: &State| Ok(if fs::read_dir(state.data.join("blobs")).unwrap().count() == 3 { 0.9 } else { 0.79 });
        collect_garbage(&state, now, &usage).unwrap();
        assert!(!blobs.join("old").exists());
        assert!(blobs.join("mid").exists() && blobs.join("new").exists());
        let full = |_: &State| Ok(0.99);
        let fresh = blobs.join("fresh");
        fs::write(&fresh, "").unwrap();
        collect_garbage(&state, now, &full).unwrap();
        assert!(fresh.exists(), "anything used within the minimum age is spared");
    }
    #[test]
    fn host_samples_are_plausible() {
        let mut sampler = Sampler { cpu: cpu_ticks() };
        assert!(sampler.cpu.is_some(), "CPU ticks are readable");
        std::thread::sleep(Duration::from_millis(200));
        let metrics = sampler.sample(Path::new("."));
        assert!((0.0..=100.0).contains(&metrics.cpu_percent), "{metrics:?}");
        assert!(metrics.memory_total > 0 && metrics.memory_used > 0, "{metrics:?}");
        assert!(metrics.memory_used <= metrics.memory_total, "{metrics:?}");
        assert!(metrics.disk_percent > 0.0 && metrics.disk_percent <= 100.0, "{metrics:?}");
    }
    #[tokio::test]
    async fn restart_marks_unfinished_unknown_and_duplicate_does_not_run() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        attempt.log(&state, "stdout", "durable\n".into()).unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(attempt.dir.join("logs.jsonl"))
            .unwrap();
        file.write_all(b"{partial").unwrap();
        drop(file);
        load_state(&state).await.unwrap();
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"].report().state,
            "unknown"
        );
        let job = Job {
            id: "job".into(),
            attempt_id: "attempt".into(),
            state: "assigned".into(),
            spec: JobSpec {
                command: vec!["sh".into(), "-c".into(), "touch should-not-run".into()],
                ..Default::default()
            },
            worker_id: Some("test".into()),
            created_at: 0,
            updated_at: 0,
            exit_code: None,
            error: None,
            results: vec![],
            submitted_by: None,
            started_at: None,
            last_output_at: None,
        };
        accept(state.clone(), job, None).unwrap();
        assert!(!attempt.dir.join("work").exists());
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"]
                .journal
                .lock()
                .unwrap()
                .seq,
            1
        );
        assert!(
            fs::read_to_string(attempt.dir.join("logs.jsonl"))
                .unwrap()
                .ends_with('\n')
        );
    }
    #[tokio::test]
    async fn writable_checkout_cannot_mutate_content_cache() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let hash = hex::encode(Sha256::digest(b"original"));
        fs::write(temp.path().join("blobs").join(&hash), b"original").unwrap();
        let job = Job {
            id: "job".into(),
            attempt_id: "attempt".into(),
            state: "assigned".into(),
            spec: JobSpec {
                command: if cfg!(windows) {
                    vec![
                        "powershell.exe".into(),
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-Command".into(),
                        "[IO.File]::WriteAllText('input', 'edited')".into(),
                    ]
                } else {
                    vec!["sh".into(), "-c".into(), "printf edited > input".into()]
                },
                inputs: vec![FileEntry {
                    path: "input".into(),
                    hash: hash.clone(),
                    size: 8,
                    mode: 0o644,
                }],
                ..Default::default()
            },
            worker_id: None,
            created_at: 0,
            updated_at: 0,
            exit_code: None,
            error: None,
            results: vec![],
            submitted_by: None,
            started_at: None,
            last_output_at: None,
        };
        let (cancel, receiver) = watch::channel(false);
        let (exit, _) = run_attempt(&state, &attempt, &job, None, receiver)
            .await
            .unwrap();
        drop(cancel);
        assert_eq!(exit, 0);
        assert_eq!(fs::read(attempt.dir.join("work/input")).unwrap(), b"edited");
        assert_eq!(
            fs::read(temp.path().join("blobs").join(hash)).unwrap(),
            b"original"
        );
    }
    #[test]
    fn cached_tasks_reuse_one_fixed_fresh_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let spec = JobSpec { cache: vec!["target".into()], cache_scope: Some("app".into()), ..Default::default() };
        let attempt = temp.path().join("attempts/a");
        let first = Workspace::claim(&state, &spec, &attempt).unwrap();
        let fixed = first.dir.clone();
        assert!(fixed.starts_with(temp.path().join("workspaces")));
        fs::create_dir(&fixed).unwrap();
        fs::write(fixed.join("left"), "x").unwrap();
        let busy = Workspace::claim(&state, &spec, &attempt).unwrap();
        assert_eq!(busy.dir, attempt.join("work"), "a concurrent attempt uses its own directory");
        drop(busy);
        drop(first);
        assert!(!fixed.exists(), "the workspace is removed after the attempt");
        assert_eq!(Workspace::claim(&state, &spec, &attempt).unwrap().dir, fixed);
        let uncached = JobSpec { cache_scope: Some("app".into()), ..Default::default() };
        assert_eq!(Workspace::claim(&state, &uncached, &attempt).unwrap().dir, attempt.join("work"));
    }
    #[test]
    fn cache_lease_restores_saves_and_skips_busy_or_redirected_caches() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let spec = JobSpec {
            cache: vec!["deps/target".into()],
            cache_scope: Some("app".into()),
            ..Default::default()
        };
        let work = |name: &str| {
            let dir = temp.path().join(name);
            fs::create_dir(&dir).unwrap();
            dir
        };
        let first = work("first");
        drop({
            let lease = CacheLease::claim(&state, &spec, &first).unwrap();
            fs::create_dir_all(first.join("deps/target")).unwrap();
            fs::write(first.join("deps/target/built"), "1").unwrap();
            lease
        });
        let second = work("second");
        let held = CacheLease::claim(&state, &spec, &second).unwrap();
        assert_eq!(
            fs::read_to_string(second.join("deps/target/built")).unwrap(),
            "1"
        );
        let busy = work("busy");
        let busy_lease = CacheLease::claim(&state, &spec, &busy).unwrap();
        assert!(busy_lease.entries.is_empty() && !busy.join("deps/target").exists());
        drop(busy_lease);
        drop(held);
        assert!(!second.join("deps/target").exists());
        let other_scope = JobSpec {
            cache_scope: Some("other".into()),
            ..spec.clone()
        };
        let third = work("third");
        drop(CacheLease::claim(&state, &other_scope, &third).unwrap());
        assert!(!third.join("deps/target").exists());
        let other_environment = JobSpec {
            environment: Some("node-22.12.0".into()),
            ..spec.clone()
        };
        let toolchain = work("toolchain");
        drop(CacheLease::claim(&state, &other_environment, &toolchain).unwrap());
        assert!(!toolchain.join("deps/target").exists());
        #[cfg(unix)]
        {
            let fourth = work("fourth");
            let lease = CacheLease::claim(&state, &spec, &fourth).unwrap();
            fs::remove_dir_all(fourth.join("deps")).unwrap();
            std::os::unix::fs::symlink(temp.path().join("first"), fourth.join("deps")).unwrap();
            drop(lease);
            assert!(temp.path().join("first").exists());
            assert!(
                CacheLease::claim(&state, &spec, &work("fifth"))
                    .unwrap()
                    .entries
                    .len()
                    == 1
            );
        }
    }
    #[cfg(windows)]
    fn windows_descendant_command(directory: &Path, wait: bool) -> Command {
        let script = format!(
            "$ErrorActionPreference = 'Stop'; Write-Output before; [Console]::Error.WriteLine('error'); $child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') -NoNewWindow -PassThru; Set-Content ready $child.Id; {}",
            if wait {
                "Wait-Process -Id $child.Id"
            } else {
                "exit 0"
            }
        );
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .current_dir(directory);
        command
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_cancellation_timeout_and_parent_exit_terminate_the_whole_job() {
        for mode in ["cancel", "timeout", "exit"] {
            let temp = tempfile::tempdir().unwrap();
            let state = state_at(temp.path());
            let attempt = attempt_at(temp.path(), "attempt");
            let command = windows_descendant_command(temp.path(), mode != "exit");
            let (cancel, receiver) = watch::channel(false);
            let cancellation = async {
                if mode == "cancel" {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    while !temp.path().join("ready").exists() {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "descendant did not start"
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    cancel.send(true).unwrap();
                }
            };
            let (result, _) = tokio::join!(
                execute(
                    &state,
                    &attempt,
                    command,
                    receiver,
                    if mode == "timeout" { 3 } else { 15 },
                    None
                ),
                cancellation
            );
            if mode == "exit" {
                assert_eq!(result.unwrap(), 0);
            } else {
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains(if mode == "cancel" {
                        "job cancelled"
                    } else {
                        "exceeded timeout"
                    }),
                    "{error:#}"
                );
            }
            assert!(temp.path().join("ready").exists());
            let name = attempt.journal.lock().unwrap().windows_job.clone().unwrap();
            assert!(windows::absent(&name).unwrap());
            let logs = fs::read_to_string(attempt.dir.join("logs.jsonl")).unwrap();
            assert!(logs.contains("before") && logs.contains("error"));
        }
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_restart_cancellation_queries_named_job_and_never_saved_pid() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .kill_on_drop(true);
        let (mut child, job, thread) = windows::spawn(&mut command).await.unwrap();
        attempt
            .update(|journal| {
                journal.report.state = "running".into();
                journal.pid = Some(u32::MAX); // Reused/arbitrary PIDs are never cancellation evidence.
                journal.windows_job = Some(job.name.clone());
            })
            .unwrap();
        thread.resume().unwrap();
        load_state(&state).await.unwrap();
        request_cancel(&state, "job", "attempt").await.unwrap();
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"].report().state,
            "unknown"
        );
        assert!(job.active().unwrap() > 0);
        stop_process(&mut child, &job, None).await.unwrap();
        drop(job);
        request_cancel(&state, "job", "attempt").await.unwrap();
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"].report().state,
            "cancelled"
        );
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cleanup_waits_for_zombie_group_without_claiming_absence() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]).process_group(0);
        let mut child = command.spawn().unwrap();
        let pgid = child.id().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let error = loop {
            if unsafe { libc::kill(-(pgid as i32), 0) } != 0 {
                break std::io::Error::last_os_error();
            }
            assert!(tokio::time::Instant::now() < deadline, "child did not exit");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let absent_before_reaping = process_group_absent(pgid);
        let cleanup = stop_process(&mut child, pgid, None).await;
        child.wait().await.unwrap();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert!(!absent_before_reaping.unwrap());
        cleanup.unwrap();
        assert!(process_group_absent(pgid).unwrap());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_descendants_and_preserves_logs() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf before; printf error >&2; (sleep 1; touch leaked) & wait",
            ])
            .current_dir(temp.path());
        let (cancel, receiver) = watch::channel(false);
        let cancellation = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.send(true).unwrap();
        };
        let (result, _) = tokio::join!(
            execute(&state, &attempt, command, receiver, 10, None),
            cancellation
        );
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!temp.path().join("leaked").exists());
        let logs = fs::read_to_string(attempt.dir.join("logs.jsonl")).unwrap();
        assert!(logs.contains("before"));
        assert!(logs.contains("error"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_lets_term_trap_clean_up() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "trap 'sleep 0.3; touch cleaned; exit 1' TERM; printf ready > ready; while :; do sleep 0.05; done",
            ])
            .current_dir(temp.path());
        let (cancel, receiver) = watch::channel(false);
        let cancellation = async {
            while !temp.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.send(true).unwrap();
        };
        let (result, _) = tokio::join!(
            execute(&state, &attempt, command, receiver, 10, None),
            cancellation
        );
        assert!(result.is_err());
        assert!(temp.path().join("cleaned").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_term_ignoring_child_after_leader_exits() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "(trap '' TERM; printf ready > ready; sleep 3; touch leaked) & wait",
            ])
            .current_dir(temp.path());
        let (cancel, receiver) = watch::channel(false);
        let cancellation = async {
            while !temp.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.send(true).unwrap();
        };
        let (result, _) = tokio::join!(
            execute(&state, &attempt, command, receiver, 10, None),
            cancellation
        );
        let error = result.unwrap_err();
        assert!(error.to_string().contains("job cancelled"), "{error:#}");
        let pgid = attempt.journal.lock().unwrap().pid.unwrap();
        assert!(process_group_absent(pgid).unwrap());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!temp.path().join("leaked").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn natural_parent_exit_cleans_stubborn_child_and_retained_pipes() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command.args(["-c", "(trap '' TERM; printf ready > ready; sleep 3; touch leaked) & while [ ! -f ready ]; do sleep 0.01; done; exit 0"]).current_dir(temp.path());
        let (_keepalive, receiver) = watch::channel(false);
        assert_eq!(
            execute(&state, &attempt, command, receiver, 10, None)
                .await
                .unwrap(),
            0
        );
        let pgid = attempt.journal.lock().unwrap().pid.unwrap();
        assert!(process_group_absent(pgid).unwrap());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!temp.path().join("leaked").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_stubborn_child_before_returning_failure() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command
            .args(["-c", "(trap '' TERM; sleep 5; touch leaked) & wait"])
            .current_dir(temp.path());
        let (_keepalive, receiver) = watch::channel(false);
        let error = execute(&state, &attempt, command, receiver, 1, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeded timeout"), "{error:#}");
        let pgid = attempt.journal.lock().unwrap().pid.unwrap();
        assert!(process_group_absent(pgid).unwrap());
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(!temp.path().join("leaked").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn restarted_dead_attempt_can_be_cancelled_without_signalling_saved_pid() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]).process_group(0);
        let mut child = command.spawn().unwrap();
        let pgid = child.id().unwrap();
        child.wait().await.unwrap();
        assert!(process_group_absent(pgid).unwrap());
        attempt
            .update(|journal| {
                journal.pid = Some(pgid);
                journal.report.state = "running".into();
            })
            .unwrap();
        load_state(&state).await.unwrap();
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"].report().state,
            "unknown"
        );
        let mut events = state.events.subscribe();
        request_cancel(&state, "job", "attempt").await.unwrap();
        assert_eq!(
            state.attempts.lock().unwrap()["attempt"].report().state,
            "cancelled"
        );
        assert!(
            matches!(events.recv().await.unwrap(), WorkerMessage::Finished { report } if report.state == "cancelled")
        );
        let persisted: Journal =
            serde_json::from_reader(File::open(attempt.dir.join("journal.json")).unwrap()).unwrap();
        assert_eq!(persisted.report.state, "cancelled");
    }
    #[tokio::test]
    async fn cancel_for_never_accepted_attempt_confirms_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let mut events = state.events.subscribe();
        request_cancel(&state, "job", "lost-attempt").await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            WorkerMessage::Finished { report }
                if report.state == "cancelled" && report.job_id == "job" && report.attempt_id == "lost-attempt"
        ));
        assert!(state.attempts.lock().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn restarted_live_group_cancel_remains_unknown_and_never_signals_it() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]).process_group(0);
        let mut child = command.spawn().unwrap();
        let pgid = child.id().unwrap();
        attempt
            .update(|journal| {
                journal.pid = Some(pgid);
                journal.report.state = "unknown".into();
            })
            .unwrap();
        state
            .attempts
            .lock()
            .unwrap()
            .insert("attempt".into(), attempt.clone());
        request_cancel(&state, "job", "attempt").await.unwrap();
        assert_eq!(attempt.report().state, "unknown");
        assert!(attempt.report().error.unwrap().contains("still exists"));
        assert!(!process_group_absent(pgid).unwrap());
        stop_process(&mut child, pgid, None).await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn ready_environment_tool_drift_is_rejected_before_job_runs() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let attempt = attempt_at(temp.path(), "attempt");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let tool = shared.join("drift");
        fs::write(&tool, "#!/bin/sh\nprintf '1.0.0\\n'\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let mut environment = Environment {
            id: "a".into(),
            revision: "1".into(),
            ..Default::default()
        };
        environment
            .requirements
            .tools
            .insert("drift".into(), "1.0.0".into());
        environment
            .env
            .insert("PATH".into(), format!("{}:${{PATH}}", shared.display()));
        prepare(&state, &environment, true).await.unwrap();
        let mut second = Environment {
            id: "b".into(),
            revision: "1".into(),
            ..Default::default()
        };
        second.setup.push(format!(
            "printf '#!/bin/sh\\nprintf \"2.0.0\\\\n\"\\n' > '{}'",
            tool.display()
        ));
        prepare(&state, &second, true).await.unwrap();
        let job = Job {
            id: "job".into(),
            attempt_id: "attempt".into(),
            state: "assigned".into(),
            spec: JobSpec {
                command: vec!["sh".into(), "-c".into(), "touch should-not-run".into()],
                ..Default::default()
            },
            worker_id: None,
            created_at: 0,
            updated_at: 0,
            exit_code: None,
            error: None,
            results: vec![],
            submitted_by: None,
            started_at: None,
            last_output_at: None,
        };
        let (_keepalive, receiver) = watch::channel(false);
        let error = run_attempt(&state, &attempt, &job, Some(&environment), receiver)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("required drift 1.0.0, found 2.0.0"));
        assert!(!attempt.dir.join("work").exists());
        assert_eq!(state.environments.lock().unwrap()["a/1"].state, "failed");
        assert!(fs::read_to_string(tool).unwrap().contains("2.0.0"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn preparation_retry_initialization_failure_is_durable_and_releases_drain() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let environment = Environment {
            id: "broken".into(),
            revision: "1".into(),
            setup: vec![
                "rmdir \"$YLCC_ENV_DIR/bin\"; printf bad > \"$YLCC_ENV_DIR/bin\"; exit 1".into(),
            ],
            ..Default::default()
        };
        assert!(prepare(&state, &environment, true).await.is_err());
        let mut events = state.events.subscribe();
        let error = prepare(&state, &environment, true).await.unwrap_err();
        assert!(format!("{error:#}").contains("create private directory"));
        assert!(matches!(
            events.try_recv().unwrap(),
            WorkerMessage::Environment { status } if status.state == "failed"
        ));
        let status: EnvironmentStatus = serde_json::from_reader(
            File::open(
                state
                    .environment_dir(&environment)
                    .unwrap()
                    .join("status.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "failed");
        assert_eq!(
            state.environments.lock().unwrap()["broken/1"].state,
            "failed"
        );
        let attempt = attempt_at(temp.path(), "unrelated");
        let job = Job {
            id: "unrelated".into(),
            attempt_id: "unrelated".into(),
            state: "assigned".into(),
            spec: JobSpec {
                command: vec!["sh".into(), "-c".into(), "printf ok > completed".into()],
                ..Default::default()
            },
            worker_id: None,
            created_at: 0,
            updated_at: 0,
            exit_code: None,
            error: None,
            results: vec![],
            submitted_by: None,
            started_at: None,
            last_output_at: None,
        };
        let (_keepalive, receiver) = watch::channel(false);
        let (exit, _) = tokio::time::timeout(
            Duration::from_secs(5),
            run_attempt(&state, &attempt, &job, None, receiver),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(exit, 0);
        assert_eq!(
            fs::read_to_string(attempt.dir.join("work/completed")).unwrap(),
            "ok"
        );
    }
    fn policy_job(command: &[&str], environment: Option<&str>) -> Job {
        Job {
            id: "job".into(),
            attempt_id: "attempt".into(),
            state: "assigned".into(),
            spec: JobSpec {
                command: command.iter().map(|s| s.to_string()).collect(),
                environment: environment.map(Into::into),
                ..Default::default()
            },
            worker_id: None,
            created_at: 0,
            updated_at: 0,
            exit_code: None,
            error: None,
            results: vec![],
            submitted_by: None,
            started_at: None,
            last_output_at: None,
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn policy_refuses_jobs_before_preparing_or_fetching_anything() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = state_at(temp.path());
        Arc::get_mut(&mut state).unwrap().policy =
            Some(Policy::parse("version: 1\ncommands: [[sh, -c, \"printf ok > completed\"]]\n").unwrap());
        let environment = Environment {
            id: "tools".into(),
            revision: "1".into(),
            setup: vec!["touch \"$YLCC_ENV_DIR/should-not-run\"".into()],
            ..Default::default()
        };
        let mut job = policy_job(&["sh", "-c", "touch pwned"], Some("tools"));
        job.spec.inputs.push(FileEntry { path: "input".into(), hash: "0".repeat(64), size: 1, mode: 0o644 });
        let attempt = attempt_at(temp.path(), "attempt");
        let (_keepalive, receiver) = watch::channel(false);
        let error = run_attempt(&state, &attempt, &job, Some(&environment), receiver).await.unwrap_err();
        assert!(format!("{error:#}").contains("not allowed by worker policy"), "{error:#}");
        assert!(!attempt.dir.join("work").exists());
        assert!(!state.environment_dir(&environment).unwrap().exists());
        let allowed = policy_job(&["sh", "-c", "printf ok > completed"], None);
        let attempt = attempt_at(temp.path(), "allowed");
        let (_keepalive, receiver) = watch::channel(false);
        let (exit, _) = run_attempt(&state, &attempt, &allowed, None, receiver).await.unwrap();
        assert_eq!(exit, 0);
        assert_eq!(fs::read_to_string(attempt.dir.join("work/completed")).unwrap(), "ok");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn policy_pins_environment_content_including_prepared_definitions() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = state_at(temp.path());
        let environment = Environment {
            id: "tools".into(),
            revision: "1".into(),
            setup: vec!["touch \"$YLCC_ENV_DIR/prepared\"".into()],
            ..Default::default()
        };
        // Prepared before the policy existed.
        prepare(&state, &environment, true).await.unwrap();
        let digest = crate::policy::environment_digest(&environment);
        Arc::get_mut(&mut state).unwrap().policy = Some(
            Policy::parse(&format!("version: 1\nenvironments: [{{id: tools, sha256: \"*\"}}, {{id: pinned, sha256: {digest}}}]\n")).unwrap(),
        );
        // Same id and revision, different content: the directory on disk was built from the old one.
        let swapped = Environment { setup: vec!["touch \"$YLCC_ENV_DIR/evil\"".into()], ..environment.clone() };
        let attempt = attempt_at(temp.path(), "attempt");
        let (_keepalive, receiver) = watch::channel(false);
        let error = run_attempt(&state, &attempt, &policy_job(&["true"], Some("tools")), Some(&swapped), receiver)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("differs from the definition"), "{error:#}");
        // Explicit preparation of an unpinned recipe fails durably without running its setup.
        let unpinned = Environment { id: "pinned".into(), setup: vec!["touch \"$YLCC_ENV_DIR/should-not-run\"".into()], ..environment.clone() };
        let error = prepare(&state, &unpinned, true).await.unwrap_err();
        assert!(format!("{error:#}").contains("sha256"), "{error:#}");
        assert_eq!(state.environments.lock().unwrap()["pinned/1"].state, "failed");
        assert!(!state.environment_dir(&unpinned).unwrap().join("should-not-run").exists());
        let pinned = Environment { id: "pinned".into(), ..environment.clone() };
        assert_ne!(crate::policy::environment_digest(&pinned), digest, "the id is part of the content");
    }
    #[tokio::test]
    async fn preparation_definition_error_reports_failure_without_running_setup() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let environment = Environment {
            id: "broken".into(),
            revision: "1".into(),
            setup: vec!["touch should-not-run".into()],
            ..Default::default()
        };
        let dir = state.environment_dir(&environment).unwrap();
        private_dir(&dir).unwrap();
        fs::write(dir.join("definition.json"), "invalid json").unwrap();
        assert!(prepare(&state, &environment, true).await.is_err());
        let status: EnvironmentStatus =
            serde_json::from_reader(File::open(dir.join("status.json")).unwrap()).unwrap();
        assert_eq!(status.state, "failed");
        assert!(!dir.join("should-not-run").exists());
    }
    #[tokio::test]
    async fn preparation_storage_error_reports_failure_in_events_and_reconnect_hello() {
        let temp = tempfile::tempdir().unwrap();
        let state = state_at(temp.path());
        let environment = Environment {
            id: "broken".into(),
            revision: "1".into(),
            setup: vec!["touch should-not-run".into()],
            ..Default::default()
        };
        fs::write(temp.path().join("environments/broken"), "not a directory").unwrap();
        let mut events = state.events.subscribe();
        assert!(prepare(&state, &environment, true).await.is_err());
        assert!(matches!(
            events.try_recv().unwrap(),
            WorkerMessage::Environment { status }
                if status.state == "failed"
                    && status.error.as_deref().is_some_and(|error| error.contains("could not persist environment failure"))
        ));
        let WorkerMessage::Hello { environments, .. } = state.hello() else {
            panic!("expected worker hello");
        };
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].state, "failed");
        assert!(!temp.path().join("should-not-run").exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn image_path_is_inspected_by_immutable_id_and_cached_without_shell() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let mut state = state_at(temp.path());
        let runtime = temp.path().join("runtime");
        fs::write(&runtime, "#!/bin/sh\nprintf '%s\\n' '[\"HOME=/root\",\"PATH=/opt/custom/bin:/usr/local/cargo/bin:/usr/bin\"]'\n").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        Arc::get_mut(&mut state).unwrap().capabilities.runtimes =
            vec![runtime.to_string_lossy().into()];
        let id = format!("sha256:{}", "a".repeat(64));
        let path = image_path(&state, &id).await.unwrap();
        fs::remove_file(runtime).unwrap();
        assert_eq!(image_path(&state, &id).await.unwrap(), path);
        let mut environment = Environment::default();
        environment
            .env
            .insert("PATH".into(), "${YLCC_ENV_DIR}/custom:${PATH}".into());
        let values = expanded_env(
            Some(&environment),
            None,
            Path::new("/not-the-image-home"),
            Some(&path),
        );
        assert_eq!(
            values["PATH"],
            "/ylcc-env/custom:/ylcc-env/bin:/opt/custom/bin:/usr/local/cargo/bin:/usr/bin"
        );
        assert_eq!(values["HOME"], "/ylcc-env");
    }
    #[test]
    fn identifiers_cannot_escape() {
        for value in ["", ".", "..", "a/b", "a\\b", "-x", "a b"] {
            assert!(identifier(value).is_err());
        }
        assert!(identifier("worker-1_a").is_ok());
    }
    #[test]
    fn version_parsing() {
        assert_eq!(version_token("rustc 1.96.1 (hash)"), "1.96.1");
        assert_eq!(version_token("v22.19.0"), "22.19.0");
        assert_eq!(version_token("Python 3.13.7"), "3.13.7");
        assert_eq!(
            version_token("openjdk version \"17.0.15\" 2025-04-15"),
            "17.0.15"
        );
        assert_eq!(
            version_token("Apple clang version 17.0.0 (clang)"),
            "17.0.0"
        );
    }
    #[test]
    fn windows_input_spellings_do_not_merge_distinct_files_or_directories() {
        let entry = |path: &str| FileEntry {
            path: path.into(),
            hash: "a".repeat(64),
            size: 0,
            mode: 0o644,
        };
        assert!(validate_windows_inputs(&[entry("A/a"), entry("a/b")]).is_err());
        assert!(validate_windows_inputs(&[entry("A"), entry("a")]).is_err());
        validate_windows_inputs(&[entry("a/one"), entry("a/two")]).unwrap();
    }
    #[test]
    fn environment_expands_only_known_variables() {
        let mut environment = Environment::default();
        environment
            .env
            .insert("PATH".into(), "${YLCC_ENV_DIR}/custom:${PATH}".into());
        environment
            .env
            .insert("OTHER".into(), "$(echo bad):${UNKNOWN}".into());
        let values = expanded_env(Some(&environment), None, Path::new("/tmp/tools"), None);
        assert!(values["PATH"].starts_with(if cfg!(windows) {
            "/tmp/tools/custom:/tmp/tools/bin;"
        } else {
            "/tmp/tools/custom:/tmp/tools/bin:"
        }));
        assert_eq!(values["OTHER"], "$(echo bad):${UNKNOWN}");
    }
    #[test]
    fn outputs_reject_symlinks_and_include_mode_hash() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("out")).unwrap();
        fs::write(temp.path().join("out/a"), b"hello").unwrap();
        let mut files = BTreeMap::new();
        collect_output(temp.path(), &temp.path().join("out"), &mut files).unwrap();
        assert_eq!(files["out/a"].size, 5);
        assert_eq!(files["out/a"].hash, hex::encode(Sha256::digest(b"hello")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", temp.path().join("out/escape")).unwrap();
            assert!(collect_output(temp.path(), &temp.path().join("out"), &mut files).is_err());
        }
    }
    #[test]
    fn corrupt_cache_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("blob");
        fs::write(&path, b"changed").unwrap();
        assert!(verify_file(&path, &hex::encode(Sha256::digest(b"original")), None).is_err());
    }
}
