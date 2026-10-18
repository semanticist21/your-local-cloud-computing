use std::{fs, path::Path, process::Command};

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dashboard_assets)");
    if Path::new("dashboard/dist/index.html").exists() {
        println!("cargo:rustc-cfg=dashboard_assets");
    }
    println!("cargo:rerun-if-changed=dashboard/dist/index.html");
    println!("cargo:rustc-env=YLCC_VERSION={}", version());
}

/// Release builds (`YLCC_RELEASE=1`) report the package version; any other build adds the
/// commit, or `+source` without Git, so an unreleased binary never claims a published version.
fn version() -> String {
    let package = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rerun-if-env-changed=YLCC_RELEASE");
    if std::env::var_os("YLCC_RELEASE").is_some_and(|v| v == "1") {
        return package;
    }
    if let Ok(head) = fs::read_to_string(".git/HEAD") {
        println!("cargo:rerun-if-changed=.git/HEAD");
        println!("cargo:rerun-if-changed=.git/index");
        println!("cargo:rerun-if-changed=.git/packed-refs");
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    match git(&["rev-parse", "--short=9", "HEAD"]) {
        Some(commit) if !commit.is_empty() => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            format!("{package}+g{commit}{}", if dirty { ".dirty" } else { "" })
        }
        _ => format!("{package}+source"),
    }
}
