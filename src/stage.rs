//! Stage builds.
//!
//! `cargo install --root <stage>` runs as the invoking user: registry cache,
//! build scripts and proc macros never execute as root. The stage's
//! `.crates2.json` is then the source of truth for what was actually built —
//! version and binary names — regardless of what the index promised earlier.

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Debug)]
pub struct Built {
    pub version: Version,
    pub bins: Vec<String>,
    pub bin_paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Crates2 {
    installs: BTreeMap<String, InstallInfo>,
}

#[derive(Deserialize)]
struct InstallInfo {
    bins: Vec<String>,
}

/// Build `name` from crates.io into the stage root.
pub fn build(name: &str, locked: bool, stage: &Path) -> Result<Built> {
    fs::create_dir_all(stage).with_context(|| format!("creating {}", stage.display()))?;
    let mut cmd = Command::new("cargo");
    cmd.arg("install").arg(name).arg("--root").arg(stage);
    if locked {
        cmd.arg("--locked");
    }
    // Compiler output goes straight to the terminal; the user should see the
    // build exactly as cargo presents it.
    let status = cmd.status().context("failed to spawn cargo")?;
    if !status.success() {
        bail!("cargo install {name} failed with {status}");
    }
    staged_info(name, stage)
}

/// Read what the stage actually contains for `name` from `.crates2.json`.
fn staged_info(name: &str, stage: &Path) -> Result<Built> {
    let path = stage.join(".crates2.json");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: Crates2 =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    // Key format: `name version (source)`. The stage only ever holds one
    // version per crate (cargo replaces on reinstall), but be defensive and
    // take the semver max if we ever see more.
    let mut best: Option<Built> = None;
    for (key, info) in &parsed.installs {
        let mut parts = key.split_whitespace();
        let (Some(key_name), Some(key_version), Some(key_source)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if key_name != name || !key_source.contains(CRATES_IO_SOURCE) {
            continue;
        }
        let version = Version::parse(key_version)
            .with_context(|| format!("unparsable staged version `{key_version}`"))?;
        let replace = match &best {
            Some(b) => version > b.version,
            None => true,
        };
        if replace {
            // Stage bookkeeping is also disk input steering placement; hold
            // it to the same standard as the manifest: valid filenames, no
            // duplicates — caught here, before anything touches the prefix.
            crate::validate::validate_bin_list(&info.bins)
                .with_context(|| format!("stage bookkeeping for `{name}`"))?;
            let bin_dir = stage.join("bin");
            best = Some(Built {
                bin_paths: info.bins.iter().map(|b| bin_dir.join(b)).collect(),
                bins: info.bins.clone(),
                version,
            });
        }
    }
    best.with_context(|| format!("`{name}` missing from stage bookkeeping after build"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_info_parses_crates2() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-stage");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".crates2.json"),
            r#"{"installs":{
                "hexyl 0.14.0 (registry+https://github.com/rust-lang/crates.io-index)":
                    {"bins":["hexyl"]},
                "other 1.0.0 (git+https://example.com/other#abc)":
                    {"bins":["other"]}
            }}"#,
        )
        .unwrap();

        let built = staged_info("hexyl", &dir).unwrap();
        assert_eq!(built.version, Version::parse("0.14.0").unwrap());
        assert_eq!(built.bins, vec!["hexyl"]);
        assert!(
            staged_info("other", &dir).is_err(),
            "git source must not match"
        );
        assert!(staged_info("absent", &dir).is_err());

        // Forged bookkeeping with duplicate bins must fail here, before
        // anything would touch the prefix.
        fs::write(
            dir.join(".crates2.json"),
            r#"{"installs":{
                "dupes 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":
                    {"bins":["foo","foo"]}
            }}"#,
        )
        .unwrap();
        let err = format!("{:#}", staged_info("dupes", &dir).unwrap_err());
        assert!(err.contains("listed twice"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
