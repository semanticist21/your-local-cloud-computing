use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn normalize_arch(s: &str) -> String {
    match s {
        "amd64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        _ => s,
    }
    .into()
}
pub fn safe_relative(s: &str) -> Result<()> {
    if s.split('/')
        .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains(['\\', '\0']))
    {
        bail!("unsafe relative path: {s}")
    }
    Ok(())
}
pub fn windows_relative(s: &str) -> Result<()> {
    safe_relative(s)?;
    if s.split('/').any(|part| {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        part.ends_with(['.', ' '])
            || part.chars().any(|c| c < ' ' || "\\<>:\"|?*".contains(c))
            || ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
    }) {
        bail!("unsafe Windows relative path: {s}")
    }
    Ok(())
}
pub fn validate_env_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.contains(['=', '\0'])
        || key
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("YLCC_"))
    {
        bail!("invalid or reserved environment variable")
    }
    Ok(())
}
pub fn validate_hash(s: &str) -> Result<()> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("invalid SHA-256")
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpu_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_target: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub emulation: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub os: String,
    pub arch: String,
    pub abi: String,
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub gpu: Vec<String>,
    #[serde(default)]
    pub cpu_features: Vec<String>,
    #[serde(default)]
    pub runtimes: Vec<String>,
    pub cpus: usize,
    pub memory_bytes: u64,
    #[serde(default)]
    pub containerized: bool,
    /// The worker's command policy; absent means it runs everything it is assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::policy::Policy>,
}
impl Capabilities {
    pub fn mismatch(&self, r: &Requirements) -> Vec<String> {
        let mut reasons = Vec::new();
        if r.os.as_ref().is_some_and(|v| v != &self.os) {
            reasons.push(format!("OS is {}", self.os));
        }
        if r.arch
            .as_ref()
            .is_some_and(|v| normalize_arch(v) != normalize_arch(&self.arch))
        {
            reasons.push(format!(
                "architecture is {} (emulation is never implicit)",
                self.arch
            ));
        }
        if r.abi.as_ref().is_some_and(|v| v != &self.abi) {
            reasons.push(format!("ABI is {}", self.abi));
        }
        for (tool, version) in &r.tools {
            if !self
                .tools
                .get(tool)
                .is_some_and(|v| version == "*" || v == version)
            {
                reasons.push(format!("missing tool {tool} version {version}"));
            }
        }
        if r.gpu.as_ref().is_some_and(|v| !self.gpu.contains(v)) {
            reasons.push("GPU backend unavailable".into());
        }
        for f in &r.cpu_features {
            if !self.cpu_features.contains(f) {
                reasons.push(format!("missing CPU feature {f}"));
            }
        }
        reasons
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub mode: u32,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobSpec {
    pub command: Vec<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub inputs: Vec<FileEntry>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub requirements: Requirements,
    pub environment: Option<String>,
    pub container: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_secs: Option<u64>,
    /// Workspace-relative directories the worker keeps between attempts of the same scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
}
impl JobSpec {
    pub fn validate(&self) -> Result<()> {
        validate_priority(self.priority)?;
        if self.command.is_empty() || self.command[0].is_empty() {
            bail!("command is required")
        }
        let mut paths = std::collections::BTreeSet::new();
        for f in &self.inputs {
            safe_relative(&f.path)?;
            if self.requirements.os.as_deref() == Some("windows") {
                windows_relative(&f.path)?;
            }
            validate_hash(&f.hash)?;
            if !paths.insert(&f.path) {
                bail!("duplicate input path")
            }
        }
        for p in &self.outputs {
            safe_relative(p)?;
            if self.requirements.os.as_deref() == Some("windows") {
                windows_relative(p)?;
            }
        }
        for p in &self.cache {
            safe_relative(p)?;
            if self.requirements.os.as_deref() == Some("windows") {
                windows_relative(p)?;
            }
        }
        if !self.cache.is_empty() && self.cache_scope.as_deref().is_none_or(str::is_empty) {
            bail!("cache requires a cache scope")
        }
        for key in self.env.keys() {
            validate_env_key(key)?;
        }
        if self.requirements.emulation {
            bail!("emulation is not implemented; use a native matching worker")
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub attempt_id: String,
    pub state: String,
    pub spec: JobSpec,
    pub worker_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(default)]
    pub results: Vec<FileEntry>,
    /// Name of the token that submitted the job, recorded by the Hub; absent for older jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_by: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub id: String,
    pub capabilities: Capabilities,
    pub max_jobs: usize,
    #[serde(default)]
    pub paused: bool,
    pub online: bool,
    pub last_seen: u64,
    /// The worker binary's `ylcc --version`, empty for workers older than 0.4.6.
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_error: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControl {
    pub paused: Option<bool>,
    pub max_jobs: Option<usize>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorityRequest {
    pub priority: i32,
}
pub fn validate_priority(priority: i32) -> Result<()> {
    if !(-100..=100).contains(&priority) {
        bail!("priority must be between -100 and 100")
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub job_id: String,
    pub attempt_id: String,
    pub seq: u64,
    pub stream: String,
    pub text: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    pub revision: String,
    #[serde(default)]
    pub requirements: Requirements,
    #[serde(default)]
    pub setup: Vec<String>,
    #[serde(default)]
    pub verify: Vec<String>,
    pub image: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    pub worker_id: String,
    pub environment_id: String,
    pub revision: String,
    pub state: String,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptReport {
    pub job_id: String,
    pub attempt_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(default)]
    pub results: Vec<FileEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Hello {
        worker_id: String,
        capabilities: Capabilities,
        max_jobs: usize,
        attempts: Vec<AttemptReport>,
        environments: Vec<EnvironmentStatus>,
        #[serde(default)]
        version: String,
    },
    Started {
        job_id: String,
        attempt_id: String,
    },
    Log {
        entry: LogEntry,
    },
    Finished {
        report: AttemptReport,
    },
    Environment {
        status: EnvironmentStatus,
    },
    Heartbeat,
    /// A requested self-update did not replace the binary; the worker keeps running.
    UpdateFailed {
        error: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    Run {
        job: Box<Job>,
        environment: Option<Environment>,
    },
    Cancel {
        job_id: String,
        attempt_id: String,
    },
    Prepare {
        environment: Environment,
    },
    /// The environment was deleted from the catalogue; remove every local revision.
    Evict {
        environment_id: String,
    },
    /// Replace the worker binary with this published release tag, then restart.
    Update {
        version: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingBlobs {
    pub hashes: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub worker_id: Option<String>,
}
/// What a Hub token may do: admins manage the pool, requesters run work, workers serve one id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Requester,
    Worker,
}
impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Admin => "admin",
            Role::Requester => "requester",
            Role::Worker => "worker",
        })
    }
}
/// A scoped Hub token as the API shows it; the secret and its hash never leave the Hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRequest {
    pub name: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedToken {
    pub token: TokenInfo,
    pub secret: String,
}
/// The identity a request authenticated as; `token_id` is absent for the pool token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub name: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub seq: i64,
    pub at: u64,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_token_id: Option<String>,
    pub role: Role,
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_paths() {
        for s in [
            "../x",
            "/tmp/x",
            "a/../b",
            "a\\b",
            "",
            "//host/share",
            "a//b",
            "a/./b",
            "a\0b",
        ] {
            assert!(safe_relative(s).is_err());
            assert!(windows_relative(s).is_err());
        }
        assert!(safe_relative("dir/a.txt").is_ok());
    }
    #[test]
    fn windows_component_rules_do_not_restrict_unix_jobs() {
        for s in [
            "C:/temp/a",
            "a:stream",
            "a.",
            "a ",
            "NUL",
            "con.txt",
            "COM1.log",
            "lpt9",
            "COM¹.txt",
            "CONIN$",
            "con .txt",
            "a/aux/file",
            "a\nfile",
            "a?b",
            "a|b",
            "aux.txt",
            "con/part.rs",
            "logs/2026-10-16T11:22:33.log",
        ] {
            assert!(safe_relative(s).is_ok(), "{s}");
            assert!(windows_relative(s).is_err(), "{s}");
            let mut spec = JobSpec {
                command: vec!["test".into()],
                inputs: vec![FileEntry {
                    path: s.into(),
                    hash: "a".repeat(64),
                    size: 0,
                    mode: 0o644,
                }],
                outputs: vec![s.into()],
                ..Default::default()
            };
            for os in [None, Some("linux"), Some("macos")] {
                spec.requirements.os = os.map(str::to_owned);
                assert!(spec.validate().is_ok(), "{s}");
            }
            spec.requirements.os = Some("windows".into());
            assert!(spec.validate().is_err(), "{s}");
        }
        assert!(windows_relative("dir/COM10.txt").is_ok());
        assert!(windows_relative("dir/hello world.txt").is_ok());
    }
    #[test]
    fn reserved_environment_keys_are_case_insensitive_and_nonempty() {
        let mut spec = JobSpec {
            command: vec!["test".into()],
            ..Default::default()
        };
        for key in [
            "",
            "YLCC_JOB_ID",
            "ylcc_job_id",
            "YlCc_attempt_id",
            "YLCC_",
            "bad=name",
            "bad\0name",
        ] {
            spec.env = BTreeMap::from([(key.into(), "value".into())]);
            assert!(spec.validate().is_err(), "{key:?}");
        }
        for key in ["PATH", "YLCC", "MY_YLCC_VALUE", "ordinary"] {
            spec.env = BTreeMap::from([(key.into(), "value".into())]);
            assert!(spec.validate().is_ok(), "{key}");
        }
    }
    #[test]
    fn architecture_and_gpu_are_required() {
        let c = Capabilities {
            os: "linux".into(),
            arch: "x86_64".into(),
            ..Default::default()
        };
        assert!(
            c.mismatch(&Requirements {
                arch: Some("amd64".into()),
                ..Default::default()
            })
            .is_empty()
        );
        assert!(
            !c.mismatch(&Requirements {
                gpu: Some("cuda".into()),
                ..Default::default()
            })
            .is_empty()
        );
    }
    #[test]
    fn priorities_are_bounded_and_old_records_default_to_normal() {
        let mut spec: JobSpec = serde_json::from_value(serde_json::json!({
            "command":["true"],
            "environment":null,
            "container":null,
            "timeout_secs":null
        }))
        .unwrap();
        assert_eq!(spec.priority, 0);
        for priority in [-100, 0, 100] {
            spec.priority = priority;
            assert!(spec.validate().is_ok());
        }
        for priority in [-101, 101] {
            spec.priority = priority;
            assert!(spec.validate().is_err());
        }
        let worker: WorkerInfo = serde_json::from_value(serde_json::json!({
            "id":"old-worker",
            "capabilities":Capabilities::default(),
            "max_jobs":1,
            "online":false,
            "last_seen":0
        }))
        .unwrap();
        assert!(!worker.paused);
    }
}
