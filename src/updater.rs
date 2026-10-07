//! Self-updates from the app's own GitHub releases, fetched through `gh` like
//! everything else. macOS swaps in the app bundle from `octowatcher-macos.tar.gz`;
//! Linux swaps in the release's AppImage when running from one. Anything else
//! (a .deb or .rpm install, a bare binary) can only point at the release page.

use std::{fs, path::Path, path::PathBuf};

use anyhow::{Context as _, Result};
use semver::Version;
use serde::Deserialize;

use crate::github::gh;

const REPO: &str = "mattsverse/octowatch";

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    tag: String,
    /// The release page, for when the update can't be installed in place.
    pub url: String,
    /// This platform's update asset, when the release ships one.
    asset: Option<String>,
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}

/// The latest published release, when it's newer than the running build.
/// Drafts and prereleases never count.
pub fn check() -> Result<Option<Release>> {
    let output = gh(&["api", &format!("repos/{REPO}/releases/latest")])?;
    let latest: Latest = serde_json::from_str(&output).context("unexpected GitHub response")?;
    let version = Version::parse(latest.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag {} is not a version", latest.tag_name))?;
    if version <= current_version() {
        return Ok(None);
    }
    Ok(Some(Release {
        version,
        tag: latest.tag_name,
        url: latest.html_url,
        asset: latest.assets.into_iter().map(|a| a.name).find(|n| is_update_asset(n)),
    }))
}

/// Whether `install` can swap the release in for the running copy, rather
/// than the user fetching it from the release page.
pub fn can_install(release: &Release) -> bool {
    // A dev build would overwrite its own target dir with a release.
    !cfg!(debug_assertions) && release.asset.is_some() && install_target().is_ok()
}

/// Downloads the release and swaps it in for the running copy, which keeps
/// running the old build until it restarts. Returns the path to relaunch.
pub fn install(release: &Release) -> Result<PathBuf> {
    let asset = release
        .asset
        .as_deref()
        .with_context(|| format!("release {} has no update for this platform", release.tag))?;
    let target = install_target()?;
    let parent = target.parent().context("the app has no parent directory")?;
    // Stage next to the target so the swap is a rename within one volume.
    let staging = parent.join(".octowatcher-update");
    fs::remove_dir_all(&staging).ok();
    fs::create_dir_all(&staging)
        .with_context(|| format!("cannot write to {}", parent.display()))?;
    let result = download(release, asset, &staging).and_then(|()| swap(asset, &staging, &target));
    fs::remove_dir_all(&staging).ok();
    result.map(|()| target)
}

fn download(release: &Release, asset: &str, staging: &Path) -> Result<()> {
    let dir = staging.to_str().context("staging path is not UTF-8")?;
    gh(&[
        "release", "download", &release.tag, "--repo", REPO, "--pattern", asset, "--dir", dir,
    ])
    .map(drop)
}

#[cfg(target_os = "macos")]
fn is_update_asset(name: &str) -> bool {
    // One universal bundle covers both architectures.
    name == "octowatcher-macos.tar.gz"
}

#[cfg(target_os = "macos")]
fn install_target() -> Result<PathBuf> {
    std::env::current_exe()?
        .canonicalize()?
        .ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
        .context("not running from an app bundle")
}

#[cfg(target_os = "macos")]
fn swap(asset: &str, staging: &Path, target: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .args(["-xzf", asset])
        .current_dir(staging)
        .status()
        .context("could not run `tar`")?;
    if !status.success() {
        anyhow::bail!("could not unpack {asset}");
    }
    let fresh = staging.join("Octowatcher.app");
    if !fresh.is_dir() {
        anyhow::bail!("{asset} does not contain Octowatcher.app");
    }
    // A bundle can't be renamed over, so move the old one aside first.
    let old = staging.join("previous.app");
    fs::rename(target, &old)?;
    if let Err(err) = fs::rename(&fresh, target) {
        fs::rename(&old, target).ok();
        return Err(err.into());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn is_update_asset(name: &str) -> bool {
    // cargo-bundle names them `octowatcher_<version>_<arch>.AppImage`.
    name.starts_with("octowatcher_")
        && name.ends_with(&format!("_{}.AppImage", std::env::consts::ARCH))
}

#[cfg(not(target_os = "macos"))]
fn install_target() -> Result<PathBuf> {
    // The AppImage runtime exports its own path; the executable itself sits
    // in a read-only mount.
    let appimage = std::env::var_os("APPIMAGE")
        .context("not running from an AppImage; install the update from the release page")?;
    Ok(PathBuf::from(appimage).canonicalize()?)
}

#[cfg(not(target_os = "macos"))]
fn swap(asset: &str, staging: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let fresh = staging.join(asset);
    fs::set_permissions(&fresh, fs::Permissions::from_mode(0o755))?;
    fs::rename(&fresh, target)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn running_executable() -> Result<PathBuf> {
    // AppImages mount their inner executable at a temporary, read-only path.
    match std::env::var_os("APPIMAGE") {
        Some(path) => Ok(PathBuf::from(path).canonicalize()?),
        None => Ok(std::env::current_exe()?.canonicalize()?),
    }
}

#[cfg(target_os = "linux")]
pub fn relaunch(path: &Path) -> Result<()> {
    restart_command(path, std::process::id())
        .spawn()
        .context("could not start relaunch helper")?;
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn restart_command(path: &Path, pid: u32) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;

    // Preserve GPUI's wait-for-exit handoff, passing the PID and path as
    // arguments. Its Linux restart implementation interpolates the path into
    // shell text, which fails for AppImages with spaces or shell metacharacters.
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; exec \"$2\"")
        .arg("octowatcher-restart")
        .arg(pid.to_string())
        .arg(path)
        .process_group(0);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn relaunch_handles_paths_with_spaces_and_shell_metacharacters() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("Octowatcher ' $`% image.AppImage");
        fs::write(&executable, "#!/bin/sh\nprintf 'relaunched\\n'\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut previous = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = previous.id();
        previous.wait().unwrap();
        let result = restart_command(&executable, pid).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"relaunched\n");
    }
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}
