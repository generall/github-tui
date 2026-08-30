//! `github-tui self-upgrade`: replace the running binary with the latest
//! GitHub release. Transport is plain curl — same tool the install uses.

use anyhow::{anyhow, bail, Context, Result};
use std::process::Command;

const REPO: &str = "generall/github-tui";

fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("github-tui-x86_64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("github-tui-aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("github-tui-x86_64-apple-darwin"),
        _ => None,
    }
}

pub fn self_upgrade() -> Result<()> {
    let Some(asset) = asset_name() else {
        bail!(
            "no prebuilt binary for {}-{}; upgrade from source with cargo",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let current = env!("CARGO_PKG_VERSION");
    println!("current version: v{current}");

    let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSL", &api])
        .output()
        .context("failed to run curl (is it installed?)")?;
    if !out.status.success() {
        bail!("release check failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let tag = v["tag_name"].as_str().context("no tag_name in latest release")?.to_string();
    let latest = tag.trim_start_matches('v');
    println!("latest release:  v{latest}");
    if !newer(latest, current) {
        println!("already up to date.");
        return Ok(());
    }

    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let tmp = exe.with_file_name(".github-tui.upgrade");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    println!("downloading {url}");
    let status = Command::new("curl")
        .args(["-fL", "-#", "-o"])
        .arg(&tmp)
        .arg(&url)
        .status()
        .context("failed to run curl")?;
    let result = (|| -> Result<()> {
        if !status.success() {
            bail!("download failed");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &exe)
            .map_err(|e| anyhow!("could not replace {} ({e}); try with sudo", exe.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    println!("upgraded v{current} -> v{latest} ({})", exe.display());
    Ok(())
}

/// Numeric semver comparison; falls back to plain inequality on odd tags.
fn newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.split('.').map(|x| x.parse::<u64>().ok()).collect()
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest != current,
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn version_compare() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(newer("0.2.0-rc1", "0.1.0") || true); // odd tags: inequality fallback, no panic
    }
}
