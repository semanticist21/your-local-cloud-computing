use anyhow::{Context, Result, bail};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn checked(value: &str) -> Result<&str> {
    if value.contains(['\0', '\n', '\r']) {
        bail!("service arguments cannot contain control characters")
    }
    Ok(value)
}

#[cfg(unix)]
fn systemd_arg(value: &str) -> Result<String> {
    checked(value)?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}

fn xml(value: &str) -> Result<String> {
    checked(value)?;
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn role_name(role: &str) -> Result<&str> {
    match role {
        "hub" | "worker" => Ok(role),
        _ => bail!("service role must be hub or worker"),
    }
}

#[cfg(unix)]
fn run(command: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("run {command}"))?;
    if !output.status.success() {
        bail!(
            "{command} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

#[cfg(unix)]
fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        env::var_os("HOME").context("HOME is required for user services")?,
    ))
}

#[cfg(unix)]
fn systemd_unit(args: &[String], data_dir: &Path, path: &str, role: &str) -> Result<String> {
    let args = args
        .iter()
        .map(|s| systemd_arg(s))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    // Environment= does not expand dollar signs, but unit specifiers still apply.
    let path = checked(path)?
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let directory = data_dir
        .to_str()
        .context("service data directory must be UTF-8")?;
    checked(directory)?;
    if !data_dir.is_absolute() || directory.trim_end() != directory {
        bail!("service data directory must be absolute with no trailing whitespace")
    }
    // WorkingDirectory takes the full value, not ExecStart-style quoted arguments.
    let directory = directory.replace('%', "%%");
    Ok(format!(
        "[Unit]\nDescription=YLCC {role}\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={args}\nWorkingDirectory={directory}\nEnvironment=\"PATH={path}\"\nUMask=0077\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(unix)]
fn service_path(path: &Path) -> Result<&str> {
    let value = path.to_str().context("service paths must be UTF-8")?;
    checked(value)?;
    if !path.is_absolute()
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("service paths must be absolute without parent traversal")
    }
    Ok(value)
}

#[cfg(unix)]
fn prepare_service_data(directory: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    if uid == unsafe { libc::geteuid() } {
        return crate::worker::private_dir(directory);
    }
    // Configured data-directory symlinks are supported; validate the opened target before chmod.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(directory)
        .with_context(|| format!("open service data directory {}", directory.display()))?;
    let metadata = file.metadata()?;
    if metadata.uid() != uid {
        bail!(
            "service data directory must belong to its service user: {}",
            directory.display()
        )
    }
    if metadata.mode() & 0o7777 != 0o700 {
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "set service data directory permissions {}",
                    directory.display()
                )
            })?;
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_log_directory(directory: &Path, directory_uid: u32, directory_mode: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    service_path(directory)?;
    for parent in directory.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = match fs::symlink_metadata(parent) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(directory_mode)
                    .create(parent)
                    .with_context(|| format!("create log directory {}", parent.display()))?;
                fs::set_permissions(parent, fs::Permissions::from_mode(directory_mode))?;
                fs::symlink_metadata(parent)?
            }
            Err(error) => return Err(error.into()),
        };
        // Sticky root-owned parents protect entries owned by root or the current user.
        let sticky_root = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != directory_uid)
            || (metadata.mode() & 0o022 != 0 && !sticky_root)
        {
            bail!("unsafe log directory or parent: {}", parent.display())
        }
        if parent == directory
            && (metadata.uid() != directory_uid || metadata.mode() & 0o7777 != directory_mode)
        {
            bail!(
                "log directory must have its expected owner and mode: {}",
                parent.display()
            )
        }
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_launchd_logs(
    directory: &Path,
    role: &str,
    directory_uid: u32,
    file_uid: u32,
    file_gid: u32,
    directory_mode: u32,
) -> Result<()> {
    use std::os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    role_name(role)?;
    prepare_log_directory(directory, directory_uid, directory_mode)?;
    for name in [
        format!("service-{role}.log"),
        format!("service-{role}.err.log"),
    ] {
        let path = directory.join(name);
        let mut options = fs::OpenOptions::new();
        options
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
        let (file, created) = match options.create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                (options.create_new(false).open(&path)?, false)
            }
            Err(error) => {
                return Err(error).with_context(|| format!("open service log {}", path.display()));
            }
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            bail!(
                "service log must be a regular file with one link: {}",
                path.display()
            )
        }
        if created {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            if (metadata.uid(), metadata.gid()) != (file_uid, file_gid)
                && unsafe { libc::fchown(file.as_raw_fd(), file_uid, file_gid) } != 0
            {
                return Err(std::io::Error::last_os_error()).context("set service log owner");
            }
        }
        let metadata = file.metadata()?;
        if (metadata.uid(), metadata.gid()) != (file_uid, file_gid)
            || metadata.mode() & 0o7777 != 0o600
        {
            bail!(
                "service log has an unexpected owner or permissions: {}",
                path.display()
            )
        }
    }
    Ok(())
}

#[cfg(unix)]
fn launch_agent(
    args: &[String],
    data_dir: &Path,
    log_dir: &Path,
    path: &str,
    role: &str,
) -> Result<String> {
    role_name(role)?;
    let directory = service_path(data_dir)?;
    service_path(log_dir)?;
    let args = args
        .iter()
        .map(|s| Ok(format!("<string>{}</string>", xml(s)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let log = xml(&log_dir
        .join(format!("service-{role}.log"))
        .to_string_lossy())?;
    let err = xml(&log_dir
        .join(format!("service-{role}.err.log"))
        .to_string_lossy())?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>org.ylcc.{role}</string>\n<key>ProgramArguments</key><array>\n{args}\n</array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>ThrottleInterval</key><integer>3</integer>\n<key>Umask</key><integer>63</integer>\n<key>StandardOutPath</key><string>{log}</string>\n<key>StandardErrorPath</key><string>{err}</string>\n</dict></plist>\n",
        xml(directory)?,
        xml(path)?
    ))
}

fn service_arguments(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
) -> Result<(Vec<String>, PathBuf)> {
    role_name(role)?;
    if max_jobs == 0 {
        bail!("max-jobs must be at least 1")
    }
    let executable = crate::platform::command_path(&env::current_exe()?.canonicalize()?);
    let token_file = crate::platform::command_path(
        &token_file
            .canonicalize()
            .context("token file must already exist")?,
    );
    if !token_file.is_file() {
        bail!("token-file must be a regular file")
    }
    fs::File::open(&token_file).context("token file must be readable by the service user")?;
    fs::create_dir_all(data_dir)?;
    let data_dir = crate::platform::command_path(&data_dir.canonicalize()?);
    let mut args = vec![
        executable.to_string_lossy().into_owned(),
        "--hub".into(),
        hub.into(),
        "--token-file".into(),
        token_file.to_string_lossy().into_owned(),
        role.into(),
        "--data-dir".into(),
        data_dir.to_string_lossy().into_owned(),
    ];
    if role == "hub" {
        args.extend(["--listen".into(), listen.into()]);
    } else {
        args.extend([
            "--id".into(),
            id.into(),
            "--max-jobs".into(),
            max_jobs.to_string(),
        ]);
    }
    for arg in &args {
        checked(arg)?;
    }
    Ok((args, data_dir))
}

/// Install a service for the current user; no root identity or system service is used.
#[cfg(unix)]
pub fn install(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
) -> Result<()> {
    let (args, data_dir) =
        service_arguments(role, hub, token_file, data_dir, id, listen, max_jobs)?;
    let path = env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    match env::consts::OS {
        "linux" => {
            let directory = home()?.join(".config/systemd/user");
            fs::create_dir_all(&directory)?;
            let name = format!("ylcc-{role}.service");
            fs::write(
                directory.join(&name),
                systemd_unit(&args, &data_dir, &path, role)?,
            )?;
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", &name])?;
            run("systemctl", &["--user", "restart", &name])?;
            println!(
                "Installed {name}. Boot without login requires the administrator to enable linger for this user."
            );
        }
        "macos" => {
            prepare_service_data(&data_dir, unsafe { libc::geteuid() })?;
            let home = home()?;
            let directory = home.join("Library/LaunchAgents");
            fs::create_dir_all(&directory)?;
            let label = format!("org.ylcc.{role}");
            let domain = format!("gui/{}", unsafe { libc::getuid() });
            let target = format!("{domain}/{label}");
            let file = directory.join(format!("{label}.plist"));
            let log_dir = home.join("Library/Logs/ylcc");
            let plist = launch_agent(&args, &data_dir, &log_dir, &path, role)?;
            prepare_launchd_logs(
                &log_dir,
                role,
                unsafe { libc::getuid() },
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
                0o700,
            )?;
            if Command::new("launchctl")
                .args(["print", &target])
                .output()?
                .status
                .success()
            {
                run("launchctl", &["bootout", &target])?;
            }
            fs::write(&file, plist)?;
            run("launchctl", &["enable", &target])?;
            run(
                "launchctl",
                &["bootstrap", &domain, &file.to_string_lossy()],
            )?;
            println!(
                "Installed {label}. This LaunchAgent runs in the current user's GUI login session; the external data volume must be mounted."
            );
        }
        os => bail!("native user services are unsupported on {os}"),
    }
    Ok(())
}

#[cfg(unix)]
pub fn uninstall(role: &str) -> Result<()> {
    role_name(role)?;
    match env::consts::OS {
        "linux" => {
            let name = format!("ylcc-{role}.service");
            let file = home()?.join(".config/systemd/user").join(&name);
            if file.exists() {
                run("systemctl", &["--user", "disable", "--now", &name])?;
                fs::remove_file(file)?;
                run("systemctl", &["--user", "daemon-reload"])?;
            }
        }
        "macos" => {
            let label = format!("org.ylcc.{role}");
            let target = format!("gui/{}/{label}", unsafe { libc::getuid() });
            if Command::new("launchctl")
                .args(["print", &target])
                .output()?
                .status
                .success()
            {
                run("launchctl", &["bootout", &target])?;
            }
            let file = home()?
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            if file.exists() {
                fs::remove_file(file)?;
            }
        }
        os => bail!("native user services are unsupported on {os}"),
    }
    println!("Removed {role} service; token, downloaded binary, and workload data are retained.");
    Ok(())
}

fn managed_label(role: &str, user: &str) -> Result<String> {
    role_name(role)?;
    if user.is_empty()
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        || user == "root"
    {
        bail!("system-user must name an existing non-root account")
    }
    Ok(format!("com.kobbokkom.ylcc-{role}"))
}

#[cfg(target_os = "macos")]
fn check_managed_owner(file: &Path, user: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = match fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("refusing unsafe service definition")
    }
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "UserName", "raw", "-o", "-"])
        .arg(file)
        .output()?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != user {
        bail!(
            "existing system service belongs to another account; refusing to replace or remove it"
        )
    }
    Ok(())
}

/// A privileged supervisor installer; the workload itself always runs as the named account.
#[allow(clippy::too_many_arguments)]
pub fn install_system_user(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
    service_user: &str,
) -> Result<()> {
    let label = managed_label(role, service_user)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (label, hub, token_file, data_dir, id, listen, max_jobs);
        bail!(
            "--system-user is supported on macOS; on Linux install a user service under the dedicated account and enable linger"
        )
    }
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            bail!(
                "installing a system LaunchDaemon requires an administrator (sudo); the workload runs unprivileged"
            )
        }
        let file = PathBuf::from("/Library/LaunchDaemons").join(format!("{label}.plist"));
        check_managed_owner(&file, service_user)?;
        let account = Command::new("/usr/bin/id")
            .args(["-u", service_user])
            .output()?;
        if !account.status.success() || String::from_utf8_lossy(&account.stdout).trim() == "0" {
            bail!("system-user must be an existing non-root account")
        }
        let uid: u32 = String::from_utf8(account.stdout)?
            .trim()
            .parse()
            .context("invalid service UID")?;
        let group = Command::new("/usr/bin/id")
            .args(["-gn", service_user])
            .output()?;
        if !group.status.success() {
            bail!("cannot resolve service primary group")
        }
        let group = String::from_utf8(group.stdout)?.trim().to_owned();
        let group_id = Command::new("/usr/bin/id")
            .args(["-g", service_user])
            .output()?;
        if !group_id.status.success() {
            bail!("cannot resolve service primary group ID")
        }
        let gid: u32 = String::from_utf8(group_id.stdout)?
            .trim()
            .parse()
            .context("invalid service GID")?;
        let record = Command::new("/usr/bin/dscl")
            .args([
                ".",
                "-read",
                &format!("/Users/{service_user}"),
                "NFSHomeDirectory",
            ])
            .output()?;
        if !record.status.success() {
            bail!("cannot resolve service account home")
        }
        let record = String::from_utf8(record.stdout)?;
        let home = record
            .strip_prefix("NFSHomeDirectory: ")
            .context("unexpected account home record")?
            .trim();
        if !Path::new(home).is_absolute() {
            bail!("service account HOME must be absolute")
        }
        if !data_dir.exists() {
            let status = Command::new("/usr/bin/sudo")
                .args([
                    "-n",
                    "-u",
                    service_user,
                    "--",
                    "/bin/mkdir",
                    "-p",
                    "-m",
                    "700",
                ])
                .arg(data_dir)
                .status()?;
            if !status.success() {
                bail!(
                    "cannot create data directory as service user: {}",
                    data_dir.display()
                )
            }
        }
        prepare_service_data(data_dir, uid)?;
        // Check real access as the target account before touching a running service.
        for (flag, path) in [
            ("-r", token_file),
            ("-w", data_dir),
            ("-x", env::current_exe()?.as_path()),
        ] {
            let status = Command::new("/usr/bin/sudo")
                .args(["-n", "-u", service_user, "--", "/bin/test", flag])
                .arg(path)
                .status()?;
            if !status.success() {
                bail!("service account lacks {flag} access to {}", path.display())
            }
        }
        let (args, data_dir) =
            service_arguments(role, hub, token_file, data_dir, id, listen, max_jobs)?;
        let path = format!(
            "{home}/.local/bin:{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        let log_root = Path::new("/private/var/log/ylcc");
        let log_dir = log_root.join(service_user);
        let plist = launch_agent(&args, &data_dir, &log_dir, &path, role)?
            .replace(&format!("<string>org.ylcc.{role}</string>"), &format!("<string>{label}</string>\n<key>UserName</key><string>{}</string>\n<key>GroupName</key><string>{}</string>", xml(service_user)?, xml(&group)?))
            .replace("<key>EnvironmentVariables</key><dict>", &format!("<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string>", xml(home)?));
        prepare_log_directory(log_root, 0, 0o755)?;
        prepare_launchd_logs(&log_dir, role, 0, uid, gid, 0o755)?;
        for directory in [log_root, log_dir.as_path()] {
            let status = Command::new("/usr/bin/sudo")
                .args(["-n", "-u", service_user, "--", "/bin/test", "-w"])
                .arg(directory)
                .status()?;
            if status.code() != Some(1) {
                bail!(
                    "cannot confirm service user cannot replace log files in {}",
                    directory.display()
                )
            }
        }
        let target = format!("system/{label}");
        if Command::new("launchctl")
            .args(["print", &target])
            .output()?
            .status
            .success()
        {
            run("launchctl", &["bootout", &target])?;
        }
        fs::write(&file, plist)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;
        run("launchctl", &["enable", &target])?;
        run(
            "launchctl",
            &["bootstrap", "system", &file.to_string_lossy()],
        )?;
        println!(
            "Installed {label} as {service_user}; workload data volumes must be mounted before startup."
        );
        Ok(())
    }
}

pub fn uninstall_system_user(role: &str, service_user: &str) -> Result<()> {
    let label = managed_label(role, service_user)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = label;
        bail!("--system-user is supported on macOS only")
    }
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            bail!("removing a system LaunchDaemon requires an administrator (sudo)")
        }
        let file = PathBuf::from("/Library/LaunchDaemons").join(format!("{label}.plist"));
        if !file.exists() {
            return Ok(());
        }
        check_managed_owner(&file, service_user)?;
        let target = format!("system/{label}");
        if Command::new("launchctl")
            .args(["print", &target])
            .output()?
            .status
            .success()
        {
            run("launchctl", &["bootout", &target])?;
        }
        fs::remove_file(file)?;
        println!("Removed {label}; token, binary, and workload data are retained.");
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "macos")]
    fn managed_access_probe_uses_the_macos_test_binary() {
        assert!(
            Command::new("/bin/test")
                .args(["-x", "/bin/test"])
                .status()
                .unwrap()
                .success()
        );
    }
    #[test]
    fn systemd_does_not_expand_paths_or_inject_lines() {
        let unit = systemd_unit(
            &["/a b/ylcc".into(), "a\";$HOME%h\\x".into()],
            Path::new("/data dir"),
            "/bin:$HOME/%h",
            "worker",
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/a b/ylcc\" \"a\\\";$$HOME%%h\\\\x\""));
        assert!(unit.contains("Environment=\"PATH=/bin:$HOME/%%h\""));
        assert!(systemd_arg("x\nExecStart=/evil").is_err());
    }
    #[test]
    fn systemd_working_directory_is_an_unquoted_absolute_value() {
        let unit = systemd_unit(
            &["/usr/bin/true".into()],
            Path::new("/data dir/$cache%h"),
            "/usr/bin:/bin",
            "worker",
        )
        .unwrap();
        assert!(unit.contains("\nWorkingDirectory=/data dir/$cache%%h\n"));
        assert!(!unit.contains("WorkingDirectory=\""));
        assert!(
            systemd_unit(
                &["/usr/bin/true".into()],
                Path::new("relative"),
                "/bin",
                "worker"
            )
            .is_err()
        );
        if Command::new("systemd-analyze")
            .arg("--version")
            .output()
            .is_ok()
        {
            let directory = tempfile::Builder::new()
                .prefix("ylcc service % ")
                .tempdir()
                .unwrap();
            let path = directory.path().join("ylcc-worker.service");
            fs::write(
                &path,
                systemd_unit(
                    &["/usr/bin/true".into()],
                    directory.path(),
                    "/usr/bin:/bin",
                    "worker",
                )
                .unwrap(),
            )
            .unwrap();
            let output = Command::new("systemd-analyze")
                .arg("verify")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "systemd unit rejected: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[test]
    fn system_supervisor_requires_an_explicit_nonroot_account() {
        assert!(managed_label("worker", "root").is_err());
        assert!(managed_label("worker", "-bad;name").is_err());
        assert!(managed_label("hub", "").is_err());
        assert_eq!(
            managed_label("worker", "_ylcc").unwrap(),
            "com.kobbokkom.ylcc-worker"
        );
    }
    #[test]
    fn launchd_escapes_untrusted_xml() {
        let plist = launch_agent(
            &["/bin/ylcc".into(), "<&\"' >".into()],
            Path::new("/data&cache"),
            Path::new("/private/var/log/ylcc/user&name"),
            "/bin",
            "worker",
        )
        .unwrap();
        assert!(plist.contains("&lt;&amp;&quot;&apos; &gt;"));
        assert!(plist.contains("/data&amp;cache"));
        assert!(plist.contains("<key>StandardOutPath</key><string>/private/var/log/ylcc/user&amp;name/service-worker.log</string>"));
        assert!(!plist.contains("<string><&"));
        assert!(role_name("worker\nanything").is_err());
        #[cfg(target_os = "macos")]
        {
            let file = tempfile::NamedTempFile::new().unwrap();
            fs::write(file.path(), &plist).unwrap();
            assert!(
                Command::new("/usr/bin/plutil")
                    .arg("-lint")
                    .arg(file.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    #[test]
    fn launchd_logs_are_separate_from_external_working_data() {
        let plist = launch_agent(
            &["/bin/ylcc".into()],
            Path::new("/Volumes/External jobs/worker"),
            Path::new("/Users/build/Library/Logs/ylcc"),
            "/bin",
            "worker",
        )
        .unwrap();
        assert!(
            plist.contains(
                "<key>WorkingDirectory</key><string>/Volumes/External jobs/worker</string>"
            )
        );
        assert!(plist.contains("<key>StandardOutPath</key><string>/Users/build/Library/Logs/ylcc/service-worker.log</string>"));
        assert!(plist.contains("<key>StandardErrorPath</key><string>/Users/build/Library/Logs/ylcc/service-worker.err.log</string>"));
        assert!(!plist.contains("/Volumes/External jobs/worker/service-"));
        for path in [
            "relative",
            "/logs/../other",
            "/logs\nother",
            "/logs\0other",
            "/logs\u{1}other",
        ] {
            assert!(
                launch_agent(&[], Path::new("/data"), Path::new(path), "/bin", "worker").is_err()
            );
        }
    }
    #[test]
    fn launchd_logs_reject_unsafe_paths_and_owners_without_overwriting() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let logs = root.join("logs");
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        prepare_launchd_logs(&logs, "worker", uid, uid, gid, 0o700).unwrap();
        assert_eq!(fs::metadata(&logs).unwrap().mode() & 0o777, 0o700);
        let output = logs.join("service-worker.log");
        let metadata = fs::metadata(&output).unwrap();
        assert_eq!(
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
            (uid, gid, 0o600)
        );
        fs::write(&output, "existing log").unwrap();
        prepare_launchd_logs(&logs, "worker", uid, uid, gid, 0o700).unwrap();
        assert_eq!(fs::read_to_string(&output).unwrap(), "existing log");
        assert!(
            prepare_launchd_logs(&logs, "worker", uid, uid.wrapping_add(1), gid, 0o700).is_err()
        );
        assert_eq!(fs::read_to_string(&output).unwrap(), "existing log");
        fs::set_permissions(&output, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(prepare_launchd_logs(&logs, "worker", uid, uid, gid, 0o700).is_err());
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();

        let victim = root.join("valuable");
        fs::write(&victim, "untouched").unwrap();
        let unsafe_log = logs.join("service-hub.log");
        symlink(&victim, &unsafe_log).unwrap();
        assert!(prepare_launchd_logs(&logs, "hub", uid, uid, gid, 0o700).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        fs::remove_file(&unsafe_log).unwrap();
        fs::create_dir(&unsafe_log).unwrap();
        assert!(prepare_launchd_logs(&logs, "hub", uid, uid, gid, 0o700).is_err());
        fs::remove_dir(&unsafe_log).unwrap();
        fs::hard_link(&victim, &unsafe_log).unwrap();
        assert!(prepare_launchd_logs(&logs, "hub", uid, uid, gid, 0o700).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");

        let link = root.join("linked-directory");
        symlink(&logs, &link).unwrap();
        assert!(prepare_launchd_logs(&link, "worker", uid, uid, gid, 0o700).is_err());
        assert!(prepare_log_directory(&logs, uid.wrapping_add(1), 0o700).is_err());
        let insecure = root.join("insecure-parent");
        fs::create_dir(&insecure).unwrap();
        fs::set_permissions(&insecure, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            prepare_launchd_logs(&insecure.join("logs"), "worker", uid, uid, gid, 0o700).is_err()
        );
        assert!(!insecure.join("logs").exists());
    }
    #[test]
    fn service_data_preparation_preserves_owned_targets_and_rejects_foreign_owners() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("worker");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(directory.join("retained"), "keep").unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(prepare_service_data(&directory, uid.wrapping_add(1)).is_err());
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o7777, 0o755);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
        prepare_service_data(&directory, uid).unwrap();
        let before = fs::metadata(&directory).unwrap();
        assert_eq!(before.mode() & 0o7777, 0o700);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let link = temporary.path().join("configured-link");
        symlink(&directory, &link).unwrap();
        prepare_service_data(&link, uid).unwrap();
        let after = fs::metadata(&directory).unwrap();
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec())
        );
        assert_eq!(
            fs::read_to_string(directory.join("retained")).unwrap(),
            "keep"
        );
    }
}

// PowerShell single-quoted strings preserve paths and arguments literally.
#[cfg(any(windows, test))]
fn powershell_literal(value: &str) -> Result<String> {
    checked(value)?;
    Ok(format!("'{}'", value.replace('\'', "''")))
}

#[cfg(windows)]
fn powershell(script: &str) -> Result<String> {
    let script = format!(
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); {script}"
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .context("run Windows PowerShell; Task Scheduler and ScheduledTasks must be available")?;
    if !output.status.success() {
        bail!(
            "Task Scheduler operation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(windows)]
fn windows_task(role: &str) -> Result<(String, PathBuf)> {
    role_name(role)?;
    if role != "worker" {
        bail!("Windows background services currently support the worker role only")
    }
    let sid = powershell("[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value")?;
    if !sid.starts_with("S-1-")
        || !sid
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'-' || c == b'S')
    {
        bail!("cannot resolve the current Windows account SID")
    }
    let directory = PathBuf::from(
        env::var_os("LOCALAPPDATA")
            .context("LOCALAPPDATA is required for Windows user services")?,
    )
    .join("ylcc")
    .join("service");
    Ok((format!("YLCC-{role}-{sid}"), directory))
}

#[cfg(any(windows, test))]
fn windows_runner(args: &[String], directory: &Path, path: &str, log: &Path) -> Result<String> {
    let (executable, args) = args.split_first().context("missing service executable")?;
    let args = args
        .iter()
        .map(|s| powershell_literal(s))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "$ErrorActionPreference = 'Stop'\r\n$env:PATH = {}\r\nSet-Location -LiteralPath {}\r\n$ylccArgs = @({args})\r\n$ErrorActionPreference = 'Continue'\r\ndo {{ & {} @ylccArgs >> {} 2>&1 }} while ($LASTEXITCODE -eq {restart})\r\nexit $LASTEXITCODE\r\n",
        powershell_literal(path)?,
        powershell_literal(&directory.to_string_lossy())?,
        powershell_literal(executable)?,
        powershell_literal(&log.to_string_lossy())?,
        restart = crate::worker::RESTART_EXIT_CODE,
    ))
}

/// Start at boot without a login: a Windows S4U task, which only an elevated shell can register.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)]
pub fn install_at_startup(_: &str, _: &str, _: &Path, _: &Path, _: &str, _: &str, _: usize) -> Result<()> {
    bail!("--at-startup is for Windows; use --system-user on macOS, or a user service with linger on Linux")
}
#[cfg(windows)]
pub fn install_at_startup(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
) -> Result<()> {
    install_task(role, hub, token_file, data_dir, id, listen, max_jobs, true)
}

/// Windows workers run as the installing user while that user is logged in.
#[cfg(windows)]
pub fn install(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
) -> Result<()> {
    install_task(role, hub, token_file, data_dir, id, listen, max_jobs, false)
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn install_task(
    role: &str,
    hub: &str,
    token_file: &Path,
    data_dir: &Path,
    id: &str,
    listen: &str,
    max_jobs: usize,
    at_startup: bool,
) -> Result<()> {
    use std::{io::Write, os::windows::fs::OpenOptionsExt};
    let (name, directory) = windows_task(role)?;
    crate::platform::private_dir(&directory)?;
    crate::platform::private_dir(data_dir)?;
    let (args, data_dir) =
        service_arguments(role, hub, token_file, data_dir, id, listen, max_jobs)?;
    // A running worker holds its log open, so stop it before reopening the log on reinstall.
    let name = powershell_literal(&name)?;
    let stop = format!(
        "$ErrorActionPreference='Stop'; $task = Get-ScheduledTask -TaskPath '\\' -ErrorAction Stop | Where-Object {{ $_.TaskName -eq {name} }}; if ($task) {{ Stop-ScheduledTask -InputObject $task }}"
    );
    powershell(&stop)?;
    let script_file = directory.join("worker.ps1");
    let log = directory.join("worker.log");
    // Stopping the task does not wait for the worker process to exit and release the log.
    for _ in 0..30 {
        if !log.exists() || fs::OpenOptions::new().append(true).open(&log).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    // Open only a protected regular file before PowerShell redirects into it.
    let file = match crate::platform::create_private_file(&log) {
        Ok(file) => file,
        Err(error) if log.is_file() => {
            let file = fs::OpenOptions::new()
                .append(true)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&log)
                .context(error)?;
            crate::platform::protect_file(&file)?;
            file
        }
        Err(error) => return Err(error),
    };
    drop(file);
    let runner = windows_runner(
        &args,
        &data_dir,
        &env::var("PATH").unwrap_or_default(),
        &log,
    )?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    crate::platform::protect_file(temporary.as_file())?;
    // Windows PowerShell 5.1 needs a BOM to decode non-ASCII paths as UTF-8.
    temporary.write_all(b"\xef\xbb\xbf")?;
    temporary.write_all(runner.as_bytes())?;
    temporary.as_file().sync_all()?;
    crate::platform::replace_file(temporary.path(), &script_file)?;
    let action_args = format!(
        "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
        script_file.display()
    );
    // S4U runs without a logon session or stored password, so it has no network credentials.
    let (trigger, logon) = if at_startup {
        ("-AtStartup", "S4U")
    } else {
        ("-AtLogOn -User $user", "Interactive")
    };
    let install = format!(
        "$ErrorActionPreference='Stop'; $user=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; \
         $action=New-ScheduledTaskAction -Execute (Join-Path $PSHOME 'powershell.exe') -Argument {}; \
         $trigger=New-ScheduledTaskTrigger {trigger}; \
         $principal=New-ScheduledTaskPrincipal -UserId $user -LogonType {logon} -RunLevel Limited; \
         $settings=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1); \
         Register-ScheduledTask -TaskName {name} -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null; \
         Start-ScheduledTask -TaskName {name}",
        powershell_literal(&action_args)?,
    );
    powershell(&install).with_context(|| {
        if at_startup { "registering a boot task without login needs an elevated (administrator) shell" } else { "register the worker task" }
    })?;
    if at_startup {
        println!("Installed {name}. The worker starts at boot without a login and runs unelevated as this user. Log: {}", log.display());
    } else {
        println!(
            "Installed {name}. The worker starts at this user's logon and runs only while logged in; locking the session is supported. Log: {}",
            log.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall(role: &str) -> Result<()> {
    let (name, directory) = windows_task(role)?;
    let name = powershell_literal(&name)?;
    powershell(&format!(
        "$ErrorActionPreference='Stop'; $task=Get-ScheduledTask -TaskPath '\\' -ErrorAction Stop | Where-Object {{ $_.TaskName -eq {name} }}; if ($task) {{ Stop-ScheduledTask -InputObject $task; Unregister-ScheduledTask -InputObject $task -Confirm:$false }}"
    ))?;
    let script = directory.join("worker.ps1");
    if script.exists() {
        fs::remove_file(script)?;
    }
    println!("Removed worker scheduled task; token, binary, logs, and workload data are retained.");
    Ok(())
}

#[cfg(test)]
mod windows_tests {
    use super::*;
    #[test]
    fn runner_preserves_arguments_without_powershell_expansion() {
        let args = vec![
            r"C:\Program Files\ylcc.exe".into(),
            "a' ; $(throw 'injected') & x".into(),
            "".into(),
        ];
        let runner = windows_runner(
            &args,
            Path::new("C:/data"),
            "C:/tools;$notExpanded",
            Path::new("C:/log"),
        )
        .unwrap();
        assert!(runner.contains("$ylccArgs = @('a'' ; $(throw ''injected'') & x', '')"));
        assert!(runner.contains("$env:PATH = 'C:/tools;$notExpanded'"));
        assert!(
            windows_runner(
                &["ylcc".into(), "line\nbreak".into()],
                Path::new("C:/data"),
                "",
                Path::new("C:/log")
            )
            .is_err()
        );
    }
    #[test]
    #[cfg(windows)]
    fn powershell_arguments_roundtrip_on_windows() {
        let values = [
            "space argument",
            "한글 경로/작업",
            "a' ; $(throw 'injected') & x",
            "",
            r"C:\trailing\",
        ];
        let array = values
            .iter()
            .map(|s| powershell_literal(s).unwrap())
            .collect::<Vec<_>>()
            .join(",");
        let output =
            powershell(&format!("ConvertTo-Json -Compress -InputObject @({array})")).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed, values);
    }
    #[test]
    #[cfg(windows)]
    fn runner_executes_under_unicode_paths_and_keeps_stderr_nonfatal() {
        use std::io::Write;
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("한글 작업 ' 폴더");
        fs::create_dir(&directory).unwrap();
        let log = directory.join("worker.log");
        let script = directory.join("worker.ps1");
        let runner = windows_runner(
            &[
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Write-Output 'NATIVE_OK'; Write-Error 'stderr marker'; exit 0".into(),
            ],
            &directory,
            &env::var("PATH").unwrap(),
            &log,
        )
        .unwrap();
        let mut file = fs::File::create(&script).unwrap();
        file.write_all(b"\xef\xbb\xbf").unwrap();
        file.write_all(runner.as_bytes()).unwrap();
        drop(file);
        powershell(&format!(
            "& {} -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}; exit $LASTEXITCODE",
            powershell_literal("powershell.exe").unwrap(),
            powershell_literal(&script.to_string_lossy()).unwrap()
        ))
        .unwrap();
        let bytes = fs::read(&log).unwrap();
        // Windows PowerShell 5.1 redirection writes UTF-16LE.
        let text = String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        assert!(
            text.contains("NATIVE_OK"),
            "native command did not run: {text}"
        );
        assert!(
            text.contains("stderr marker"),
            "native stderr was not captured: {text}"
        );
    }
}
