use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use ylcc::{client::Client, model::*, platform, policy::Policy, project::Project, snapshot};

#[derive(Parser)]
#[command(
    version = ylcc::VERSION,
    about = "Run workloads across your own Linux, macOS and Windows machines"
)]
struct Cli {
    #[arg(long, global = true, env = "YLCC_HUB_URL")]
    hub: Option<String>,
    #[arg(long, global = true, env = "YLCC_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[arg(long, global = true)]
    results_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
// CLI arguments are parsed once; keeping the argument structures direct simplifies dispatch.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Generate a protected pool token (never prints its contents).
    Init,
    /// Show first-run setup state and next command; does not modify secrets by default.
    Setup,
    /// Save requester defaults, including token file reference.
    Configure,
    Hub {
        #[arg(long, default_value = "127.0.0.1:7443")]
        listen: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    Worker {
        /// Worker id. Defaults to HOSTNAME (or COMPUTERNAME on Windows) when omitted.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        max_jobs: usize,
        /// Command policy YAML; without it the worker runs every job it is assigned.
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    /// Show the input snapshot manifest without submitting a job.
    Manifest(Selection),
    Run(RunArgs),
    /// List or run named tasks from a project YAML file.
    Task {
        #[command(subcommand)]
        command: Tasks,
    },
    Workers {
        #[command(subcommand)]
        command: Option<Workers>,
    },
    Jobs {
        #[command(subcommand)]
        command: Jobs,
    },
    Env {
        #[command(subcommand)]
        command: Envs,
    },
    Doctor,
    /// Print natural-language prompts and the commands agents should run.
    Examples,
    /// Machine-readable status summary for agents: health, workers and job states.
    Status,
    /// Print the verified install/upgrade command for this platform without exposing tokens.
    Update {
        /// Release tag to install, for example v0.4.2.
        #[arg(long, conflicts_with = "latest")]
        version: Option<String>,
        /// Resolve the newest published release tag before installing or printing the command.
        #[arg(long)]
        latest: bool,
        /// Download and execute the verified installer instead of only printing it.
        #[arg(long)]
        apply: bool,
        /// Replace the current binary without checking active Hub jobs first.
        #[arg(long)]
        force: bool,
    },
    /// Resolve the newest (or requested) release and install it in one step.
    #[command(alias = "up")]
    Upgrade {
        /// Release tag to install, for example v0.4.2.
        #[arg(long, conflicts_with = "latest")]
        version: Option<String>,
        /// Resolve the newest published release tag before installing (default when --version is omitted).
        #[arg(long)]
        latest: bool,
        /// Replace the current binary without checking active Hub jobs first.
        #[arg(long)]
        force: bool,
    },
    /// Preview or remove downloaded job result directories older than the given number of days.
    Cleanup {
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,
        #[arg(long)]
        apply: bool,
    },
    /// Print the Hub dashboard address and token-file location. Does not print the token value.
    Dashboard {
        /// Launch or avoid browser launch. Defaults to opening a browser on supported platforms.
        #[arg(long)]
        open: bool,
        /// Do not launch a browser; print the URL and token-file path only.
        #[arg(long, conflicts_with = "open")]
        no_open: bool,
    },
    Service {
        #[command(subcommand)]
        command: Services,
    },
    /// Issue, list or revoke per-machine Hub tokens (requires an admin token).
    Tokens {
        #[command(subcommand)]
        command: Tokens,
    },
    /// Validate a worker command policy or suggest one for a project's tasks.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Show recent Hub audit events, newest first (requires an admin token).
    Audit {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}
#[derive(Subcommand)]
enum PolicyCommand {
    /// Parse a policy file exactly as `ylcc worker --policy` does and show what it restricts.
    Check { file: PathBuf },
    /// Print a policy allowing exactly the given tasks (all tasks by default) and environments.
    Suggest {
        #[arg(long, default_value = "ylcc.yaml")]
        file: PathBuf,
        #[arg(long)]
        task: Vec<String>,
        /// Also allow a preset such as bun@1.3.14, as used by `ylcc run --environment`.
        #[arg(long, value_name = "NAME@VERSION")]
        environment: Vec<String>,
        /// Target OS for --environment presets (presets differ on Windows).
        #[arg(long)]
        os: Option<String>,
        /// Allow inline environments in projects without `name:`; their ids depend on the checkout path.
        #[arg(long)]
        allow_path_scoped: bool,
    },
}
#[derive(Subcommand)]
enum Tokens {
    /// Create a token and write its secret to a new owner-only file; the secret is never printed.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        role: Role,
        /// Worker id a worker token may connect as (required for --role worker).
        #[arg(long)]
        worker_id: Option<String>,
        /// New file for the secret; an existing file is never replaced.
        #[arg(long)]
        out: PathBuf,
    },
    List,
    /// Revoke a token by id; workers connected with it are disconnected.
    Revoke { id: String },
}
#[derive(Args)]
struct Selection {
    #[arg(long)]
    input: Vec<PathBuf>,
    #[arg(long)]
    project: bool,
    #[arg(long,conflicts_with_all=["input","project"])]
    no_input: bool,
}
impl Selection {
    fn snapshot(&self) -> Result<snapshot::Snapshot> {
        if self.no_input {
            return Ok(snapshot::Snapshot {
                files: vec![],
                excluded: vec![],
                blobs: tempfile::tempdir()?,
            });
        }
        snapshot::create(&std::env::current_dir()?, &self.input, self.project)
    }
}
#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    selection: Selection,
    #[arg(long)]
    output: Vec<String>,
    #[arg(long)]
    os: Option<String>,
    #[arg(long)]
    arch: Option<String>,
    #[arg(long)]
    abi: Option<String>,
    #[arg(long)]
    gpu: Option<String>,
    #[arg(
        long,
        value_name = "NAME=VERSION",
        help = "Require an exact tool version, or NAME=* for any detected version"
    )]
    tool: Vec<String>,
    #[arg(long)]
    cpu_feature: Vec<String>,
    #[arg(long)]
    build_target: Option<String>,
    /// Registered environment id, or a preset such as bun@1.3.14 (see `ylcc env presets`).
    #[arg(long)]
    environment: Option<String>,
    #[arg(long)]
    container: Option<String>,
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long, default_value_t = 0, allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-100..=100))]
    priority: i32,
    #[arg(long)]
    env: Vec<String>,
    /// Keep this workspace-relative directory on the worker between runs (for example target).
    #[arg(long)]
    cache: Vec<String>,
    /// Save this run as a task in ./ylcc.yaml (after success, or at submission with --detach).
    #[arg(long, value_name = "NAME")]
    save_task: Option<String>,
    #[arg(long)]
    detach: bool,
    #[arg(long)]
    no_download: bool,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}
#[derive(Subcommand)]
enum Tasks {
    List {
        #[arg(long, default_value = "ylcc.yaml")]
        file: PathBuf,
    },
    Init {
        #[arg(long, default_value = "ylcc.yaml")]
        file: PathBuf,
        #[arg(long, default_value = "rust", value_parser = ["rust", "node", "python", "bun", "go", "smoke"])]
        runtime: String,
        #[arg(long, default_value = "check")]
        name: String,
        #[arg(long, default_value = "linux")]
        os: String,
        #[arg(long, default_value = "x86_64")]
        arch: String,
        #[arg(long)]
        force: bool,
    },
    /// Remove a task from the project file; succeeds if it is already gone.
    Delete {
        name: String,
        #[arg(long, default_value = "ylcc.yaml")]
        file: PathBuf,
    },
    Run {
        name: String,
        #[arg(long, default_value = "ylcc.yaml")]
        file: PathBuf,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        no_download: bool,
    },
}
#[derive(Subcommand)]
enum Workers {
    /// Stop assigning new work; running jobs continue.
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    /// Limit concurrency within the worker's startup max-jobs ceiling.
    Limit {
        id: String,
        #[arg(long, value_parser = parse_max_jobs)]
        max_jobs: usize,
    },
    /// Delete a worker from the Hub's worker list.
    Delete {
        id: String,
    },
    /// Replace native workers' binaries with a published release, one worker at a time.
    /// Each worker is paused, finishes its running jobs, restarts on the new binary and is resumed.
    Update {
        ids: Vec<String>,
        /// Every online native worker; container workers are skipped (recreate them from the new image).
        #[arg(long, conflicts_with = "ids")]
        all: bool,
        /// Release tag such as v0.4.6; defaults to the newest published release.
        #[arg(long)]
        version: Option<String>,
        /// Seconds to wait for each worker to drain, update and reconnect.
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
    },
}
fn parse_max_jobs(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "max-jobs must be a positive integer".to_owned())?;
    if value == 0 {
        return Err("max-jobs must be a positive integer".to_owned());
    }
    Ok(value)
}
#[derive(Subcommand)]
enum Jobs {
    List,
    Show {
        id: String,
    },
    Logs {
        id: String,
    },
    Wait {
        id: String,
        #[arg(long)]
        no_download: bool,
    },
    Fetch {
        id: String,
    },
    Cancel {
        id: String,
    },
    /// Resubmit an existing job using its recorded spec.
    Retry {
        id: String,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        no_download: bool,
    },
    /// Change the priority of a queued job (-100 through 100).
    Priority {
        id: String,
        #[arg(allow_hyphen_values = true, value_parser = clap::value_parser!(i32).range(-100..=100))]
        priority: i32,
    },
}
#[derive(Subcommand)]
enum Envs {
    List,
    Add {
        file: PathBuf,
    },
    Prepare {
        id: String,
        #[arg(long, required = true)]
        worker: String,
    },
    Logs {
        id: String,
    },
    Status,
    Ensure {
        id: String,
        #[arg(long, required = true)]
        worker: String,
    },
    /// List built-in presets usable as `uses: NAME@VERSION` in ylcc.yaml.
    Presets,
    /// Remove an environment from the Hub catalogue; succeeds if it is already gone.
    Delete {
        id: String,
    },
}
#[derive(Subcommand)]
enum Services {
    Install {
        #[arg(long)]
        role: String,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long, default_value = "worker")]
        id: String,
        #[arg(long, default_value = "127.0.0.1:7443")]
        listen: String,
        #[arg(long, default_value_t = 1)]
        max_jobs: usize,
        #[arg(long)]
        system_user: Option<String>,
        /// Windows: start at boot without anyone logged in (S4U task; run from an elevated shell).
        #[arg(long, conflicts_with = "system_user")]
        at_startup: bool,
        /// Worker command policy YAML (see `ylcc policy check`); workers only.
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        role: String,
        #[arg(long)]
        system_user: Option<String>,
    },
}
#[derive(Default, Serialize, Deserialize)]
struct Config {
    hub: Option<String>,
    token_file: Option<PathBuf>,
    results_dir: Option<PathBuf>,
}

fn default_worker_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::env::var("HOST"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "ylcc-worker".to_string())
}

fn stale_last_seen(last_seen: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(last_seen);
    now.saturating_sub(last_seen) > 300
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}
fn valid_release_version(version: &str) -> bool {
    if !version.starts_with('v') || version.len() > 64 || version.len() < 2 {
        return false;
    }
    version.chars().skip(1).all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

async fn resolve_latest_release_version() -> Result<String> {
    let api_url = std::env::var("YLCC_UPDATE_API_URL")
        .unwrap_or_else(|_| "https://api.github.com/repos/semanticist21/your-local-cloud-computing/releases/latest".to_string());
    let client = reqwest::Client::new();
    let response = client
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("ylcc/{}", ylcc::VERSION))
        .send()
        .await
        .context("failed to query GitHub releases")?;
    if !response.status().is_success() {
        bail!("GitHub releases request failed with {}", response.status())
    }
    let body: serde_json::Value = response.json().await.context("invalid GitHub release response")?;
    let tag = body["tag_name"]
        .as_str()
        .context("GitHub did not return tag_name")?
        .to_string();
    Ok(tag)
}

async fn update_workers(client: &Client, ids: Vec<String>, all: bool, version: Option<String>, timeout_secs: u64) -> Result<()> {
    if ids.is_empty() && !all {
        bail!("name worker IDs or pass --all")
    }
    let version = match version {
        Some(version) => version,
        None => resolve_latest_release_version().await?,
    };
    if !valid_release_version(&version) {
        bail!("invalid release version; expected a tag such as v0.4.6")
    }
    let target = version.trim_start_matches('v');
    let workers = client.get::<Vec<WorkerInfo>>("/v1/workers").await?;
    let selected: Vec<_> = if all {
        workers.iter().filter(|w| w.online).collect()
    } else {
        ids.iter()
            .map(|id| workers.iter().find(|w| &w.id == id).with_context(|| format!("unknown worker {id}")))
            .collect::<Result<_>>()?
    };
    let mut results = vec![];
    let mut failed = false;
    for worker in selected {
        let mut result = serde_json::json!({"worker": worker.id, "from": worker.version, "to": target});
        let outcome = if worker.version == target {
            Ok("current")
        } else if worker.capabilities.containerized {
            Ok("skipped: container worker; recreate it from the new release image")
        } else {
            eprintln!("updating {} to {version}", worker.id);
            update_worker(client, worker, &version, std::time::Duration::from_secs(timeout_secs)).await.map(|()| "updated")
        };
        match outcome {
            Ok(state) => result["state"] = state.into(),
            Err(error) => {
                failed = true;
                result["state"] = "failed".into();
                result["error"] = format!("{error:#}").into();
            }
        }
        results.push(result);
        if failed {
            break; // a rolling update stops at the first failure
        }
    }
    print(&results)?;
    if failed {
        bail!("worker update failed")
    }
    Ok(())
}

async fn update_worker(client: &Client, worker: &WorkerInfo, version: &str, timeout: std::time::Duration) -> Result<()> {
    ylcc::client::segment(&worker.id)?;
    let control = |paused| WorkerControl { paused: Some(paused), max_jobs: None };
    let path = format!("/v1/workers/{}", worker.id);
    if !worker.paused {
        client.post::<_, WorkerInfo>(&format!("{path}/control"), &control(true)).await?;
    }
    let result = async {
        client
            .post::<_, WorkerInfo>(&format!("{path}/update"), &UpdateRequest { version: version.into() })
            .await
            .context("request the update; the Hub must run ylcc 0.4.6 or newer")?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let current = client.get::<Vec<WorkerInfo>>("/v1/workers").await?.into_iter().find(|w| w.id == worker.id).context("worker disappeared")?;
            if let Some(error) = current.update_error {
                bail!("{error}")
            }
            if current.online && current.version == version.trim_start_matches('v') {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                bail!("timed out; the worker reports version {:?}", current.version)
            }
        }
    }
    .await;
    if !worker.paused {
        client.post::<_, WorkerInfo>(&format!("{path}/control"), &control(false)).await?;
    }
    result
}

async fn update_release(
    version: Option<String>,
    latest: bool,
    apply: bool,
    force: bool,
    hub: &str,
    token: &Path,
) -> Result<()> {
            let version = if latest {
                resolve_latest_release_version().await?
            } else {
                version.unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")))
            };
            if !valid_release_version(&version) {
                bail!("invalid release version; expected a tag such as v0.3.8 with only ASCII alphanumerics, dots, or hyphens")
            }
            if apply && !force {
                if let Ok(client) = Client::new(&hub, &token) {
                    let active: Vec<String> = match client.get::<Vec<Job>>("/v1/jobs").await {
                        Ok(jobs) => jobs
                            .into_iter()
                            .filter(|job| matches!(job.state.as_str(), "queued" | "assigned" | "running"))
                            .map(|job| job.id)
                            .collect(),
                        Err(error) => {
                            bail!("cannot verify active jobs before replacing the binary: {error:#}; drain jobs or pass --force")
                        }
                    };
                    if !active.is_empty() {
                        bail!("active jobs remain ({}); pause/finish them or pass --force", active.join(", "))
                    }
                } else {
                    bail!("cannot verify active jobs before replacing the binary; configure the Hub and token or pass --force")
                }
            }
            let is_windows = cfg!(target_os = "windows");
            let command = if is_windows {
                format!(
                    "$env:YLCC_VERSION='{}'; irm https://raw.githubusercontent.com/semanticist21/your-local-cloud-computing/{}/scripts/install.ps1 | iex",
                    version, version
                )
            } else {
                format!(
                    "YLCC_VERSION={} curl -fsSL https://raw.githubusercontent.com/semanticist21/your-local-cloud-computing/{}/scripts/install.sh | sh -s -- --version {}",
                    version, version, version
                )
            };
            if apply {
                eprintln!("running verified installer for {version}");
                let status = if is_windows {
                    std::process::Command::new("powershell")
                        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &command])
                        .status()
                        .context("failed to launch the Windows installer")?
                } else {
                    std::process::Command::new("sh")
                        .args(["-c", &command])
                        .status()
                        .context("failed to launch the POSIX installer")?
                };
                if !status.success() {
                    bail!("installer exited with status {status}")
                }
            }
            print(&serde_json::json!({
                "current_version": ylcc::VERSION,
                "target_version": version.trim_start_matches('v'),
                "preserved": ["config.json", "token file", "results_dir", "worker data", "hub data", "background service IDs unless you reinstall"],
                "command": command,
                "mode": if apply {"applied"} else {"planned"},
                "note": "Drain active jobs and stop hub/worker services before replacing a running binary; verify with `ylcc --version` afterward."
            }))?;
            Ok(())
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("APPDATA")
            } else {
                None
            }
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("ylcc/config.json")
}
fn data_path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA")
            } else {
                None
            }
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("ylcc")
}
fn pairs(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    values
        .into_iter()
        .map(|s| {
            let (k, v) = s.split_once('=').context("expected NAME=VALUE")?;
            if k.is_empty() {
                bail!("empty name")
            };
            Ok((k.to_owned(), v.to_owned()))
        })
        .collect()
}
/// Fails with an upgrade hint when the Hub does not advertise `capability` in /health.
async fn require_capability(client: &Client, capability: &str, feature: &str) -> Result<()> {
    let health: serde_json::Value = client.get("/health").await?;
    if !health["capabilities"]
        .as_array()
        .is_some_and(|capabilities| capabilities.iter().any(|c| c == capability))
    {
        bail!("this Hub does not support {feature}; upgrade the Hub first")
    }
    Ok(())
}
fn print<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolve path {}", path.display()))
}

fn resolve_settings(cli: &Cli, cfg: Config, cfg_path: &Path) -> (String, PathBuf, PathBuf) {
    let hub = cli
        .hub
        .clone()
        .or(cfg.hub)
        .unwrap_or_else(|| "http://127.0.0.1:7443".into());
    let token = cli
        .token_file
        .clone()
        .or(cfg.token_file)
        .unwrap_or_else(|| cfg_path.parent().unwrap().join("token"));
    let results = cli
        .results_dir
        .clone()
        .or(cfg.results_dir)
        .unwrap_or_else(|| data_path().join("results"));
    (hub, token, results)
}

/// Uploads the snapshot once and submits one job per spec. Returns final jobs, or submitted ones with `detach`.
async fn submit(
    client: &Client,
    snap: snapshot::Snapshot,
    specs: Vec<JobSpec>,
    results: &Path,
    detach: bool,
    no_download: bool,
) -> Result<Vec<Job>> {
    for spec in &specs {
        spec.validate()?;
    }
    if specs.iter().any(|spec| spec.priority != 0) {
        let health: serde_json::Value = client.get("/health").await?;
        let supported = health["capabilities"]
            .as_array()
            .is_some_and(|capabilities| {
                capabilities
                    .iter()
                    .any(|capability| capability == "job_priority")
            });
        if !supported {
            bail!(
                "this Hub does not support job priority; upgrade the Hub before submitting a nonzero priority"
            )
        }
    }
    let (count, bytes) = client.upload(&snap).await?;
    eprintln!(
        "uploaded {count} missing blobs ({bytes} bytes); {} selected files",
        snap.files.len()
    );
    let mut jobs = Vec::new();
    for spec in &specs {
        match client.post::<_, Job>("/v1/jobs", spec).await {
            Ok(job) => {
                eprintln!("job {} ({})", job.id, job.state);
                jobs.push(job);
            }
            // Keep already-submitted jobs trackable on stdout instead of orphaning them.
            Err(error) if !jobs.is_empty() => {
                report(&jobs, true)?;
                return Err(error.context("a later target was not submitted; the jobs above are running"));
            }
            Err(error) => return Err(error),
        }
    }
    if detach {
        return Ok(jobs);
    }
    let mut done = Vec::new();
    for job in jobs {
        done.push(client.wait(&job.id, results, !no_download).await?);
    }
    Ok(done)
}

/// A `--save-task` input relative to the project directory, as `ylcc.yaml` requires.
fn task_input(cwd: &Path, input: &Path) -> Result<PathBuf> {
    let absolute = if input.is_absolute() { input.to_path_buf() } else { cwd.join(input) };
    let mut relative = PathBuf::new();
    for part in absolute
        .strip_prefix(cwd)
        .with_context(|| format!("input {} is outside the project directory", input.display()))?
        .components()
    {
        match part {
            std::path::Component::Normal(part) => relative.push(part),
            std::path::Component::CurDir => {}
            _ => bail!("input {} must stay inside the project directory", input.display()),
        }
    }
    Ok(if relative.as_os_str().is_empty() { ".".into() } else { relative })
}

/// Idempotent: an unchanged recipe keeps its revision; workers prepare it on first use.
async fn register(client: &Client, environment: &Environment) -> Result<()> {
    client
        .post::<_, Environment>("/v1/environments", environment)
        .await
        .with_context(|| format!("register environment {}", environment.id))?;
    Ok(())
}

/// Prints one job as an object (the single-target shape) or several as an array, failing on any unsuccessful job.
fn report(jobs: &[Job], detach: bool) -> Result<()> {
    match jobs {
        [job] => print(job)?,
        _ => print(&jobs)?,
    }
    if !detach {
        let failed: Vec<_> = jobs
            .iter()
            .filter(|job| job.state != "succeeded")
            .map(|job| format!("job {} ended {}", job.id, job.state))
            .collect();
        if !failed.is_empty() {
            bail!("{}", failed.join("; "))
        }
    }
    Ok(())
}

async fn diagnostics(hub: &str, token: &Path, local: Capabilities) -> Result<serde_json::Value> {
    let mut value = serde_json::json!({"local":local});
    let workers = match Client::new(hub, token) {
        Ok(client) => client.get::<Vec<WorkerInfo>>("/v1/workers").await,
        Err(error) => Err(error),
    };
    match workers {
        Ok(workers) => value["workers"] = serde_json::to_value(workers)?,
        Err(error) => value["hub_error"] = serde_json::Value::String(format!("{error:#}")),
    }
    Ok(value)
}

fn task_template(runtime: &str, name: &str, os: &str, arch: &str) -> Result<String> {
    // Preset versions are known-good starting points; bump them in the generated file.
    let (command, inputs, preset, cache) = match runtime {
        "rust" => ("[cargo, test, --locked]", "inputs: [Cargo.toml, Cargo.lock, src, tests]", Some("rust@1.96.0"), "[target]"),
        "node" => ("[npm, test]", "inputs: [package.json, package-lock.json, src, test]", Some("node@22.12.0"), "[node_modules]"),
        "python" => ("[python, -m, pytest, -q]", "inputs: [pyproject.toml, src, tests]", None, ""),
        "bun" => ("[bun, test]", "inputs: [package.json, bun.lock, src, tests]", Some("bun@1.3.14"), "[node_modules]"),
        "go" => ("[go, test, ./...]", "inputs: [go.mod, go.sum, .]", Some("go@1.23.4"), ""),
        _ => ("[true]", "no_input: true", None, ""),
    };
    let mut text = String::from("version: 1\n");
    if let Some(preset) = preset {
        text += &format!("environments:\n  {runtime}: {{uses: {preset}}}\n");
    }
    text += &format!("defaults:\n  requirements: {{os: {os}, arch: {arch}}}\n  timeout_secs: 1800\n");
    if preset.is_some() {
        text += &format!("  environment: {runtime}\n");
    }
    text += &format!("tasks:\n  {name}:\n    command: {command}\n    {inputs}\n");
    if !cache.is_empty() {
        text += &format!("    cache: {cache}\n");
    }
    Ok(text)
}

#[tokio::main]
async fn main() {
    if let Err(e) = execute().await {
        eprintln!("{e:#}");
        std::process::exit(1)
    }
}
async fn execute() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = config_path();
    let cfg: Config = if cfg_path.exists() {
        serde_json::from_slice(&fs::read(&cfg_path)?)?
    } else {
        Config::default()
    };
    let (hub, token, results) = resolve_settings(&cli, cfg, &cfg_path);
    match cli.command {
        Commands::Init => {
            let parent = token
                .parent()
                .context("token file needs a parent directory")?;
            fs::create_dir_all(parent)?;
            let mut f = platform::create_private_file(&token).context(
                "token file exists or cannot be created; existing tokens are never replaced",
            )?;
            writeln!(
                f,
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )?;
            print(&serde_json::json!({"token_file":token}))
        }
        Commands::Tokens { command } => {
            let client = Client::new(&hub, &token)?;
            require_capability(&client, "scoped_tokens", "scoped tokens").await?;
            match command {
                Tokens::Create {
                    name,
                    role,
                    worker_id,
                    out,
                } => {
                    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                        fs::create_dir_all(parent)?;
                    }
                    // Claim the file first so a created token always has somewhere to live.
                    let mut file = platform::create_private_file(&out).with_context(|| {
                        format!("{} exists or cannot be created; token files are never replaced", out.display())
                    })?;
                    let request = TokenRequest {
                        name,
                        role,
                        worker_id,
                    };
                    let created = match client.post::<_, CreatedToken>("/v1/tokens", &request).await {
                        Ok(created) => created,
                        Err(error) => {
                            drop(file);
                            let _ = fs::remove_file(&out);
                            return Err(error);
                        }
                    };
                    writeln!(file, "{}", created.secret)?;
                    file.sync_all()?;
                    print(&serde_json::json!({"token": created.token, "token_file": absolute_path(&out)?}))
                }
                Tokens::List => print(&client.get::<Vec<TokenInfo>>("/v1/tokens").await?),
                Tokens::Revoke { id } => {
                    ylcc::client::segment(&id)?;
                    let revoked: TokenInfo = client.delete_json(&format!("/v1/tokens/{id}")).await?;
                    print(&serde_json::json!({"revoked": revoked}))
                }
            }
        }
        Commands::Policy { command } => match command {
            PolicyCommand::Check { file } => {
                let policy = Policy::load(&file)?;
                print(&serde_json::json!({
                    "policy_file": absolute_path(&file)?,
                    "categories": policy.summary(),
                    "warnings": policy.warnings,
                }))
            }
            PolicyCommand::Suggest {
                file,
                task,
                environment,
                os,
                allow_path_scoped,
            } => {
                let mut specs = vec![];
                let mut environments: Vec<Environment> = vec![];
                let mut notes = vec![];
                if file.exists() || environment.is_empty() {
                    let (project, root) = Project::load(&file)?;
                    let names: Vec<String> = if task.is_empty() { project.tasks.keys().cloned().collect() } else { task };
                    let path_scope = format!("-{}", ylcc::project::project_scope(None, &root));
                    for name in names {
                        let plan = project.plan(&name, &root, vec![])?;
                        for environment in plan.environments {
                            if project.name.is_none() && environment.id.ends_with(&path_scope) {
                                if !allow_path_scoped {
                                    bail!(
                                        "environment {} is named after this checkout's path because {} has no `name:`; add a project name so every checkout gets the same id, or pass --allow-path-scoped",
                                        environment.id,
                                        file.display()
                                    )
                                }
                                notes.push(format!("environment {} only matches checkouts at this path", environment.id));
                            }
                            if !environments.iter().any(|e| e.id == environment.id) {
                                environments.push(environment);
                            }
                        }
                        specs.extend(plan.specs);
                    }
                }
                for name in environment {
                    let preset = ylcc::project::preset_environment(&name, os.as_deref())?
                        .with_context(|| format!("{name} is not a preset; use NAME@VERSION from `ylcc env presets`"))?;
                    if !environments.iter().any(|e| e.id == preset.id) {
                        environments.push(preset);
                    }
                }
                let (yaml, more) = ylcc::policy::suggest(&specs, &environments);
                notes.extend(more);
                notes.push("preset digests change when a new ylcc release changes a preset recipe; re-run this command after upgrading".into());
                for note in notes {
                    eprintln!("note: {note}");
                }
                print!("{yaml}");
                Ok(())
            }
        },
        Commands::Audit { limit } => {
            let client = Client::new(&hub, &token)?;
            require_capability(&client, "audit_log", "the audit log").await?;
            print(
                &client
                    .get::<Vec<AuditEvent>>(&format!("/v1/audit?limit={}", limit.clamp(1, 1000)))
                    .await?,
            )
        }
        Commands::Configure => {
            ylcc::client::validate_hub(&hub)?;
            ylcc::client::read_token(&token)?;
            let token = absolute_path(&token)?;
            let results = absolute_path(&results)?;
            platform::private_dir(cfg_path.parent().unwrap())?;
            let mut temp = tempfile::NamedTempFile::new_in(cfg_path.parent().unwrap())?;
            platform::protect_file(temp.as_file())?;
            temp.write_all(&serde_json::to_vec_pretty(&Config {
                hub: Some(hub),
                token_file: Some(token),
                results_dir: Some(results),
            })?)?;
            temp.persist(&cfg_path)?;
            print(&serde_json::json!({"config_file":cfg_path}))
        }
        Commands::Hub { listen, data_dir } => {
            ylcc::hub::run(
                listen,
                data_dir.unwrap_or_else(|| data_path().join("hub")),
                token,
            )
            .await
        }
        Commands::Worker {
            id,
            data_dir,
            max_jobs,
            policy,
        } => {
            let id = id.unwrap_or_else(default_worker_id);
            let policy = policy.map(|path| Policy::load(&path)).transpose()?;
            ylcc::worker::run(
                hub,
                token,
                data_dir.unwrap_or_else(|| data_path().join("worker")),
                id,
                max_jobs,
                policy,
            )
            .await
        }
        Commands::Manifest(s) => {
            let snap = s.snapshot()?;
            print(&serde_json::json!({
                "files": snap.files,
                "excluded": snap.excluded,
                "summary": {
                    "selected_files": snap.files.len(),
                    "excluded_paths": snap.excluded.len(),
                    "note": "The request path is a snapshot, not a live project: submit again after source changes."
                }
            }))
        }
        Commands::Run(r) => {
            let client = Client::new(&hub, &token)?;
            let snap = r.selection.snapshot()?;
            let requirements = Requirements {
                os: r.os,
                arch: r.arch.map(|v| normalize_arch(&v)),
                abi: r.abi,
                tools: pairs(r.tool)?,
                gpu: r.gpu,
                cpu_features: r.cpu_feature,
                build_target: r.build_target,
                emulation: false,
            };
            let cwd = platform::command_path(&std::env::current_dir()?.canonicalize()?);
            let project_file = cwd.join("ylcc.yaml");
            // A run inside a project shares the project's cache scope, so a saved task stays warm.
            let project_name = fs::read_to_string(&project_file)
                .ok()
                .and_then(|text| Project::parse(&text).ok())
                .and_then(|project| project.name);
            let preset = r
                .environment
                .as_deref()
                .map(|name| ylcc::project::preset_environment(name, requirements.os.as_deref()))
                .transpose()?
                .flatten();
            let spec = JobSpec {
                command: r.command.clone(),
                inputs: snap.files.clone(),
                outputs: r.output.clone(),
                requirements: requirements.clone(),
                environment: preset.as_ref().map(|e| e.id.clone()).or(r.environment.clone()),
                container: r.container.clone(),
                env: pairs(r.env)?,
                timeout_secs: r.timeout,
                priority: r.priority,
                cache_scope: (!r.cache.is_empty())
                    .then(|| ylcc::project::project_scope(project_name.as_deref(), &cwd)),
                cache: r.cache.clone(),
            };
            let saved = match r.save_task {
                Some(name) => {
                    let task = ylcc::project::Task {
                        command: r.command,
                        inputs: r
                            .selection
                            .input
                            .iter()
                            .map(|input| task_input(&cwd, input))
                            .collect::<Result<_>>()?,
                        project: r.selection.project,
                        no_input: r.selection.no_input,
                        outputs: r.output,
                        requirements: Some(requirements),
                        environment: r.environment,
                        container: r.container,
                        env: spec.env.clone(),
                        timeout_secs: r.timeout,
                        priority: (r.priority != 0).then_some(r.priority),
                        cache: (!r.cache.is_empty()).then_some(r.cache),
                        ..Default::default()
                    };
                    // Check before submitting, so a run that succeeds is never left unsaveable.
                    let text = Project::with_task(&project_file, &name, &task)?;
                    let planned = Project::parse(&text)?.plan(&name, &cwd, snap.files.clone())?;
                    let (want, got) = (serde_json::to_value(&spec)?, serde_json::to_value(&planned.specs)?);
                    if got.as_array().is_none_or(|specs| specs.len() != 1 || specs[0] != want) {
                        bail!(
                            "ylcc.yaml defaults would change this run once saved (planned {got}); pass matching options or add the task manually"
                        )
                    }
                    Some((name, task))
                }
                None => None,
            };
            if let Some(environment) = &preset {
                register(&client, environment).await?;
            }
            let jobs = submit(&client, snap, vec![spec], &results, r.detach, r.no_download).await?;
            let mut save = Ok(());
            if let Some((name, task)) = saved
                && (r.detach || jobs.iter().all(|job| job.state == "succeeded"))
            {
                save = Project::save_task(&project_file, &name, &task);
                if save.is_ok() {
                    eprintln!("saved task {name} to ylcc.yaml; rerun with: ylcc task run {name}");
                }
            }
            report(&jobs, r.detach)?;
            save.context("save task")
        }
        Commands::Task { command } => match command {
            Tasks::List { file } => {
                let (project, _) = Project::load(&file)?;
                print(&project.tasks)
            }
            Tasks::Delete { name, file } => {
                let deleted = Project::delete_task(&file, &name)?;
                print(&serde_json::json!({"task": name, "deleted": deleted, "file": absolute_path(&file)?}))
            }
            Tasks::Init { file, runtime, name, os, arch, force } => {
                let existed_before = file.exists();
                if existed_before && !force {
                    bail!("project file already exists: {}; pass --force to overwrite", file.display())
                }
                let parent = file.parent().unwrap_or(Path::new("."));
                fs::create_dir_all(parent)?;
                let template = task_template(runtime.as_str(), name.as_str(), os.as_str(), arch.as_str())?;
                fs::write(&file, template)?;
                print(&serde_json::json!({
                    "file": absolute_path(&file)?,
                    "runtime": runtime,
                    "name": name,
                    "overwritten": force && existed_before
                }))?;
                Ok(())
            }
            Tasks::Run {
                name,
                file,
                detach,
                no_download,
            } => {
                let (project, root) = Project::load(&file)?;
                let task = project.tasks.get(&name).with_context(|| {
                    format!(
                        "unknown task {name}; use ylcc task list --file {}",
                        file.display()
                    )
                })?;
                let snap = task.snapshot(&root)?;
                let plan = project.plan(&name, &root, snap.files.clone())?;
                let client = Client::new(&hub, &token)?;
                for environment in &plan.environments {
                    register(&client, environment).await?;
                }
                let jobs = submit(&client, snap, plan.specs, &results, detach, no_download).await?;
                report(&jobs, detach)
            }
        },
        Commands::Workers { command } => {
            let client = Client::new(&hub, &token)?;
            let (id, control) = match command {
                None => return print(&client.get::<Vec<WorkerInfo>>("/v1/workers").await?),
                Some(Workers::Pause { id }) => (
                    id,
                    WorkerControl {
                        paused: Some(true),
                        max_jobs: None,
                    },
                ),
                Some(Workers::Resume { id }) => (
                    id,
                    WorkerControl {
                        paused: Some(false),
                        max_jobs: None,
                    },
                ),
                Some(Workers::Limit { id, max_jobs }) => (
                    id,
                    WorkerControl {
                        paused: None,
                        max_jobs: Some(max_jobs),
                    },
                ),
                Some(Workers::Delete { id }) => {
                    ylcc::client::segment(&id)?;
                    client.delete(&format!("/v1/workers/{}", id)).await?;
                    return print(&serde_json::json!({"deleted": id}));
                }
                Some(Workers::Update { ids, all, version, timeout_secs }) => {
                    return update_workers(&client, ids, all, version, timeout_secs).await;
                }
            };
            ylcc::client::segment(&id)?;
            print(
                &client
                    .post::<_, WorkerInfo>(&format!("/v1/workers/{id}/control"), &control)
                    .await
                    .context("control worker; this command requires a Hub with queue controls")?,
            )
        }
        Commands::Jobs { command } => {
            let c = Client::new(&hub, &token)?;
            match command {
                Jobs::List => print(&c.get::<Vec<Job>>("/v1/jobs").await?),
                Jobs::Show { id } => print(&c.job(&id).await?),
                Jobs::Logs { id } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c.get::<Vec<LogEntry>>(&format!("/v1/jobs/{id}/logs"))
                            .await?,
                    )
                }
                Jobs::Wait { id, no_download } => {
                    let j = c.wait(&id, &results, !no_download).await?;
                    print(&j)?;
                    if j.state != "succeeded" {
                        bail!("job {} ended {}", j.id, j.state)
                    }
                    Ok(())
                }
                Jobs::Fetch { id } => {
                    let j = c.job(&id).await?;
                    print(&serde_json::json!({"results_dir":c.fetch(&j,&results).await?}))
                }
                Jobs::Cancel { id } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c.post::<_, Job>(&format!("/v1/jobs/{id}/cancel"), &serde_json::json!({}))
                            .await?,
                    )
                }
                Jobs::Retry { id, detach, no_download } => {
                    ylcc::client::segment(&id)?;
                    let previous = c.job(&id).await?;
                    if !matches!(previous.state.as_str(), "succeeded" | "failed" | "cancelled") {
                        bail!("job {} is not terminal ({})", id, previous.state)
                    }
                    if previous.spec.priority != 0 {
                        let health: serde_json::Value = c.get("/health").await?;
                        let supported = health["capabilities"]
                            .as_array()
                            .is_some_and(|capabilities| {
                                capabilities
                                    .iter()
                                    .any(|capability| capability == "job_priority")
                            });
                        if !supported {
                            bail!("this Hub does not support job priority; upgrade the Hub before retrying a nonzero-priority job")
                        }
                    }
                    let job: Job = c.post("/v1/jobs", &previous.spec).await?;
                    if detach {
                        print(&job)
                    } else {
                        let j = c.wait(&job.id, &results, !no_download).await?;
                        print(&j)?;
                        if j.state != "succeeded" {
                            bail!("job {} finished with state {}", j.id, j.state)
                        }
                        Ok(())
                    }
                }
                Jobs::Priority { id, priority } => {
                    ylcc::client::segment(&id)?;
                    print(&c.post::<_, Job>(&format!("/v1/jobs/{id}/priority"), &PriorityRequest {priority}).await.context("set queued job priority; this command requires a Hub with queue controls")?)
                }
            }
        }
        Commands::Env { command } => {
            let c = || Client::new(&hub, &token);
            match command {
                Envs::List => print(&c()?.get::<Vec<Environment>>("/v1/environments").await?),
                Envs::Status => print(
                    &c()?.get::<Vec<EnvironmentStatus>>("/v1/environments/status")
                        .await?,
                ),
                Envs::Add { file } => {
                    let e: Environment = serde_json::from_slice(&fs::read(file)?)?;
                    print(&c()?.post::<_, Environment>("/v1/environments", &e).await?)
                }
                Envs::Logs { id } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c()?.get::<Vec<LogEntry>>(&format!("/v1/environments/{id}/logs"))
                            .await?,
                    )
                }
                Envs::Prepare { id, worker } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c()?.post::<_, Vec<EnvironmentStatus>>(
                            &format!("/v1/environments/{id}/prepare"),
                            &PrepareRequest { worker_id: Some(worker) },
                        )
                        .await?,
                    )
                }
                Envs::Delete { id } => {
                    ylcc::client::segment(&id)?;
                    c()?.delete(&format!("/v1/environments/{id}")).await?;
                    print(&serde_json::json!({"deleted": id}))
                }
                Envs::Presets => print(
                    &ylcc::project::PRESETS
                        .iter()
                        .map(|name| serde_json::json!({"name": name, "uses": format!("{name}@VERSION")}))
                        .collect::<Vec<_>>(),
                ),
                Envs::Ensure { id, worker } => {
                    ylcc::client::segment(&id)?;
                    let revision = c()?.get::<Vec<Environment>>("/v1/environments").await?.into_iter().find(|e| e.id == id).map(|e| e.revision);
                    let statuses = c()?.get::<Vec<EnvironmentStatus>>("/v1/environments/status").await?;
                    // Reuse only the current revision already ready on the requested worker.
                    let current = statuses.into_iter().find(|s| s.environment_id == id && s.worker_id == worker && Some(&s.revision) == revision.as_ref() && s.state == "ready");
                    if current.is_none() {
                        let prepared = c()?.post::<_, Vec<EnvironmentStatus>>(&format!("/v1/environments/{id}/prepare"), &PrepareRequest { worker_id: Some(worker.clone()) }).await?;
                        print(&serde_json::json!({"environment": id, "action": "prepared", "statuses": prepared}))
                    } else {
                        print(&serde_json::json!({"environment": id, "action": "reused", "status": current}))
                    }
                }
            }
        }
        Commands::Status => {
            let client = Client::new(&hub, &token)?;
            let workers = client.get::<Vec<WorkerInfo>>("/v1/workers").await.unwrap_or_default();
            let jobs = client.get::<Vec<Job>>("/v1/jobs").await.unwrap_or_default();
            let health = client.get::<serde_json::Value>("/health").await.ok();
            print(&serde_json::json!({
                "hub": hub,
                "health": health,
                "workers": workers,
                "summary": {
                    "workers": workers.len(),
                    "online_workers": workers.iter().filter(|w| !stale_last_seen(w.last_seen)).count(),
                    "workers_with_unknown_heartbeat": workers.iter().filter(|w| stale_last_seen(w.last_seen)).count(),
                    "total_jobs": jobs.len(),
                    "queued": jobs.iter().filter(|j| j.state == "queued").count(),
                    "assigned": jobs.iter().filter(|j| j.state == "assigned").count(),
                    "running": jobs.iter().filter(|j| j.state == "running").count(),
                    "succeeded": jobs.iter().filter(|j| j.state == "succeeded").count(),
                    "failed": jobs.iter().filter(|j| j.state == "failed").count(),
                }
            }))?;
            Ok(())
        }
        Commands::Setup => {
            let mut actions = Vec::new();
            if !token.exists() { actions.push("ylcc init"); }
            if !cfg_path.exists() { actions.push("ylcc configure"); }
            if !results.exists() { actions.push("mkdir -p <results_dir>"); }
            actions.push("ylcc doctor");
            actions.push("ylcc examples");
            let setup = serde_json::json!({
                "schema": "first-run-v1",
                "config_file": cfg_path,
                "config_exists": cfg_path.exists(),
                "token_file": token,
                "token_exists": token.exists(),
                "results_dir": results,
                "results_exists": results.exists(),
                "next_actions": actions,
                "hint": "Secret values are never printed. Run ylcc configure once the token exists."
            });
            print(&setup)?;
            Ok(())
        }
        Commands::Examples => {
            print(&serde_json::json!({
                "examples": [
                    {
                        "prompt": "Check the pool and workers, but do not run anything.",
                        "commands": ["ylcc --json status", "ylcc --json workers"]
                    },
                    {
                        "prompt": "Run this only on a powered Windows x86_64 worker.",
                        "commands": ["ylcc --json workers", "ylcc run --os windows --arch x86_64 --no-input --json -- powershell -NoProfile -Command \"...\""]
                    },
                    {
                        "prompt": "Build this on Linux x86_64 and bring back the binary.",
                        "commands": ["ylcc --json manifest --project", "ylcc run --input Cargo.toml --input Cargo.lock --input src --os linux --arch x86_64 --output target/release/my-tool --detach -- cargo build --locked --release", "ylcc jobs wait JOB_ID"]
                    },
                    {
                        "prompt": "Use the reviewed project task.",
                        "commands": ["ylcc --json task list", "ylcc task run TASK --detach", "ylcc --json jobs wait JOB_ID"]
                    },
                    {
                        "prompt": "Open the dashboard without giving me the token in chat.",
                        "commands": ["ylcc dashboard --open"]
                    }
                ]
            }))?;
            Ok(())
        }
        Commands::Doctor => {
            let mut diag = diagnostics(&hub, &token, ylcc::worker::probe().await?).await?;
            if let serde_json::Value::Object(map) = &mut diag {
                map.insert("ylcc_version".into(), serde_json::Value::String(ylcc::VERSION.into()));
                map.insert("config_file".into(), serde_json::Value::String(config_path().display().to_string()));
                map.insert("token_file".into(), serde_json::Value::String(token.display().to_string()));
                map.insert("results_dir".into(), serde_json::Value::String(results.display().to_string()));
            }
            print(&diag)
        }
        Commands::Update { version, latest, apply, force } => {
            update_release(version, latest, apply, force, &hub, &token).await
        }
        Commands::Upgrade { version, latest, force } => {
            let latest = latest || version.is_none();
            update_release(version, latest, true, force, &hub, &token).await
        }
        Commands::Cleanup { older_than_days, apply } => {
            use std::time::{Duration, SystemTime};
            let retention_seconds = older_than_days.checked_mul(86400).context("retention period is too large")?;
            let cutoff = SystemTime::now()
                .checked_sub(Duration::from_secs(retention_seconds))
                .context("invalid retention period")?;
            let active_job_ids: std::collections::BTreeSet<String> = {
                let client = Client::new(&hub, &token).context("failed to connect to the Hub, active jobs cannot be enumerated")?;
                client
                    .get::<Vec<Job>>("/v1/jobs")
                    .await
                    .context("failed to query active Hub jobs, cleanup aborted")?
                    .into_iter()
                    .filter(|job| matches!(job.state.as_str(), "queued" | "assigned" | "running"))
                    .map(|job| job.id)
                    .collect()
            };
            let mut skipped_active = Vec::new();
            let mut candidates = Vec::new();
            if results.exists() {
                for entry in fs::read_dir(&results)? {
                    let entry = entry?;
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if active_job_ids.contains(name) {
                            skipped_active.push(name.to_string());
                            continue;
                        }
                    }
                    let modified = entry.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    if modified < cutoff {
                        candidates.push((path, modified));
                    }
                }
            }
            candidates.sort_by(|a,b| a.0.cmp(&b.0));
            if apply {
                for (path, _) in &candidates {
                    fs::remove_dir_all(path)
                        .with_context(|| format!("remove {}", path.display()))?;
                }
            }
            print(&serde_json::json!({
                "results_dir": results,
                "older_than_days": older_than_days,
                "mode": if apply {"applied"} else {"dry-run"},
                "candidate_count": candidates.len(),
                "candidates": candidates.iter().map(|(p,_)| p.display().to_string()).collect::<Vec<_>>(),
                "skipped_active_jobs": skipped_active,
                "note": if apply {"Removed. Rerun without --apply to preview."} else {"No files were deleted; rerun with --apply to remove."}
            }))?;
            Ok(())
        }
        Commands::Dashboard { open, no_open } => {
            let open = open || !no_open;
            let url = format!("{}/", hub.trim_end_matches('/'));
            let browser_url = if open {
                match fs::read_to_string(&token) {
                    Ok(value) => {
                        let value = value.trim();
                        if !value.is_empty() {
                            format!("{}#token={}", url.trim_end_matches('/'), value)
                        } else {
                            url.clone()
                        }
                    }
                    _ => url.clone(),
                }
            } else {
                url.clone()
            };
            print(&serde_json::json!({
                "dashboard_url": url,
                "token_file": token,
                "token_guidance": "The browser is opened with the token in the URL fragment when possible. On shared/public machines, use --no-open and paste the token manually to avoid browser history.",
            }))?;
            if open {
                #[cfg(target_os = "macos")]
                let opened = std::process::Command::new("open").arg(&browser_url).status().is_ok();
                #[cfg(target_os = "windows")]
                let opened = std::process::Command::new("cmd").args(["/C", "start", "", &browser_url]).status().is_ok();
                #[cfg(target_os = "linux")]
                let opened = std::process::Command::new("xdg-open").arg(&browser_url).status().is_ok();
                #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                let opened = false;
                if !opened {
                    eprintln!("could not open a browser automatically; open dashboard_url manually");
                }
            }
            Ok(())
        }
        Commands::Service { command } => match command {
            Services::Install {
                role,
                data_dir,
                id,
                listen,
                max_jobs,
                system_user,
                at_startup,
                policy,
            } => {
                let dir = data_dir.unwrap_or_else(|| data_path().join(&role));
                let policy = policy.as_deref();
                if let Some(user) = system_user {
                    ylcc::service::install_system_user(
                        &role, &hub, &token, &dir, &id, &listen, max_jobs, policy, &user,
                    )
                } else if at_startup {
                    ylcc::service::install_at_startup(&role, &hub, &token, &dir, &id, &listen, max_jobs, policy)
                } else {
                    ylcc::service::install(&role, &hub, &token, &dir, &id, &listen, max_jobs, policy)
                }
            }
            Services::Uninstall { role, system_user } => {
                if let Some(user) = system_user {
                    ylcc::service::uninstall_system_user(&role, &user)
                } else {
                    ylcc::service::uninstall(&role)
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn token_and_audit_commands_parse() {
        let cli = Cli::try_parse_from([
            "ylcc", "tokens", "create", "--name", "builder", "--role", "worker", "--worker-id",
            "builder", "--out", "builder.token",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Tokens { command: Tokens::Create { role: Role::Worker, worker_id: Some(_), .. } }
        ));
        assert!(Cli::try_parse_from(["ylcc", "tokens", "create", "--name", "x", "--role", "root", "--out", "f"]).is_err());
        assert!(Cli::try_parse_from(["ylcc", "tokens", "create", "--name", "x", "--role", "requester"]).is_err());
        assert!(Cli::try_parse_from(["ylcc", "tokens", "list"]).is_ok());
        assert!(Cli::try_parse_from(["ylcc", "tokens", "revoke", "t123"]).is_ok());
        assert!(matches!(
            Cli::try_parse_from(["ylcc", "audit", "--limit", "5"]).unwrap().command,
            Commands::Audit { limit: 5 }
        ));
    }
    #[test]
    fn existing_commands_and_negative_priorities_parse() {
        let cli = Cli::try_parse_from(["ylcc", "workers"]).unwrap();
        assert!(matches!(cli.command, Commands::Workers { command: None }));
        let cli = Cli::try_parse_from([
            "ylcc",
            "run",
            "--no-input",
            "--priority",
            "-5",
            "--",
            "echo",
            "ok",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run(RunArgs { priority: -5, .. })
        ));
        assert!(
            Cli::try_parse_from([
                "ylcc",
                "run",
                "--no-input",
                "--priority",
                "101",
                "--",
                "true"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["ylcc", "jobs", "priority", "job-id", "-100"]).is_ok());
        assert!(
            Cli::try_parse_from(["ylcc", "workers", "limit", "worker-id", "--max-jobs", "0"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["ylcc", "workers", "limit", "worker-id", "--max-jobs", "2"])
                .is_ok()
        );
    }

    #[test]
    fn explicit_connection_options_override_saved_defaults_for_named_tasks() {
        let cli = Cli::try_parse_from([
            "ylcc",
            "--hub",
            "https://override.example",
            "--token-file",
            "new-token",
            "--results-dir",
            "new-results",
            "task",
            "run",
            "build",
            "--file",
            "other/ylcc.yaml",
            "--detach",
            "--no-download",
        ])
        .unwrap();
        let cfg = Config {
            hub: Some("https://saved.example".into()),
            token_file: Some("saved-token".into()),
            results_dir: Some("saved-results".into()),
        };
        let (hub, token, results) =
            resolve_settings(&cli, cfg, Path::new("/config/ylcc/config.json"));
        assert_eq!(hub, "https://override.example");
        assert_eq!(token, Path::new("new-token"));
        assert_eq!(results, Path::new("new-results"));
        assert!(
            matches!(cli.command, Commands::Task {command: Tasks::Run {ref name, ref file, detach: true, no_download: true}} if name == "build" && file == Path::new("other/ylcc.yaml"))
        );
        let cli = Cli::try_parse_from(["ylcc", "task", "list"]).unwrap();
        let cfg = Config {
            hub: Some("https://saved.example".into()),
            token_file: Some("saved-token".into()),
            results_dir: Some("saved-results".into()),
        };
        let (hub, token, results) =
            resolve_settings(&cli, cfg, Path::new("/config/ylcc/config.json"));
        assert_eq!(hub, "https://saved.example");
        assert_eq!(token, Path::new("saved-token"));
        assert_eq!(results, Path::new("saved-results"));
    }

    #[test]
    fn configuration_paths_are_independent_of_later_working_directories() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            absolute_path(Path::new("pool.token")).unwrap(),
            current.join("pool.token")
        );
        assert_eq!(
            absolute_path(Path::new("results")).unwrap(),
            current.join("results")
        );
        assert_eq!(absolute_path(&current).unwrap(), current);
    }

    fn test_token() -> tempfile::TempPath {
        let mut token = tempfile::NamedTempFile::new().unwrap();
        platform::protect_file(token.as_file()).unwrap();
        writeln!(token, "{}", "a".repeat(64)).unwrap();
        token.into_temp_path()
    }

    #[test]
    fn task_inputs_become_project_relative() {
        let cwd = Path::new("/work/app");
        assert_eq!(task_input(cwd, Path::new("./src")).unwrap(), Path::new("src"));
        assert_eq!(task_input(cwd, Path::new("/work/app/a/b")).unwrap(), Path::new("a/b"));
        assert_eq!(task_input(cwd, Path::new(".")).unwrap(), Path::new("."));
        assert!(task_input(cwd, Path::new("../other")).is_err());
        assert!(task_input(cwd, Path::new("/elsewhere")).is_err());
    }
    #[tokio::test]
    async fn doctor_preserves_local_information_when_the_hub_request_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/v1/workers",
            axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = test_token();
        let value = diagnostics(&hub, &token, Capabilities::default())
            .await
            .unwrap();
        server.abort();
        assert!(value["local"].is_object());
        assert!(value["hub_error"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn nonzero_priority_requires_a_capable_hub_before_upload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let uploads = Arc::new(AtomicUsize::new(0));
        let count = uploads.clone();
        let app = axum::Router::new()
            .route(
                "/health",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"ok":true})) }),
            )
            .route(
                "/v1/blobs/missing",
                axum::routing::post(move || async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    axum::Json(MissingBlobs { hashes: vec![] })
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let token = test_token();
        let client = Client::new(&hub, &token).unwrap();
        let snap = snapshot::Snapshot {
            files: vec![],
            excluded: vec![],
            blobs: tempfile::tempdir().unwrap(),
        };
        let results = tempfile::tempdir().unwrap();
        let spec = JobSpec {
            command: vec!["true".into()],
            priority: 1,
            ..Default::default()
        };
        let error = submit(&client, snap, vec![spec], results.path(), true, false)
            .await
            .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("does not support job priority"));
        assert_eq!(uploads.load(Ordering::SeqCst), 0);
    }
}
