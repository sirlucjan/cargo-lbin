// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Installation manifest.
//!
//! Lives at `<prefix>/share/cargo-lbin/manifest.json` so state travels with the
//! system, not with the user. Written via a write-sealed memfd handed to
//! privileged `install` and placed atomically (same-directory temp +
//! rename), so the write path is identical with and without privilege
//! escalation.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::privileged;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    /// Version actually built and installed — source of truth is the stage's
    /// `.crates2.json`, never what the index promised at check time.
    pub version: String,
    /// Binary names this crate installed into `<prefix>/bin`.
    pub bins: Vec<String>,
    /// Whether this managed entry uses `--locked`.
    ///
    /// Stored as build policy and preserved by operations that rebuild
    /// *from* the existing entry — `update`, `migrate`, `install
    /// --reinstall`. An explicit install request sets it from that
    /// request instead, which is why a plain `install NAME` on a
    /// managed crate clears it and `install NAME --locked` sets it.
    #[serde(default)]
    pub locked: bool,
    /// Held at its installed version: excluded from `update --all`,
    /// refused by `update NAME`/`install NAME` until unpinned —
    /// but not by `install --reinstall`, which rebuilds the version the
    /// pin declares and restates the pin with it. A statement about the
    /// future — survives everything but an explicit `unpin`.
    /// Absent in older manifests, which reads as "not pinned".
    #[serde(default)]
    pub pinned: bool,
    /// The `rustc` value Cargo recorded for this install — copied
    /// verbatim (the full multi-line `rustc -vV` report) from the
    /// stage's `.crates2.json`: the invocation's own testimony, after
    /// its own toolchain resolution. The artefact and this record come
    /// from the same build and pass through the same commit path.
    /// Recorded, never interpreted — no comparison against the current
    /// toolchain, no policy; the consequence rule lives with the user.
    /// Absent when no record is available — an entry predating
    /// 0.18.0, or a cargo that wrote none — which reads as unknown,
    /// with the cause deliberately not guessed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_with_rustc: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub crates: BTreeMap<String, Entry>,
}

impl Manifest {
    pub fn path(prefix: &Path) -> PathBuf {
        prefix.join("share/cargo-lbin/manifest.json")
    }

    pub fn load(prefix: &Path) -> Result<Self> {
        let path = Self::path(prefix);
        let manifest = Self::load_unvalidated(prefix)?;
        manifest
            .validate()
            .with_context(|| format!("invalid manifest at {}", path.display()))?;
        Ok(manifest)
    }

    /// `load` without `validate` — for `verify` alone, whose job is to
    /// name each broken invariant behind `load`'s one opaque refusal.
    /// Everything that mutates keeps `load`; this reader feeds code that
    /// only ever `stat`s.
    pub fn load_unvalidated(prefix: &Path) -> Result<Self> {
        let path = Self::path(prefix);
        // Bytes first, parse second — not read_to_string, whose UTF-8 check
        // turns a corrupt *file* into an I/O error before serde sees it; a
        // 0xff belongs to the parse world, and only fs::read keeps it there.
        match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .with_context(|| format!("corrupt manifest at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The manifest steers file operations that may run under sudo, so it
    /// is untrusted input: names safe, bins plain unique filenames,
    /// versions semver, ownership a function (`check_collisions` and
    /// `remove` assume it). Validated once here.
    fn validate(&self) -> Result<()> {
        let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, entry) in &self.crates {
            crate::validate::validate_name(name)?;
            crate::validate::validate_bin_list(&entry.bins)
                .with_context(|| format!("crate `{name}`"))?;
            for bin in &entry.bins {
                if let Some(prev) = owners.insert(bin, name) {
                    bail!("binary `{bin}` is owned by both `{prev}` and `{name}`");
                }
            }
            semver::Version::parse(&entry.version).with_context(|| {
                format!("crate `{name}` has invalid version `{}`", entry.version)
            })?;
        }
        Ok(())
    }

    /// Serialize into a sealed anonymous memfd and hand root the fd path:
    /// no temp file exists for a leftover build process to find, and the
    /// seals guarantee the bytes root copies are the bytes serialized here.
    /// Placement is atomic (`install_atomic`): a crash mid-write leaves the
    /// old manifest whole, never half of the new one.
    pub fn store(&self, prefix: &Path) -> Result<()> {
        // CLI form: the caller's own terminal, so re-deriving the policy
        // here is harmless. A captured pipeline must not use this — see
        // store_with_policy.
        self.store_with_policy(prefix, privileged::Policy::for_prefix(prefix))
    }

    /// `store` with the caller's policy threaded, not re-derived: the
    /// commit is the pipeline's last privileged write, and a quiet reset
    /// of the Screen axis here would hand the end of a captured run back
    /// to interactive sudo.
    pub fn store_with_policy(&self, prefix: &Path, policy: privileged::Policy) -> Result<()> {
        // Symmetry with load(): never knowingly write state we would refuse
        // to read back; the last line of defense.
        self.validate()
            .context("refusing to store invalid manifest")?;
        let mut raw = serde_json::to_string_pretty(self)?;
        raw.push('\n');
        let sealed = privileged::SealedSource::from_bytes(raw.as_bytes())?;
        privileged::install_sealed(policy, &sealed, &Self::path(prefix), "644")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(bins: &[&str]) -> Entry {
        Entry {
            pinned: false,
            built_with_rustc: None,
            version: "1.0.0".to_owned(),
            bins: bins.iter().map(|s| (*s).to_owned()).collect(),
            locked: false,
        }
    }

    #[test]
    fn pinned_defaults_to_false_for_older_manifests() {
        // Written before pins existed: no `pinned` key at all.
        let raw = r#"{"crates":{"bat":{"version":"0.26.0","bins":["bat"],"locked":true}}}"#;
        let m: Manifest = serde_json::from_str(raw).unwrap();
        assert!(!m.crates["bat"].pinned);
        assert!(m.crates["bat"].locked);
        // And the flag round-trips once set.
        let mut m = m;
        m.crates.get_mut("bat").unwrap().pinned = true;
        let again: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert!(again.crates["bat"].pinned);
    }

    #[test]
    fn store_refuses_invalid_state() {
        // In-memory construction of a manifest that load() would reject
        // must be caught by store() before anything is written.
        let mut m = Manifest::default();
        m.crates.insert("foo".to_owned(), entry(&["x", "x"]));
        let tmp = std::env::temp_dir().join("cargo-lbin-test-store-refusal");
        let _ = std::fs::remove_dir_all(&tmp);
        let err = m.store(&tmp.join("prefix")).unwrap_err().to_string();
        assert!(err.contains("refusing to store"), "{err}");
        assert!(
            !Manifest::path(&tmp.join("prefix")).exists(),
            "nothing may be written on refusal"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ownership_is_a_function() {
        let mut m = Manifest::default();
        m.crates.insert("foo".to_owned(), entry(&["a", "b"]));
        m.crates.insert("bar".to_owned(), entry(&["c"]));
        assert!(m.validate().is_ok());

        // Cross-crate duplicate: error names both owners.
        m.crates.insert("baz".to_owned(), entry(&["a"]));
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("owned by both"), "{err}");
        assert!(err.contains("foo") && err.contains("baz"), "{err}");

        // Intra-entry duplicate: distinct message. {:#} prints the whole
        // chain, which is what main() shows.
        let mut m = Manifest::default();
        m.crates.insert("foo".to_owned(), entry(&["x", "x"]));
        let err = format!("{:#}", m.validate().unwrap_err());
        assert!(err.contains("listed twice"), "{err}");
        assert!(err.contains("foo"), "{err}");
    }
}
