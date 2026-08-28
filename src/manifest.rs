//! Installation manifest.
//!
//! Lives at `<prefix>/share/cargo-lbin/manifest.json` so state travels with the
//! system, not with the user. Written via a write-sealed memfd handed to
//! privileged `install` and placed atomically (same-directory temp +
//! rename), so the write path is identical with and without privilege
//! escalation.

use anyhow::{bail, Context, Result};
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
    /// Whether the crate was built with `--locked`; reused on update.
    #[serde(default)]
    pub locked: bool,
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
        let manifest: Self = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("corrupt manifest at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        manifest
            .validate()
            .with_context(|| format!("invalid manifest at {}", path.display()))?;
        Ok(manifest)
    }

    /// The manifest steers file operations that may run under sudo, so it is
    /// treated as untrusted input: every crate name must be a safe crates.io
    /// identifier, every bin exactly one plain filename, every version valid
    /// semver, and every bin owned by exactly one crate — `check_collisions`
    /// and `remove` both assume ownership is a function, so the manifest must
    /// guarantee it. Validated once here; the rest of the code relies on it.
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
            semver::Version::parse(&entry.version)
                .with_context(|| format!("crate `{name}` has invalid version `{}`", entry.version))?;
        }
        Ok(())
    }

    /// Serialize into a sealed anonymous memfd and hand root the fd path:
    /// no temp file exists for a leftover build process to find, and the
    /// seals guarantee the bytes root copies are the bytes serialized here.
    /// Placement is atomic (`install_atomic`): a crash mid-write leaves the
    /// old manifest whole, never half of the new one.
    pub fn store(&self, prefix: &Path) -> Result<()> {
        // Symmetry with load(): cargo-lbin never knowingly writes state it would
        // later refuse to read back. Upstream checks should make this
        // unreachable; it exists as the last line of defense.
        self.validate().context("refusing to store invalid manifest")?;
        let mut raw = serde_json::to_string_pretty(self)?;
        raw.push('\n');
        let sealed = privileged::SealedSource::from_bytes(raw.as_bytes())?;
        let policy = privileged::Escalation::for_prefix(prefix);
        privileged::install_sealed(policy, &sealed, &Self::path(prefix), "644")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(bins: &[&str]) -> Entry {
        Entry {
            version: "1.0.0".to_owned(),
            bins: bins.iter().map(|s| (*s).to_owned()).collect(),
            locked: false,
        }
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

        // Intra-entry duplicate: distinct message. Note {:#}: anyhow's
        // Display shows only the outermost context ("crate `foo`"), while
        // the alternate form prints the whole chain — which is also what
        // main() shows the user.
        let mut m = Manifest::default();
        m.crates.insert("foo".to_owned(), entry(&["x", "x"]));
        let err = format!("{:#}", m.validate().unwrap_err());
        assert!(err.contains("listed twice"), "{err}");
        assert!(err.contains("foo"), "{err}");
    }
}
