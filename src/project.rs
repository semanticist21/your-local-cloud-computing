use crate::{model::*, platform, snapshot};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub version: u32,
    /// Cache scope shared by this project's tasks; defaults to the YAML file's directory name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, InlineEnvironment>,
    pub tasks: BTreeMap<String, Task>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub requirements: Option<Requirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cache: Vec<String>,
}
impl Defaults {
    fn is_empty(&self) -> bool {
        self.requirements.is_none()
            && self.environment.is_none()
            && self.container.is_none()
            && self.env.is_empty()
            && self.timeout_secs.is_none()
            && self.priority.is_none()
            && self.cache.is_empty()
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_input: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub requirements: Option<Requirements>,
    /// Run the task once per target; each target overrides the task requirements.
    #[serde(
        default,
        deserialize_with = "strict_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub targets: Vec<Requirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Workspace-relative directories (for example target or node_modules) kept on the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Vec<String>>,
}

/// An environment declared in the project file: a preset (`uses: bun@1.3.14`) or a full recipe.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InlineEnvironment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub requirements: Option<Requirements>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A resolved task: the environment to register first, then one job per target.
pub struct Plan {
    pub environment: Option<Environment>,
    pub specs: Vec<JobSpec>,
}

fn check_requirement_fields(value: &serde_json::Value) -> std::result::Result<(), String> {
    if let Some(fields) = value.as_object() {
        for field in fields.keys() {
            if ![
                "os",
                "arch",
                "abi",
                "tools",
                "gpu",
                "cpu_features",
                "build_target",
                "emulation",
            ]
            .contains(&field.as_str())
            {
                return Err(format!("unknown requirements field {field}"));
            }
        }
    }
    Ok(())
}

fn strict_optional<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Requirements>, D::Error> {
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    check_requirement_fields(&value).map_err(D::Error::custom)?;
    serde_json::from_value(value).map_err(D::Error::custom)
}

fn strict_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Requirements>, D::Error> {
    use serde::de::Error;
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    for value in &values {
        check_requirement_fields(value).map_err(D::Error::custom)?;
    }
    serde_json::from_value(serde_json::Value::Array(values)).map_err(D::Error::custom)
}

/// Built-in native environment presets, used as `uses: NAME@VERSION` (Linux/macOS shells).
pub const PRESETS: &[&str] = &["bun", "node", "rust", "go", "uv"];

const PLATFORM: &str = r#"os=$(uname -s | tr '[:upper:]' '[:lower:]'); arch=$(uname -m); case "$arch" in x86_64|amd64) arch=x64 goarch=amd64;; aarch64|arm64) arch=arm64 goarch=arm64;; *) goarch=$arch;; esac"#;

pub fn preset(uses: &str) -> Result<Environment> {
    let (name, version) = uses
        .split_once('@')
        .context("preset must be NAME@VERSION, for example bun@1.3.14")?;
    if version.is_empty()
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("preset version must contain only letters, digits, ., _ or -")
    }
    let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    };
    let (setup, verify, env) = match name {
        "bun" => (
            format!(
                r#"curl -fsSL https://bun.sh/install -o "$YLCC_ENV_DIR/bun-install.sh" && bash "$YLCC_ENV_DIR/bun-install.sh" "bun-v{version}""#
            ),
            format!(r#"test "$(bun --version)" = "{version}""#),
            env(&[
                ("BUN_INSTALL", "${YLCC_ENV_DIR}"),
                ("BUN_INSTALL_CACHE_DIR", "${YLCC_ENV_DIR}/cache/bun"),
            ]),
        ),
        "node" => (
            format!(
                r#"{PLATFORM}; curl -fsSL "https://nodejs.org/dist/v{version}/node-v{version}-$os-$arch.tar.gz" -o "$YLCC_ENV_DIR/node.tar.gz" && tar -xzf "$YLCC_ENV_DIR/node.tar.gz" -C "$YLCC_ENV_DIR" --strip-components=1"#
            ),
            format!(r#"test "$(node --version)" = "v{version}""#),
            env(&[("npm_config_cache", "${YLCC_ENV_DIR}/cache/npm")]),
        ),
        "rust" => (
            format!(
                r#"curl --fail --location --proto '=https' --tlsv1.2 https://sh.rustup.rs -o "$YLCC_ENV_DIR/rustup-init.sh" && sh "$YLCC_ENV_DIR/rustup-init.sh" -y --no-modify-path --profile minimal --default-toolchain {version}"#
            ),
            format!(r#"rustc --version | grep -F "rustc {version} " && cargo --version"#),
            env(&[
                ("CARGO_HOME", "${YLCC_ENV_DIR}"),
                ("RUSTUP_HOME", "${YLCC_ENV_DIR}/rustup"),
            ]),
        ),
        "go" => (
            format!(
                r#"{PLATFORM}; curl -fsSL "https://go.dev/dl/go{version}.$os-$goarch.tar.gz" -o "$YLCC_ENV_DIR/go.tar.gz" && tar -xzf "$YLCC_ENV_DIR/go.tar.gz" -C "$YLCC_ENV_DIR" --strip-components=1"#
            ),
            format!(r#"go version | grep -F "go{version} ""#),
            env(&[
                ("GOPATH", "${YLCC_ENV_DIR}/gopath"),
                ("GOCACHE", "${YLCC_ENV_DIR}/cache/go-build"),
                ("GOTOOLCHAIN", "local"),
            ]),
        ),
        "uv" => (
            format!(
                r#"curl -LsSf "https://astral.sh/uv/{version}/install.sh" -o "$YLCC_ENV_DIR/uv-install.sh" && UV_INSTALL_DIR="$YLCC_ENV_DIR/bin" UV_NO_MODIFY_PATH=1 sh "$YLCC_ENV_DIR/uv-install.sh""#
            ),
            format!(r#"uv --version | grep -F "uv {version}""#),
            env(&[
                ("UV_CACHE_DIR", "${YLCC_ENV_DIR}/cache/uv"),
                ("UV_PYTHON_INSTALL_DIR", "${YLCC_ENV_DIR}/python"),
            ]),
        ),
        _ => bail!("unknown preset {name}; available: {}", PRESETS.join(", ")),
    };
    Ok(Environment {
        id: format!("{name}-{version}"),
        setup: vec![setup],
        verify: vec![verify],
        env,
        ..Default::default()
    })
}

/// Resolves `NAME@VERSION` to a registrable preset environment; other names are Hub catalogue ids.
pub fn preset_environment(name: &str) -> Result<Option<Environment>> {
    if !name.contains('@') {
        return Ok(None);
    }
    let mut environment = preset(name)?;
    environment.revision = content_revision(&environment)?;
    Ok(Some(environment))
}

/// Content-addressed revision, so editing a recipe prepares a new revision instead of conflicting.
fn content_revision(environment: &Environment) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut unrevised = environment.clone();
    unrevised.revision.clear();
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&unrevised)?))[..16].into())
}

fn merge(base: &Requirements, target: &Requirements) -> Requirements {
    let mut merged = base.clone();
    for (field, value) in [
        (&mut merged.os, &target.os),
        (&mut merged.arch, &target.arch),
        (&mut merged.abi, &target.abi),
        (&mut merged.gpu, &target.gpu),
        (&mut merged.build_target, &target.build_target),
    ] {
        if value.is_some() {
            field.clone_from(value);
        }
    }
    merged.tools.extend(target.tools.clone());
    for feature in &target.cpu_features {
        if !merged.cpu_features.contains(feature) {
            merged.cpu_features.push(feature.clone());
        }
    }
    merged.emulation |= target.emulation;
    merged.arch = merged.arch.as_deref().map(normalize_arch);
    merged
}

impl Project {
    pub fn load(file: &Path) -> Result<(Self, PathBuf)> {
        let file = platform::command_path(
            &file
                .canonicalize()
                .with_context(|| format!("project file {}", file.display()))?,
        );
        let project = Self::parse(&fs::read_to_string(&file)?)?;
        let root = file
            .parent()
            .context("project file has no parent")?
            .to_path_buf();
        Ok((project, root))
    }

    pub fn parse(yaml: &str) -> Result<Self> {
        let project: Self = yaml_serde::from_str(yaml).context("invalid YLCC project YAML")?;
        if project.version != 1 {
            bail!(
                "unsupported YLCC project version {}; expected 1",
                project.version
            )
        }
        for name in project.environments.keys() {
            project
                .environment(name)
                .with_context(|| format!("environment {name}"))?;
        }
        for (name, task) in &project.tasks {
            if name.trim().is_empty() {
                bail!("task name must not be empty")
            }
            task.validate()
                .and_then(|()| {
                    project
                        .plan(name, Path::new("project"), Vec::new())
                        .map(drop)
                })
                .with_context(|| format!("task {name}"))?;
        }
        Ok(project)
    }

    /// Resolves an inline environment to the catalogue entry the Hub stores.
    pub fn environment(&self, name: &str) -> Result<Environment> {
        let inline = self
            .environments
            .get(name)
            .with_context(|| format!("unknown environment {name}"))?;
        let mut environment = if let Some(uses) = &inline.uses {
            if inline.requirements.is_some()
                || !inline.setup.is_empty()
                || !inline.verify.is_empty()
                || inline.image.is_some()
                || !inline.env.is_empty()
            {
                bail!("uses cannot be combined with a custom recipe")
            }
            preset(uses)?
        } else {
            if inline.setup.is_empty() && inline.image.is_none() {
                bail!("declare uses, setup or image")
            }
            Environment {
                id: name.into(),
                requirements: inline.requirements.clone().unwrap_or_default(),
                setup: inline.setup.clone(),
                verify: inline.verify.clone(),
                image: inline.image.clone(),
                env: inline.env.clone(),
                ..Default::default()
            }
        };
        environment.revision = match &inline.revision {
            Some(revision) => revision.clone(),
            None => content_revision(&environment)?,
        };
        Ok(environment)
    }

    /// Applies defaults and targets; `root` names the default cache scope.
    pub fn plan(&self, name: &str, root: &Path, inputs: Vec<FileEntry>) -> Result<Plan> {
        let task = self
            .tasks
            .get(name)
            .with_context(|| format!("unknown task {name}"))?;
        let defaults = &self.defaults;
        let environment_name = task.environment.as_ref().or(defaults.environment.as_ref());
        let environment = match environment_name {
            Some(name) if self.environments.contains_key(name) => Some(self.environment(name)?),
            Some(name) => preset_environment(name)?,
            None => None,
        };
        let environment_id = environment
            .as_ref()
            .map(|e| e.id.clone())
            .or(environment_name.cloned());
        let cache = task.cache.clone().unwrap_or_else(|| defaults.cache.clone());
        let scope = self.name.clone().or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let base = task
            .requirements
            .clone()
            .or_else(|| defaults.requirements.clone())
            .unwrap_or_default();
        let targets = if task.targets.is_empty() {
            vec![Requirements::default()]
        } else {
            task.targets.clone()
        };
        let mut env = defaults.env.clone();
        env.extend(task.env.clone());
        let specs = targets
            .iter()
            .map(|target| {
                let spec = JobSpec {
                    command: task.command.clone(),
                    inputs: inputs.clone(),
                    outputs: task.outputs.clone(),
                    requirements: merge(&base, target),
                    environment: environment_id.clone(),
                    container: task
                        .container
                        .clone()
                        .or_else(|| defaults.container.clone()),
                    env: env.clone(),
                    timeout_secs: task.timeout_secs.or(defaults.timeout_secs),
                    priority: task.priority.or(defaults.priority).unwrap_or(0),
                    cache: cache.clone(),
                    cache_scope: (!cache.is_empty()).then(|| scope.clone()).flatten(),
                };
                spec.validate()?;
                Ok(spec)
            })
            .collect::<Result<_>>()?;
        Ok(Plan { environment, specs })
    }

    /// Adds a task to a project file, creating it when missing. Existing text and comments are kept.
    pub fn save_task(file: &Path, name: &str, task: Task) -> Result<()> {
        let block = yaml_serde::to_string(&BTreeMap::from([(name, &task)]))?;
        let text = if file.exists() {
            let text = fs::read_to_string(file)?;
            if Self::parse(&text)?.tasks.contains_key(name) {
                bail!("task {name} already exists in {}", file.display())
            }
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|line| line.split('#').next().unwrap_or_default().trim_end() == "tasks:")
                .context(
                    "project file has no block-style `tasks:` mapping; add the task manually",
                )?;
            let indent = lines[at + 1..]
                .iter()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .map(|line| line.len() - line.trim_start().len())
                .filter(|indent| *indent > 0)
                .unwrap_or(2);
            let mut out: Vec<String> = lines[..=at].iter().map(|l| l.to_string()).collect();
            out.extend(
                block
                    .lines()
                    .map(|line| format!("{}{line}", " ".repeat(indent))),
            );
            out.extend(lines[at + 1..].iter().map(|l| l.to_string()));
            out.join("\n") + "\n"
        } else {
            let indented: String = block.lines().map(|line| format!("  {line}\n")).collect();
            format!("version: 1\ntasks:\n{indented}")
        };
        Self::parse(&text).context("saved task would make the project file invalid")?;
        if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(file, text)?;
        Ok(())
    }
}

impl Task {
    fn validate(&self) -> Result<()> {
        if self.no_input && (self.project || !self.inputs.is_empty()) {
            bail!("no_input cannot be combined with inputs or project")
        }
        if !self.no_input && !self.project && self.inputs.is_empty() {
            bail!("select inputs, project: true, or no_input: true")
        }
        for input in &self.inputs {
            let input = input.to_str().context("input path is not UTF-8")?;
            if input != "." {
                platform::relative_path(Path::new(input))?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self, root: &Path) -> Result<snapshot::Snapshot> {
        self.validate()?;
        if self.no_input {
            return Ok(snapshot::Snapshot {
                files: Vec::new(),
                excluded: Vec::new(),
                blobs: tempfile::tempdir()?,
            });
        }
        snapshot::create(root, &self.inputs, self.project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_task(fields: &str) -> Result<Project> {
        Project::parse(&format!(
            "version: 1\ntasks:\n  build:\n    command: [cargo, build, --locked]\n{fields}"
        ))
    }

    #[test]
    fn parses_named_tasks_without_changing_their_requirements() {
        let project = parse_task("    inputs: [Cargo.toml, src]\n    outputs: [target/app]\n    requirements: {os: linux, arch: x86_64, tools: {cargo: '*'}}\n    env: {RUST_BACKTRACE: '1'}\n    priority: 3\n    timeout_secs: 1800\n").unwrap();
        let spec = project
            .plan("build", Path::new("app"), Vec::new())
            .unwrap()
            .specs
            .remove(0);
        assert_eq!(spec.command, ["cargo", "build", "--locked"]);
        assert_eq!(spec.requirements.os.as_deref(), Some("linux"));
        assert_eq!(spec.requirements.tools["cargo"], "*");
        assert_eq!(spec.env["RUST_BACKTRACE"], "1");
        assert_eq!(spec.priority, 3);
        assert_eq!(spec.timeout_secs, Some(1800));
    }

    #[test]
    fn rejects_unknown_fields_versions_and_unsafe_or_conflicting_selections() {
        for fields in [
            "    no_input: true\n    cpu: '50%'\n",
            "    no_input: true\n    requirements: {memory: '50%'}\n",
            "    no_input: true\n    inputs: [src]\n",
            "    no_input: true\n    project: true\n",
            "    inputs: [../secret]\n",
            "    inputs: [/etc/passwd]\n",
            "    no_input: true\n    priority: 101\n",
            "    no_input: true\n    priority: -101\n",
            "    no_input: true\n    outputs: [../escape]\n",
            "",
        ] {
            assert!(parse_task(fields).is_err(), "accepted {fields:?}");
        }
        assert!(Project::parse("version: 2\ntasks: {}\n").is_err());
        assert!(Project::parse("version: 1\nhub: https://example.test\ntasks: {}\n").is_err());
        assert!(
            Project::parse("version: 1\ntasks: {bad: {command: [], no_input: true}}\n").is_err()
        );
        assert!(
            Project::parse("version: 1\ntasks: {bad: {command: [''], no_input: true}}\n").is_err()
        );
    }

    #[test]
    fn snapshots_relative_to_the_project_file_parent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("app");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("source.txt"), "project input").unwrap();
        fs::write(
            root.join("ylcc.yaml"),
            "version: 1\ntasks: {check: {command: [cat, source.txt], inputs: [source.txt]}}\n",
        )
        .unwrap();
        let (project, resolved_root) = Project::load(&root.join("ylcc.yaml")).unwrap();
        assert_eq!(
            resolved_root,
            platform::command_path(&root.canonicalize().unwrap())
        );
        let snapshot = project.tasks["check"].snapshot(&resolved_root).unwrap();
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "source.txt");
        assert_eq!(
            fs::read_to_string(snapshot.blobs.path().join(&snapshot.files[0].hash)).unwrap(),
            "project input"
        );
    }

    #[test]
    fn empty_input_is_only_allowed_when_explicit() {
        let project = parse_task("    no_input: true\n").unwrap();
        let snap = project.tasks["build"]
            .snapshot(Path::new("unused"))
            .unwrap();
        assert!(snap.files.is_empty());
        assert!(parse_task("    project: true\n    inputs: [ignored-file]\n").is_ok());
    }

    #[test]
    fn defaults_targets_and_cache_resolve_to_one_spec_per_target() {
        let project = Project::parse(
            "version: 1
defaults:
  requirements: {os: linux, arch: amd64, tools: {cargo: '*'}}
  timeout_secs: 600
  cache: [target]
  env: {A: '1'}
tasks:
  test:
    command: [cargo, test]
    inputs: [src]
    env: {B: '2'}
    targets:
      - {}
      - {os: macos, arch: arm64, tools: {xcodebuild: '*'}}
  build:
    command: [cargo, build]
    inputs: [src]
    timeout_secs: 30
    cache: []
",
        )
        .unwrap();
        let specs = project
            .plan("test", Path::new("/x/app"), Vec::new())
            .unwrap()
            .specs;
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].requirements.os.as_deref(), Some("linux"));
        assert_eq!(specs[0].requirements.arch.as_deref(), Some("x86_64"));
        assert_eq!(specs[1].requirements.os.as_deref(), Some("macos"));
        assert_eq!(specs[1].requirements.arch.as_deref(), Some("aarch64"));
        assert_eq!(specs[1].requirements.tools.len(), 2);
        assert_eq!(specs[0].timeout_secs, Some(600));
        assert_eq!(specs[0].cache, ["target"]);
        assert_eq!(specs[0].cache_scope.as_deref(), Some("app"));
        assert_eq!(
            (specs[0].env["A"].as_str(), specs[0].env["B"].as_str()),
            ("1", "2")
        );
        let build = project
            .plan("build", Path::new("/x/app"), Vec::new())
            .unwrap()
            .specs;
        assert_eq!(build[0].timeout_secs, Some(30));
        assert!(build[0].cache.is_empty() && build[0].cache_scope.is_none());
        assert!(parse_task("    no_input: true\n    cache: [../up]\n").is_err());
        assert!(parse_task("    no_input: true\n    targets: [{memory: 1}]\n").is_err());
    }

    #[test]
    fn inline_environments_resolve_presets_and_content_revisions() {
        let yaml = |setup: &str| {
            format!(
                "version: 1
environments:
  bun: {{uses: bun@1.3.14}}
  tools: {{setup: ['{setup}'], verify: [true]}}
tasks:
  a: {{command: [bun, test], no_input: true, environment: bun}}
  b: {{command: [tool], no_input: true, environment: tools}}
  c: {{command: [x], no_input: true, environment: registered-elsewhere}}
"
            )
        };
        let project = Project::parse(&yaml("echo one")).unwrap();
        let plan = project.plan("a", Path::new("p"), Vec::new()).unwrap();
        let bun = plan.environment.unwrap();
        assert_eq!(bun.id, "bun-1.3.14");
        assert_eq!(plan.specs[0].environment.as_deref(), Some("bun-1.3.14"));
        assert!(bun.setup[0].contains("bun-v1.3.14") && bun.revision.len() == 16);
        let tools = project.environment("tools").unwrap();
        let changed = Project::parse(&yaml("echo two"))
            .unwrap()
            .environment("tools")
            .unwrap();
        assert_eq!(tools.id, "tools");
        assert_ne!(tools.revision, changed.revision);
        assert_eq!(
            tools.revision,
            project.environment("tools").unwrap().revision
        );
        let direct = Project::parse(
            "version: 1\ntasks: {a: {command: [go], no_input: true, environment: go@1.23.4}}\n",
        )
        .unwrap();
        let direct = direct.plan("a", Path::new("p"), Vec::new()).unwrap();
        assert_eq!(direct.environment.unwrap().id, "go-1.23.4");
        assert_eq!(direct.specs[0].environment.as_deref(), Some("go-1.23.4"));
        let other = project.plan("c", Path::new("p"), Vec::new()).unwrap();
        assert!(other.environment.is_none());
        assert_eq!(
            other.specs[0].environment.as_deref(),
            Some("registered-elsewhere")
        );
        for bad in ["bun@1;rm -rf /", "bun", "nope@1", "bun@"] {
            assert!(preset(bad).is_err(), "accepted {bad}");
        }
        for name in PRESETS {
            assert!(preset(&format!("{name}@1.0.0")).is_ok());
        }
        assert!(
            Project::parse("version: 1\nenvironments: {x: {uses: bun@1, setup: [a]}}\ntasks: {}\n")
                .is_err()
        );
        assert!(
            Project::parse("version: 1\nenvironments: {x: {verify: [a]}}\ntasks: {}\n").is_err()
        );
    }

    #[test]
    fn save_task_inserts_under_tasks_and_keeps_comments() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("ylcc.yaml");
        let task = |command: &str| Task {
            command: vec![command.into()],
            no_input: true,
            requirements: Some(Requirements {
                os: Some("linux".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        Project::save_task(&file, "first", task("true")).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        fs::write(
            &file,
            format!("# keep me\n{text}defaults: {{timeout_secs: 5}}\n"),
        )
        .unwrap();
        Project::save_task(&file, "second", task("false")).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# keep me\n"));
        let project = Project::parse(&text).unwrap();
        assert_eq!(project.tasks.len(), 2);
        assert_eq!(project.defaults.timeout_secs, Some(5));
        assert!(!text.contains("null"));
        assert!(Project::save_task(&file, "second", task("x")).is_err());
    }
}
