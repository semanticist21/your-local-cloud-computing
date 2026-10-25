use crate::{
    model::{FileEntry, safe_relative},
    platform,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Serialize)]
pub struct Excluded {
    pub path: String,
    pub reason: String,
}
pub struct Snapshot {
    pub files: Vec<FileEntry>,
    pub excluded: Vec<Excluded>,
    pub blobs: tempfile::TempDir,
}

fn git_paths(root: &Path, ignored: bool) -> Option<Vec<String>> {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).args(["ls-files", "-z"]);
    if ignored {
        c.args(["--others", "--ignored", "--exclude-standard"]);
    } else {
        c.args(["--cached", "--others", "--exclude-standard"]);
    }
    let o = c.output().ok()?;
    if !o.status.success() {
        return None;
    }
    let text = String::from_utf8(o.stdout).ok()?;
    Some(
        text.split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}
fn in_selection(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| path.starts_with(d))
}

fn collect_git_paths(
    root: &Path,
    repository: &Path,
    dirs: &[PathBuf],
    selected: &mut BTreeSet<String>,
    executable: &mut BTreeSet<String>,
    excluded: &mut Vec<Excluded>,
) -> Result<bool> {
    let Some(paths) = git_paths(repository, false) else {
        return Ok(false);
    };
    let prefix = repository.strip_prefix(root)?;
    let staged = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["ls-files", "--stage", "-z"])
        .output()?;
    anyhow::ensure!(staged.status.success(), "could not inspect Git submodules");
    let staged = String::from_utf8(staged.stdout).context("Git path is not UTF-8")?;
    for entry in staged
        .split('\0')
        .filter(|entry| entry.starts_with("100755 "))
    {
        if let Some((_, name)) = entry.split_once('\t') {
            let path = prefix.join(name);
            if in_selection(&path, dirs)
                || platform::relative_path(&path).is_ok_and(|name| selected.contains(&name))
            {
                executable.insert(platform::relative_path(&path)?);
            }
        }
    }
    let submodules: BTreeSet<_> = staged
        .split('\0')
        .filter(|entry| entry.starts_with("160000 "))
        .filter_map(|entry| entry.split_once('\t').map(|(_, name)| name))
        .collect();
    for name in paths {
        if submodules.contains(name.as_str()) {
            continue;
        }
        let path = prefix.join(name);
        if in_selection(&path, dirs) {
            selected.insert(platform::relative_path(&path)?);
        }
    }
    if let Some(paths) = git_paths(repository, true) {
        for name in paths {
            let path = prefix.join(name);
            let name = platform::relative_path(&path)?;
            if in_selection(&path, dirs) && !selected.contains(&name) {
                excluded.push(Excluded {
                    path: name,
                    reason: "Git ignore rule".into(),
                });
            }
        }
    }
    for name in submodules {
        safe_relative(name)?;
        let path = prefix.join(name);
        if !in_selection(&path, dirs) && !dirs.iter().any(|dir| dir.starts_with(&path)) {
            continue;
        }
        let module = root.join(&path);
        if fs::symlink_metadata(&module).is_ok_and(|m| platform::is_link(&m)) {
            excluded.push(Excluded {
                path: platform::relative_path(&path)?,
                reason: "submodule is a symlink; symlinks are not followed".into(),
            });
            continue;
        }
        anyhow::ensure!(
            module.join(".git").exists(),
            "submodule {} is not initialized; run git submodule update --init --recursive before submitting",
            path.display()
        );
        anyhow::ensure!(
            platform::command_path(&module.canonicalize()?).starts_with(root),
            "submodule escaped project root: {}",
            path.display()
        );
        let position = Command::new("git")
            .arg("-C")
            .arg(&module)
            .args(["rev-parse", "--show-prefix"])
            .output()?;
        anyhow::ensure!(
            position.status.success() && position.stdout == b"\n",
            "submodule {} does not have its own working tree; run git submodule update --init --recursive before submitting",
            path.display()
        );
        anyhow::ensure!(
            collect_git_paths(root, &module, dirs, selected, executable, excluded)?,
            "could not read submodule {}; run git submodule update --init --recursive before submitting",
            path.display()
        );
    }
    Ok(true)
}

pub fn create(root: &Path, inputs: &[PathBuf], whole: bool) -> Result<Snapshot> {
    let root = platform::command_path(&root.canonicalize()?);
    if !whole && inputs.is_empty() {
        bail!("select --input files/folders, or explicitly use --project")
    }
    let blobs = tempfile::Builder::new()
        .prefix("ylcc-snapshot-")
        .tempdir()?;
    platform::private_dir(blobs.path())?;
    let mut selected = BTreeSet::new();
    let mut executable = BTreeSet::new();
    let mut dirs = Vec::new();
    let mut excluded = Vec::new();
    if whole {
        dirs.push(PathBuf::new());
    }
    let mut expanded = Vec::new();
    for p in inputs {
        expanded.extend(expand_wildcard(&root, p)?);
    }
    for p in &expanded {
        let abs = if p.is_absolute() {
            platform::command_path(p)
        } else {
            root.join(p)
        };
        let rel = abs
            .strip_prefix(&root)
            .context("input is outside project root")?;
        if rel.as_os_str().is_empty() || rel == Path::new(".") {
            dirs.push(PathBuf::new());
            continue;
        }
        let name = platform::relative_path(rel)?;
        let meta = fs::symlink_metadata(&abs).with_context(|| format!("input {}", p.display()))?;
        if platform::is_link(&meta) {
            bail!("explicit symlink input is not supported: {name}")
        }
        if meta.is_file() {
            selected.insert(name);
        } else if meta.is_dir() {
            dirs.push(rel.to_path_buf());
        } else {
            bail!("input is not a regular file or directory: {name}")
        }
    }
    if !collect_git_paths(
        &root,
        &root,
        &dirs,
        &mut selected,
        &mut executable,
        &mut excluded,
    )? {
        for dir in &dirs {
            let mut walk = ignore::WalkBuilder::new(root.join(dir));
            walk.hidden(false).require_git(false).follow_links(false);
            for entry in walk.build() {
                let e = entry?;
                let p = e.path();
                if p == root || e.file_type().is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let name = platform::relative_path(p.strip_prefix(&root)?)?;
                selected.insert(name);
            }
        }
    }
    let mut files = Vec::new();
    for name in selected {
        safe_relative(&name)?;
        if Path::new(&name).components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
        }) {
            excluded.push(Excluded {
                path: name,
                reason: "Git internal data".into(),
            });
            continue;
        }
        let path = root.join(&name);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                excluded.push(Excluded {
                    path: name,
                    reason: "deleted from working tree".into(),
                });
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() || platform::is_link(&meta) {
            excluded.push(Excluded {
                path: name,
                reason: "not a regular file; symlinks are not followed".into(),
            });
            continue;
        }
        if !platform::command_path(&path.canonicalize()?).starts_with(&root) {
            bail!("input escaped root: {name}")
        }
        let mut input = platform::open_input(&path)?;
        let before = input.metadata()?;
        let mut tmp = tempfile::NamedTempFile::new_in(blobs.path())?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            tmp.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        if !platform::unchanged(&input, &before, &path, size)?
            || !platform::command_path(&path.canonicalize()?).starts_with(&root)
        {
            bail!("input changed while creating snapshot: {name}; submit again")
        }
        let hash = hex::encode(hasher.finalize());
        let dest = blobs.path().join(&hash);
        if !dest.exists() {
            tmp.persist(&dest)?;
            #[cfg(unix)]
            platform::set_mode(&File::open(&dest)?, 0o400)?;
        }
        let mode = if cfg!(windows) && executable.contains(&name) {
            0o755
        } else {
            platform::mode(&before)
        };
        files.push(FileEntry {
            path: name,
            hash,
            size,
            mode,
        });
    }
    Ok(Snapshot {
        files,
        excluded,
        blobs,
    })
}

/// `dir/*.dylib`: a `*` in the last component selects the matching regular files in `dir`
/// (not recursively). Like a named file, a match is included even when Git ignores it.
fn expand_wildcard(root: &Path, input: &Path) -> Result<Vec<PathBuf>> {
    let pattern = input.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let abs = if input.is_absolute() { input.to_path_buf() } else { root.join(input) };
    if !pattern.contains('*') || fs::symlink_metadata(&abs).is_ok() {
        return Ok(vec![input.to_path_buf()]);
    }
    let parent = input.parent().unwrap_or(Path::new(""));
    let mut matches = vec![];
    for entry in fs::read_dir(root.join(parent)).with_context(|| format!("input {}", input.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_str().is_some_and(|name| wildcard(pattern, name)) {
            matches.push(parent.join(entry.file_name()));
        }
    }
    anyhow::ensure!(!matches.is_empty(), "input {} matches no files", input.display());
    matches.sort();
    Ok(matches)
}
fn wildcard(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else { return false };
    let mut parts: Vec<_> = parts.collect();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// Serialize without allocating a second in-memory copy of a large manifest.
pub fn json_blob(directory: &Path, value: &impl Serialize) -> Result<(tempfile::NamedTempFile, String)> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
    }
    file.as_file_mut().sync_all()?;
    let hash = hash_file(file.path())?;
    Ok((file, hash))
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

pub fn ensure_safe_directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                if platform::is_link(&m) || !m.is_dir() {
                    bail!(
                        "result directory contains non-directory or symlink: {}",
                        current.display()
                    )
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap()
                .success()
        )
    }
    fn repository(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "-q"]);
        fs::write(root.join("tracked"), "original").unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        git(root, &["add", "."]);
        commit(root);
    }
    fn commit(root: &Path) {
        git(
            root,
            &[
                "-c",
                "user.name=YLCC Test",
                "-c",
                "user.email=ylcc@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        );
    }
    fn add_submodule(root: &Path, source: &Path, destination: &str) {
        git(
            root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.to_str().unwrap(),
                destination,
            ],
        );
        commit(root);
    }
    #[test]
    fn snapshot_tracks_dirty_and_ignores_except_explicit() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        fs::write(d.path().join(".gitignore"), "ignored\ntracked\n").unwrap();
        fs::write(d.path().join("tracked"), "old").unwrap();
        git(d.path(), &["add", "-f", "tracked"]);
        fs::write(d.path().join("tracked"), "dirty").unwrap();
        fs::write(d.path().join("ignored"), "secret").unwrap();
        fs::write(d.path().join("new"), "new").unwrap();
        let s = create(d.path(), &[], true).unwrap();
        assert!(s.files.iter().any(|f| f.path == "tracked"));
        assert!(!s.files.iter().any(|f| f.path == "ignored"));
        fs::write(d.path().join("tracked"), "later").unwrap();
        let f = s.files.iter().find(|f| f.path == "tracked").unwrap();
        assert_eq!(
            fs::read_to_string(s.blobs.path().join(&f.hash)).unwrap(),
            "dirty"
        );
        let explicit = create(d.path(), &["ignored".into()], false).unwrap();
        assert_eq!(explicit.files.len(), 1);
        let pattern = create(d.path(), &["ign*d".into()], false).unwrap();
        assert_eq!(pattern.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["ignored"]);
        assert!(create(d.path(), &["*.none".into()], false).is_err());
        assert!(wildcard("lib*.dylib", "libavcodec.63.dylib") && wildcard("*-arm64", "x-arm64"));
        assert!(!wildcard("lib*.dylib", "libx.so") && !wildcard("a*a", "a"));
    }
    #[test]
    fn no_stale_deleted_file_or_symlink() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        fs::write(d.path().join("gone"), "x").unwrap();
        git(d.path(), &["add", "gone"]);
        fs::remove_file(d.path().join("gone")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("link")).unwrap();
        let s = create(d.path(), &[], true).unwrap();
        assert!(s.files.is_empty());
        assert_eq!(s.excluded.len(), if cfg!(unix) { 2 } else { 1 });
    }
    #[test]
    fn nested_directory_scope() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        fs::create_dir(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/a"), "a").unwrap();
        fs::write(d.path().join("other"), "b").unwrap();
        let s = create(d.path(), &["src".into()], false).unwrap();
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].path, "src/a");
    }
    #[test]
    #[cfg(unix)]
    fn unix_snapshots_keep_names_reserved_only_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["aux.txt", "con/part.rs", "logs/2026-10-16T11:22:33.log"] {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, name).unwrap();
        }
        let snapshot = create(dir.path(), &[], true).unwrap();
        for name in ["aux.txt", "con/part.rs", "logs/2026-10-16T11:22:33.log"] {
            assert!(snapshot.files.iter().any(|file| file.path == name));
        }
    }
    #[test]
    fn initialized_submodules_include_dirty_untracked_and_nested_sources() {
        let temp = tempfile::tempdir().unwrap();
        let leaf = temp.path().join("leaf");
        let middle = temp.path().join("middle");
        let project = temp.path().join("project");
        repository(&leaf);
        repository(&middle);
        repository(&project);
        add_submodule(&middle, &leaf, "nested");
        add_submodule(&project, &middle, "vendor/module");
        git(
            &project,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
                "--quiet",
            ],
        );
        let module = project.join("vendor/module");
        let nested = module.join("nested");
        fs::write(module.join("tracked"), "dirty").unwrap();
        fs::write(module.join("new"), "untracked").unwrap();
        fs::write(module.join("ignored"), "not included").unwrap();
        fs::write(nested.join("tracked"), "nested dirty").unwrap();
        fs::write(nested.join("new"), "nested untracked").unwrap();
        fs::write(nested.join("ignored"), "not included").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&leaf, module.join("outside")).unwrap();
        let snapshot = create(&project, &[], true).unwrap();
        for (path, contents) in [
            ("vendor/module/tracked", "dirty"),
            ("vendor/module/new", "untracked"),
            ("vendor/module/nested/tracked", "nested dirty"),
            ("vendor/module/nested/new", "nested untracked"),
        ] {
            let file = snapshot
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap();
            assert_eq!(
                fs::read_to_string(snapshot.blobs.path().join(&file.hash)).unwrap(),
                contents
            );
        }
        assert!(!snapshot.files.iter().any(|file| {
            file.path.ends_with("/ignored")
                || file.path.contains("/.git/")
                || file.path.ends_with("/.git")
                || file.path.contains("outside")
        }));
        assert!(snapshot.excluded.iter().any(|excluded| {
            excluded.path == "vendor/module/ignored" && excluded.reason == "Git ignore rule"
        }));
        let selected = create(&project, &["vendor/module/nested".into()], false).unwrap();
        assert!(!selected.files.is_empty());
        assert!(
            selected
                .files
                .iter()
                .all(|file| file.path.starts_with("vendor/module/nested/"))
        );
        let explicit = create(&project, &["vendor/module/ignored".into()], false).unwrap();
        assert_eq!(explicit.files.len(), 1);
        assert_eq!(explicit.files[0].path, "vendor/module/ignored");
    }
    #[test]
    fn uninitialized_submodule_is_actionable_and_does_not_break_other_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let project = temp.path().join("project");
        repository(&source);
        repository(&project);
        add_submodule(&project, &source, "vendor/module");
        git(&project, &["submodule", "deinit", "-f", "--all"]);
        let error = create(&project, &[], true).err().unwrap().to_string();
        assert!(error.contains("vendor/module"));
        assert!(error.contains("git submodule update --init --recursive"));
        let selected = create(&project, &["tracked".into()], false).unwrap();
        assert_eq!(selected.files.len(), 1);
        assert_eq!(selected.files[0].path, "tracked");
    }
    #[test]
    #[cfg(unix)]
    fn result_parent_symlink_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/tmp", d.path().join("evil")).unwrap();
        assert!(ensure_safe_directory(&d.path().join("evil/sub")).is_err());
    }
    #[test]
    #[cfg(windows)]
    fn snapshots_native_separators_as_portable_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        fs::write(dir.path().join("src/sub/input.txt"), "input").unwrap();
        let snapshot = create(dir.path(), &[r"src\sub\input.txt".into()], false).unwrap();
        assert_eq!(snapshot.files[0].path, "src/sub/input.txt");
    }
    #[test]
    #[cfg(windows)]
    fn snapshots_preserve_git_executable_bits_for_unix_workers() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("script.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        git(dir.path(), &["add", "script.sh"]);
        git(dir.path(), &["update-index", "--chmod=+x", "script.sh"]);
        let snapshot = create(dir.path(), &["script.sh".into()], false).unwrap();
        assert_eq!(snapshot.files[0].mode, 0o755);
    }
    #[test]
    #[cfg(windows)]
    fn junction_inputs_and_result_parents_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private.txt"), "private").unwrap();
        assert!(
            Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(dir.path().join("junction"))
                .arg(outside.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(create(dir.path(), &["junction".into()], false).is_err());
        assert!(create(dir.path(), &["junction/private.txt".into()], false).is_err());
        assert!(ensure_safe_directory(&dir.path().join("junction/results")).is_err());
        fs::remove_dir(dir.path().join("junction")).unwrap();
    }
}
