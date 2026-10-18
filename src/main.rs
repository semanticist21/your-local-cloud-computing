use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use ylcc::{client::Client, model::*, platform, project::Project, snapshot};

#[derive(Parser)]
#[command(
    version,
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
        .header("User-Agent", format!("ylcc/{}", env!("CARGO_PKG_VERSION")))
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
                "current_version": env!("CARGO_PKG_VERSION"),
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

async fn submit(
    client: &Client,
    snap: snapshot::Snapshot,
    spec: JobSpec,
    results: &Path,
    detach: bool,
    no_download: bool,
) -> Result<()> {
    spec.validate()?;
    if spec.priority != 0 {
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
    let job: Job = client.post("/v1/jobs", &spec).await?;
    if detach {
        print(&job)
    } else {
        eprintln!("job {} ({})", job.id, job.state);
        let j = client.wait(&job.id, results, !no_download).await?;
        print(&j)?;
        if j.state != "succeeded" {
            bail!("job {} ended {}", j.id, j.state)
        }
        Ok(())
    }
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
    let (command, inputs, trust_command) = match runtime {
        "rust" => (r#"    command: [cargo, test, --locked]"#, r#"    inputs: [Cargo.toml, Cargo.lock, src, tests]"#, "Rust"),
        "node" => (r#"    command: [npm, test]"#, r#"    inputs: [package.json, package-lock.json, src, test]"#, "Node"),
        "python" => (r#"    command: [python, -m, pytest, -q]"#, r#"    inputs: [pyproject.toml, src, tests]"#, "Python"),
        "bun" => (r#"    command: [bun, test]"#, r#"    inputs: [package.json, bun.lock, src, tests]"#, "Bun"),
        "go" => (r#"    command: [go, test, ./...]"#, r#"    inputs: [go.mod, go.sum, .]"#, "Go"),
        _ => (r#"    command: [true]"#, r#"    no_input: true"#, "smoke-test"),
    };
    let _ = trust_command;
    Ok(format!(
        r#"version: 1
tasks:
  {name}:
{command}
{inputs}
    requirements:
      os: {os}
      arch: {arch}
      tools: {{}}
    outputs: []
    timeout_secs: 1800
    priority: 0
"#,
        name = name,
        os = os,
        arch = arch
    ))
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
        } => {
            let id = id.unwrap_or_else(default_worker_id);
            ylcc::worker::run(
                hub,
                token,
                data_dir.unwrap_or_else(|| data_path().join("worker")),
                id,
                max_jobs,
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
            let spec = JobSpec {
                command: r.command,
                inputs: snap.files.clone(),
                outputs: r.output,
                requirements: Requirements {
                    os: r.os,
                    arch: r.arch.map(|v| normalize_arch(&v)),
                    abi: r.abi,
                    tools: pairs(r.tool)?,
                    gpu: r.gpu,
                    cpu_features: r.cpu_feature,
                    build_target: r.build_target,
                    emulation: false,
                },
                environment: r.environment,
                container: r.container,
                env: pairs(r.env)?,
                timeout_secs: r.timeout,
                priority: r.priority,
            };
            submit(&client, snap, spec, &results, r.detach, r.no_download).await
        }
        Commands::Task { command } => match command {
            Tasks::List { file } => {
                let (project, _) = Project::load(&file)?;
                print(&project.tasks)
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
                let spec = task.spec(snap.files.clone());
                let client = Client::new(&hub, &token)?;
                submit(&client, snap, spec, &results, detach, no_download).await
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
            let c = Client::new(&hub, &token)?;
            match command {
                Envs::List => print(&c.get::<Vec<Environment>>("/v1/environments").await?),
                Envs::Status => print(
                    &c.get::<Vec<EnvironmentStatus>>("/v1/environments/status")
                        .await?,
                ),
                Envs::Add { file } => {
                    let e: Environment = serde_json::from_slice(&fs::read(file)?)?;
                    print(&c.post::<_, Environment>("/v1/environments", &e).await?)
                }
                Envs::Logs { id } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c.get::<Vec<LogEntry>>(&format!("/v1/environments/{id}/logs"))
                            .await?,
                    )
                }
                Envs::Prepare { id, worker } => {
                    ylcc::client::segment(&id)?;
                    print(
                        &c.post::<_, Vec<EnvironmentStatus>>(
                            &format!("/v1/environments/{id}/prepare"),
                            &PrepareRequest { worker_id: Some(worker) },
                        )
                        .await?,
                    )
                }
                Envs::Ensure { id, worker } => {
                    ylcc::client::segment(&id)?;
                    let statuses = c.get::<Vec<EnvironmentStatus>>("/v1/environments/status").await?;
                    let current = statuses.into_iter().find(|s| s.environment_id == *id);
                    if current.as_ref().is_none_or(|s| s.state != "ready") {
                        ylcc::client::segment(&id)?;
                        let prepared = c.post::<_, Vec<EnvironmentStatus>>(&format!("/v1/environments/{id}/prepare"), &PrepareRequest { worker_id: Some(worker.clone()) }).await?;
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
                map.insert("ylcc_version".into(), serde_json::Value::String(env!("CARGO_PKG_VERSION").into()));
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
            } => {
                let dir = data_dir.unwrap_or_else(|| data_path().join(&role));
                if let Some(user) = system_user {
                    ylcc::service::install_system_user(
                        &role, &hub, &token, &dir, &id, &listen, max_jobs, &user,
                    )
                } else {
                    ylcc::service::install(&role, &hub, &token, &dir, &id, &listen, max_jobs)
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
        let error = submit(&client, snap, spec, results.path(), true, false)
            .await
            .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("does not support job priority"));
        assert_eq!(uploads.load(Ordering::SeqCst), 0);
    }
}
