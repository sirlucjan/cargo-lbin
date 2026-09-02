//! Persisted result of the last `checkupdate`.
//!
//! cargo-lbin never queries the network on its own; `checkupdate` is the one
//! read-only command that does, and this file is how its answer survives
//! until the user asks again. `list` annotates from it, and nothing else
//! ever refreshes it. It is a cache in the strict sense: losing it costs
//! one `checkupdate`, nothing more.
//!
//! The report is a full snapshot — every crate that was checked, with the
//! version it was checked against and the newest version found — not just
//! the outdated ones. A reader must be able to tell "checked and current"
//! from "not checked at all" (installed after the check, or the manifest
//! changed); both would look alike as a mere absence from an outdated
//! list, and a TUI drawing a checkmark for the second case would be lying.
//!
//! Lives under the user's cache directory, keyed by prefix: the same user
//! may manage `/usr/local` and `~/.local` side by side, and one prefix's
//! answer must never be shown for the other. The file records the prefix
//! it belongs to, so a key collision degrades to "no report" rather than a
//! wrong one.

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One crate's result: what was installed when the index was asked, and
/// the newest version the index offered for it under the same pre-release
/// rules `update` applies. `latest == current` means up to date.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub name: String,
    pub current: Version,
    pub latest: Version,
}

impl Checked {
    pub fn is_outdated(&self) -> bool {
        self.latest > self.current
    }
}

/// What the report knows about one installed crate.
#[derive(Debug, PartialEq, Eq)]
pub enum Status<'a> {
    UpToDate,
    Outdated(&'a Version),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Report {
    /// Unix seconds at the time of the check.
    pub checked_at: u64,
    /// The prefix the check was run against, as `identity` renders it.
    pub prefix: PathBuf,
    /// Every crate the check covered, outdated or not.
    pub crates: Vec<Checked>,
}

/// The identity of a prefix for cache purposes.
///
/// A relative `--prefix` is anchored in the current directory first:
/// `--prefix local` run from two different directories names two different
/// trees, and hashing the bare `local` would hand the second one the
/// first one's report. Anchoring is lexical, not `canonicalize()` — the
/// prefix may not exist yet, and symlink semantics are not this cache's
/// business. Two spellings of one tree (via `..` or a symlink) therefore
/// get two keys, which costs a cache miss and never a wrong hit.
/// Trailing slashes and `.` components are dropped so they do not split
/// one prefix into several keys.
pub fn identity(prefix: &Path) -> Result<PathBuf> {
    let anchored = if prefix.is_absolute() {
        prefix.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving relative prefix against the current directory")?
            .join(prefix)
    };
    Ok(anchored.components().collect())
}

/// FNV-1a over the prefix identity. Written out rather than taken from
/// `DefaultHasher`, whose output is explicitly not stable across Rust
/// releases — a cache key must not change with the toolchain.
fn key(identity: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in identity.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

impl Report {
    pub fn new(prefix: &Path, crates: Vec<Checked>) -> Result<Self> {
        let checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Ok(Self {
            checked_at,
            prefix: identity(prefix)?,
            crates,
        })
    }

    pub fn path(cache: &Path, prefix: &Path) -> Result<PathBuf> {
        let identity = identity(prefix)?;
        Ok(cache
            .join("checkupdate")
            .join(format!("{}.json", key(&identity))))
    }

    /// `Ok(None)` when no report exists for this prefix; `Err` only for a
    /// report that exists and cannot be read. Callers that merely annotate
    /// (`list`) should warn and carry on, never fail the listing over it.
    pub fn load(cache: &Path, prefix: &Path) -> Result<Option<Self>> {
        let identity = identity(prefix)?;
        let path = Self::path(cache, &identity)?;
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let report: Self = serde_json::from_str(&raw)
            .with_context(|| format!("corrupt update report at {}", path.display()))?;
        if report.prefix != identity {
            // Key collision with another prefix: treat as absent.
            return Ok(None);
        }
        Ok(Some(report))
    }

    /// Same-directory temp + rename, so a crash mid-write leaves the old
    /// report whole. The temp name carries the PID: two concurrent
    /// `checkupdate` runs against one prefix must not write into each
    /// other's temp file — the rename then simply lets the later one win,
    /// which is fine for a cache. No privilege is ever involved: the cache
    /// directory is the user's own.
    pub fn store(&self, cache: &Path) -> Result<()> {
        // `self.prefix` is already an identity (absolute), so re-deriving
        // it is a no-op rather than a second anchoring.
        let path = Self::path(cache, &self.prefix)?;
        let dir = path
            .parent()
            .context("report path has no parent directory")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!(".{}.{}.tmp", key(&self.prefix), std::process::id()));
        let mut raw = serde_json::to_string_pretty(self)?;
        raw.push('\n');
        fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("placing {}", path.display()))
            .inspect_err(|_| {
                let _ = fs::remove_file(&tmp);
            })?;
        Ok(())
    }

    /// Time since the check; zero if the clock has since moved backwards.
    pub fn age(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Duration::from_secs(now.saturating_sub(self.checked_at))
    }

    /// What the report says about `name` as installed *now*: `None` when it
    /// was not checked at all, or was checked against a different version
    /// (installed or updated since). A report taken before an update says
    /// nothing about the version that replaced it — and nothing is what
    /// the caller must show, not a stale checkmark.
    pub fn status_for(&self, name: &str, current: &Version) -> Option<Status<'_>> {
        let checked = self.checked_for(name, current)?;
        Some(if checked.is_outdated() {
            Status::Outdated(&checked.latest)
        } else {
            Status::UpToDate
        })
    }

    /// The record for `name` as installed *now*, with the same "checked
    /// against this exact version" rule as `status_for`. For callers that
    /// need what the check found even when nothing is newer: `latest` is
    /// not always `current` for an up-to-date crate — an installed version
    /// yanked since has a lower `latest`, and reporting `current` in its
    /// place would misstate what the check saw.
    pub fn checked_for(&self, name: &str, current: &Version) -> Option<&Checked> {
        self.crates
            .iter()
            .find(|c| c.name == name && c.current == *current)
    }
}

/// Coarse relative age for a status line. Precision beyond this would
/// suggest a freshness the report does not have.
pub fn describe_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn id(p: &str) -> PathBuf {
        identity(Path::new(p)).unwrap()
    }

    #[test]
    fn identity_ignores_trailing_slash_and_dot_components() {
        assert_eq!(id("/usr/local"), id("/usr/local/"));
        assert_eq!(id("/usr/local"), id("/usr/./local"));
        assert_ne!(id("/usr/local"), id("/usr"));
        // Lexical only: `..` is not resolved, so this is a different key
        // (a miss, never a wrong hit).
        assert_ne!(id("/usr/local"), id("/usr/lib/../local"));
    }

    #[test]
    fn relative_prefix_is_anchored_in_cwd() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(id("local"), cwd.join("local"));
        assert_eq!(id("./local/"), cwd.join("local"));
        // The bare name must never be what gets hashed.
        assert_ne!(key(&id("local")), key(Path::new("local")));
    }

    #[test]
    fn key_is_stable() {
        // Pinned so a toolchain bump can never silently orphan every
        // existing report file.
        assert_eq!(key(&id("/usr/local")), "f7ab513049b9491c");
    }

    #[test]
    fn round_trip_and_prefix_isolation() {
        let tmp = std::env::temp_dir().join("cargo-lbin-test-report");
        let _ = fs::remove_dir_all(&tmp);
        let prefix = Path::new("/usr/local/");
        let report = Report::new(
            prefix,
            vec![
                Checked {
                    name: "bat".to_owned(),
                    current: v("0.26.0"),
                    latest: v("0.26.1"),
                },
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    latest: v("14.1.1"),
                },
            ],
        );
        let report = report.unwrap();
        report.store(&tmp).unwrap();
        // No temp file survives a successful store.
        let leftovers: Vec<_> = fs::read_dir(tmp.join("checkupdate"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert_eq!(leftovers, Vec::<std::ffi::OsString>::new());

        let loaded = Report::load(&tmp, Path::new("/usr/local"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.crates, report.crates);
        assert_eq!(loaded.prefix, Path::new("/usr/local"));
        assert!(loaded.age() < Duration::from_secs(60));

        // Another prefix has no report.
        assert!(Report::load(&tmp, Path::new("/opt")).unwrap().is_none());
        // Corrupt file is an error, not silently absent.
        fs::write(Report::path(&tmp, prefix).unwrap(), b"{").unwrap();
        assert!(Report::load(&tmp, prefix).is_err());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn status_distinguishes_current_from_unchecked() {
        let report = Report::new(
            Path::new("/p"),
            vec![
                Checked {
                    name: "bat".to_owned(),
                    current: v("0.26.0"),
                    latest: v("0.26.1"),
                },
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    latest: v("14.1.1"),
                },
            ],
        );
        let report = report.unwrap();
        assert_eq!(
            report.status_for("bat", &v("0.26.0")),
            Some(Status::Outdated(&v("0.26.1")))
        );
        // Checked and current: a positive answer, not an absence.
        assert_eq!(
            report.status_for("ripgrep", &v("14.1.1")),
            Some(Status::UpToDate)
        );
        // Updated since the check: the report is stale for it — unknown.
        assert_eq!(report.status_for("bat", &v("0.26.1")), None);
        // Installed after the check: never covered — unknown.
        assert_eq!(report.status_for("fd", &v("1.0.0")), None);
    }

    #[test]
    fn age_description_is_coarse() {
        assert_eq!(describe_age(Duration::from_secs(5)), "just now");
        assert_eq!(describe_age(Duration::from_secs(90)), "1m ago");
        assert_eq!(describe_age(Duration::from_secs(7200)), "2h ago");
        assert_eq!(describe_age(Duration::from_secs(3 * 86_400 + 5)), "3d ago");
    }
}
