//! Persisted result of the last `checkupdate` — the one command that
//! queries the network; `list` annotates from it, nothing else
//! refreshes it. A cache in the strict sense: losing it costs one
//! `checkupdate`.
//!
//! A full snapshot, not just the outdated crates: a reader must tell
//! "checked and current" from "not checked at all", and both look
//! alike as absence from an outdated-only list.
//!
//! Keyed by prefix under the user's cache dir; the file records its
//! prefix, so a key collision degrades to "no report", never a wrong
//! one.

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

/// The identity of a prefix for cache purposes: a relative `--prefix`
/// is anchored in the cwd first — hashing bare `local` would hand one
/// directory's report to another. Lexical, not `canonicalize()`: the
/// prefix may not exist yet; two spellings of one tree cost a cache
/// miss, never a wrong hit.
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

/// FNV-1a written out: `DefaultHasher` is not stable across Rust
/// releases, and a cache key must not change with the toolchain.
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

    /// Same-dir temp + rename: a crash leaves the old report whole; the
    /// PID in the temp name keeps concurrent runs out of each other's
    /// file — the later rename wins, fine for a cache.
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

    /// What the report says about `name` as installed *now*: `None` when
    /// unchecked, or installed at a version this report cannot speak
    /// about — nothing is what the caller must show, not a stale
    /// checkmark.
    ///
    /// It can speak about two versions: the one it checked, and the
    /// update it named. Installing the update the report itself pointed
    /// at leaves the crate current *by this report*, which is why an
    /// update does not blank the list until the next check.
    pub fn status_for(&self, name: &str, current: &Version) -> Option<Status<'_>> {
        self.record_for(name, current).map(|(_, status)| status)
    }

    /// The record behind the status, for callers that need both.
    ///
    /// One implementation of the rule, because there are three surfaces
    /// asking it — the list, the interface and the JSON views — and a
    /// second copy is how two of them come to disagree about the same
    /// crate. The rule: a report can speak about the version it checked,
    /// and about the update it named. Installing the update the report
    /// itself pointed at leaves the crate current *by this report*,
    /// which is why an update does not blank the list until the next
    /// check. A release that arrived mid-build, a downgrade, or a crate
    /// it never saw are unknown, and say so.
    pub fn record_for(&self, name: &str, current: &Version) -> Option<(&Checked, Status<'_>)> {
        if let Some(checked) = self.checked_for(name, current) {
            let status = if checked.is_outdated() {
                Status::Outdated(&checked.latest)
            } else {
                Status::UpToDate
            };
            return Some((checked, status));
        }
        let named = self
            .crates
            .iter()
            .find(|c| c.name == name && c.is_outdated() && c.latest == *current)?;
        Some((named, Status::UpToDate))
    }

    /// The record for `name` as installed now, same exact-version rule as
    /// `status_for`: `latest` is not always `current` for an up-to-date
    /// crate (an installed version yanked since has a lower `latest`).
    pub fn checked_for(&self, name: &str, current: &Version) -> Option<&Checked> {
        self.crates
            .iter()
            .find(|c| c.name == name && c.current == *current)
    }
}

/// Coarse relative age; more precision would suggest freshness the
/// report does not have.
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

    /// A report speaks about two versions and no others: the one it
    /// checked, and the update it named. The second is what keeps the
    /// list from blanking the moment an update lands — and the limit is
    /// what keeps it from guessing about anything else.
    #[test]
    fn a_report_speaks_about_what_it_checked_and_what_it_named() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-report-status-for");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let report = Report::new(
            &prefix,
            vec![Checked {
                name: "foo".to_owned(),
                current: Version::parse("1.0.0").unwrap(),
                latest: Version::parse("2.0.0").unwrap(),
            }],
        )
        .unwrap();

        let at = |v: &str| report.status_for("foo", &Version::parse(v).unwrap());
        assert!(
            matches!(at("1.0.0"), Some(Status::Outdated(l)) if l.to_string() == "2.0.0"),
            "the version it checked"
        );
        assert!(
            matches!(at("2.0.0"), Some(Status::UpToDate)),
            "the update it named, once installed"
        );
        assert!(
            at("2.0.1").is_none(),
            "a release that arrived after the check is not this report's to describe"
        );
        assert!(at("0.9.0").is_none(), "nor a downgrade");
        assert!(
            report
                .status_for("bar", &Version::parse("1.0.0").unwrap())
                .is_none(),
            "nor a crate it never saw"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }
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
        // Updated since the check, to the very version this report
        // named: the report can answer that one, because it is its own
        // answer — as of `checked_at`, 0.26.1 was the newest there was.
        // Any other version it cannot, and does not.
        assert_eq!(
            report.status_for("bat", &v("0.26.1")),
            Some(Status::UpToDate)
        );
        assert_eq!(report.status_for("bat", &v("0.26.2")), None);
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
