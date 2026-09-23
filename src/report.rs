// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persisted knowledge from update checks. Full sweeps (`checkupdate`
//! from the command line, `r` in the interface, `U`'s plan) re-stamp
//! the baseline; every partial question — `update NAME`, a bare
//! install resolving the newest release, `pinned --check` — absorbs
//! the facts it learned under their own timestamps. One sentence
//! covers both: whenever lbin asks the registry to determine update
//! status for a managed crate, that knowledge is recorded, whatever
//! the scope of the question. (`info` stays a read-only view of
//! crates.io and records nothing: browsing the registry is not a
//! freshness determination about a managed entry.) `list` and the
//! JSON views annotate from it and refresh nothing. A cache in
//! the strict sense: losing it costs one check.
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
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One crate's result: what was installed when the index was asked, and
/// the newest version the index offered for it under the same pre-release
/// rules `update` applies. `latest == current` means up to date.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
// The field deliberately mirrors `Report::checked_at`: same name, same
// meaning, one level down — that symmetry is worth more than the lint.
#[allow(clippy::struct_field_names)]
pub struct Checked {
    pub name: String,
    pub current: Version,
    pub latest: Version,
    /// Unix seconds when this one fact was learned — stamped the
    /// moment its answer arrived. `None` appears only in records
    /// written before this field existed and means "inherit
    /// `Report::checked_at`", the baseline of the full check that
    /// wrote them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Report {
    /// Unix seconds of the last full check — the baseline every
    /// `Checked` without its own stamp inherits. `None` for a report
    /// born from partial knowledge alone: no full check has happened,
    /// and the field refuses to invent one. Every cache written before
    /// this was a plain integer from a full check, so old files read
    /// as `Some`.
    pub checked_at: Option<u64>,
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

/// Serializes writers of one prefix's report. `record_knowledge`'s
/// read-modify-write would otherwise let a concurrent full sweep land
/// between its load and its store and be quietly reverted — the cache
/// stays a cache either way, but "last full check" must not travel
/// backwards. Held only across load → absorb → store; the network
/// queries that produced the facts happen before, outside it. Same
/// std file-locking as `StateLock`, on the cache's side of the fence.
pub struct CacheLock {
    _file: File,
}

/// Take the write lock for this prefix's report. Blocking without a
/// notice: the critical section is a file read and a file write, so a
/// wait here is milliseconds, not someone's build.
pub fn write_lock(cache: &Path, prefix: &Path) -> Result<CacheLock> {
    let identity = identity(prefix)?;
    let dir = cache.join("checkupdate");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.lock", key(&identity)));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            file.lock()
                .with_context(|| format!("locking {}", path.display()))?;
        }
        Err(TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
    }
    Ok(CacheLock { _file: file })
}

/// Unix seconds now; zero if the clock predates the epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Report {
    pub fn new(prefix: &Path, crates: Vec<Checked>) -> Result<Self> {
        Ok(Self {
            checked_at: Some(now_secs()),
            prefix: identity(prefix)?,
            crates,
        })
    }

    /// A report born from a partial check: no baseline — a partial
    /// check is not a full one, and the field will not pretend — with
    /// every fact carrying the moment it was learned, as absorbed
    /// facts always do. The shape a `pinned --check` displays and the
    /// shape `record_knowledge` grows are the same shape on purpose:
    /// `Some` in the report's `checked_at` means a real full check
    /// with no exception, temporary objects included.
    pub fn partial(prefix: &Path, learned: Vec<Checked>) -> Result<Self> {
        let mut report = Self {
            checked_at: None,
            prefix: identity(prefix)?,
            crates: Vec::new(),
        };
        report.absorb(learned);
        Ok(report)
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

    /// Fold freshly learned facts in, each under the stamp it already
    /// carries — set the moment its answer arrived, before any wait
    /// for the lock, because knowledge is ordered by the query, not by
    /// persistence, per fact and not per command. The baseline
    /// `checked_at` is untouched: it belongs to the last full check,
    /// and a partial one has no business re-dating knowledge it did
    /// not renew. This is what retires the old rule against saving
    /// partial snapshots — that rule guarded a single global
    /// timestamp, and the lie it prevented is now unrepresentable.
    ///
    /// Monotonic per crate, strictly: a fact replaces only knowledge
    /// older than itself. On a tie the stored fact stays — at
    /// one-second resolution a tie is undecidable, and the rule
    /// guarantees the one thing it can: a late writer never reverts
    /// existing knowledge.
    pub fn absorb(&mut self, learned: Vec<Checked>) {
        let baseline = self.checked_at;
        for fact in learned {
            let at = fact
                .checked_at
                .expect("a learned fact carries the moment of its query");
            match self.crates.iter_mut().find(|c| c.name == fact.name) {
                Some(slot) => {
                    if slot
                        .checked_at
                        .or(baseline)
                        .is_none_or(|stored| at > stored)
                    {
                        *slot = fact;
                    }
                }
                None => self.crates.push(fact),
            }
        }
    }

    /// Merge a completed full check with whatever landed while it ran,
    /// so the stored report is monotonic in both dimensions: the
    /// baseline never travels backwards (a newer full check that
    /// stored first stays the baseline), and a per-crate fact learned
    /// after this sweep's own moment survives it — a full check may
    /// only replace knowledge older than itself. Facts folded across
    /// reports have their stamps materialized first: `None` means
    /// "inherit *my* report's baseline", and under the other report's
    /// baseline it would quietly lie.
    fn merged_with(self, current: Option<Self>) -> Self {
        let Some(current) = current else {
            return self;
        };
        // On a baseline tie the stored report wins — the same
        // late-writer rule as `absorb`, one level up.
        let (mut base, other) = if current.checked_at >= self.checked_at {
            (current, self)
        } else {
            (self, current)
        };
        let other_baseline = other.checked_at;
        for mut fact in other.crates {
            fact.checked_at = fact.checked_at.or(other_baseline);
            match base.crates.iter_mut().find(|c| c.name == fact.name) {
                Some(slot) => {
                    if fact.checked_at > slot.checked_at.or(base.checked_at) {
                        *slot = fact;
                    }
                }
                None => base.crates.push(fact),
            }
        }
        base
    }

    /// Store a completed full check, reconciled under the cache lock
    /// with anything that landed while the sweep's queries ran: see
    /// `merged_with` for the two monotonicity guarantees. The caller
    /// holds no lock across the network; this is where out-of-order
    /// arrivals meet.
    pub fn store_full(&self, cache: &Path) -> Result<()> {
        let _lock = write_lock(cache, &self.prefix)?;
        let current = Self::load(cache, &self.prefix)?;
        self.clone().merged_with(current).store(cache)
    }

    /// When this record's fact was learned: its own stamp, or the
    /// report's baseline for records from a full check. `None` only
    /// for a baseline-less record in a baseline-less report, which
    /// `absorb` never produces.
    pub fn effective_checked_at(&self, checked: &Checked) -> Option<u64> {
        checked.checked_at.or(self.checked_at)
    }

    /// The footer's watermark: the age of the oldest knowledge behind
    /// the crates on display — meaningful only when it covers them
    /// all. Every displayed crate must have a record this report can
    /// speak about, by the same `record_for` rule the annotations use;
    /// one crate it cannot speak about voids the watermark, because
    /// "everything listed is backed by an answer no older than this"
    /// is then false at any age. An empty display asserts nothing and
    /// gets `None` too.
    pub fn knowledge_watermark<'a, I>(&self, entries: I) -> Option<Duration>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut oldest: Option<u64> = None;
        let mut any = false;
        for (name, version) in entries {
            any = true;
            let current = Version::parse(version).ok()?;
            let (checked, _) = self.record_for(name, &current)?;
            let at = self.effective_checked_at(checked)?;
            oldest = Some(oldest.map_or(at, |o| o.min(at)));
        }
        if !any {
            return None;
        }
        oldest.map(|at| Duration::from_secs(now_secs().saturating_sub(at)))
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

    /// The record for `name` checked at exactly this version — the
    /// strict question, deliberately narrower than `status_for`'s: that
    /// one also answers for the update this report named, this one only
    /// for what was actually checked. `latest` is not always `current`
    /// for an up-to-date crate (an installed version yanked since has a
    /// lower `latest`).
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

                checked_at: None,
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
                    checked_at: None,
                    latest: v("0.26.1"),
                },
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    checked_at: None,
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
        let stored_at = loaded
            .checked_at
            .expect("a stored full check has a baseline");
        assert!(now_secs().saturating_sub(stored_at) < 60);

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
                    checked_at: None,
                    latest: v("0.26.1"),
                },
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    checked_at: None,
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
    fn a_partial_report_never_claims_a_baseline() {
        let report = Report::partial(
            Path::new("/p"),
            vec![Checked {
                name: "bat".into(),
                current: v("0.26.0"),
                checked_at: Some(1_756_761_700),
                latest: v("0.26.1"),
            }],
        )
        .unwrap();
        assert_eq!(report.checked_at, None, "a partial check is not a full one");
        assert_eq!(report.crates[0].checked_at, Some(1_756_761_700));
    }

    #[test]
    fn absorb_is_monotonic_per_crate() {
        let mut report = Report {
            checked_at: Some(1000),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "bat".into(),
                current: v("0.26.1"),
                checked_at: Some(1500),
                latest: v("0.26.1"),
            }],
        };
        // An answer older than the knowledge on file arrives late and
        // changes nothing — ordered by the query, not by persistence.
        report.absorb(vec![Checked {
            name: "bat".into(),
            current: v("0.26.0"),
            checked_at: Some(1400),
            latest: v("0.26.0"),
        }]);
        let bat = &report.crates[0];
        assert_eq!(
            (bat.checked_at, bat.current.to_string().as_str()),
            (Some(1500), "0.26.1")
        );
        // A tie is undecidable at one-second resolution: the stored
        // fact stays, so a late writer never reverts knowledge.
        report.absorb(vec![Checked {
            name: "bat".into(),
            current: v("0.26.0"),
            checked_at: Some(1500),
            latest: v("0.26.0"),
        }]);
        assert_eq!(report.crates[0].current, v("0.26.1"), "stored wins the tie");
        // Strictly newer knowledge replaces baseline knowledge.
        let mut swept = Report {
            checked_at: Some(1000),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "fd".into(),
                current: v("10.2.0"),
                checked_at: None,
                latest: v("10.2.0"),
            }],
        };
        swept.absorb(vec![Checked {
            name: "fd".into(),
            current: v("10.3.0"),
            checked_at: Some(1001),
            latest: v("10.3.0"),
        }]);
        assert_eq!(swept.crates[0].checked_at, Some(1001));
        assert_eq!(swept.crates[0].current, v("10.3.0"));
    }

    #[test]
    fn a_full_check_never_travels_backwards() {
        let older_sweep = Report {
            checked_at: Some(100),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "bat".into(),
                current: v("0.25.0"),
                checked_at: None,
                latest: v("0.25.0"),
            }],
        };
        let newer_current = Report {
            checked_at: Some(101),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "bat".into(),
                current: v("0.26.0"),
                checked_at: None,
                latest: v("0.26.1"),
            }],
        };
        let merged = older_sweep.merged_with(Some(newer_current));
        assert_eq!(
            merged.checked_at,
            Some(101),
            "the baseline never travels backwards"
        );
        // Equal baselines: the stored report wins the tie, the same
        // late-writer rule one level up.
        let incoming = Report {
            checked_at: Some(100),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "tied".into(),
                current: v("0.25.0"),
                checked_at: None,
                latest: v("0.25.0"),
            }],
        };
        let stored = Report {
            checked_at: Some(100),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "tied".into(),
                current: v("0.25.0"),
                checked_at: None,
                latest: v("0.25.1"),
            }],
        };
        let tied = incoming.merged_with(Some(stored));
        assert_eq!(
            tied.crates
                .iter()
                .find(|c| c.name == "tied")
                .unwrap()
                .latest,
            v("0.25.1"),
            "stored wins the tie"
        );
        assert_eq!(
            merged.crates[0].latest,
            v("0.26.1"),
            "newer knowledge survives an older writer"
        );
    }

    #[test]
    fn a_full_check_keeps_fresher_partial_facts() {
        let sweep = Report {
            checked_at: Some(100),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "foo".into(),
                current: v("1.0.0"),
                checked_at: None,
                latest: v("1.0.0"),
            }],
        };
        let with_partial = Report {
            checked_at: Some(90),
            prefix: PathBuf::from("/p"),
            crates: vec![Checked {
                name: "foo".into(),
                current: v("1.0.0"),
                checked_at: Some(101),
                latest: v("1.1.0"),
            }],
        };
        let merged = sweep.merged_with(Some(with_partial));
        assert_eq!(merged.checked_at, Some(100), "the sweep's baseline stands");
        let foo = &merged.crates[0];
        assert_eq!(
            (foo.checked_at, foo.latest.to_string().as_str()),
            (Some(101), "1.1.0"),
            "a fact learned after the sweep's moment survives it, stamp materialized"
        );
    }

    #[test]
    fn the_watermark_needs_every_displayed_crate_covered() {
        let now = now_secs();
        let report = Report {
            checked_at: Some(now - 1000),
            prefix: PathBuf::from("/p"),
            crates: vec![
                Checked {
                    name: "bat".into(),
                    current: v("0.26.0"),
                    checked_at: None,
                    latest: v("0.26.1"),
                },
                Checked {
                    name: "fd".into(),
                    current: v("10.3.0"),
                    checked_at: Some(now - 10),
                    latest: v("10.3.0"),
                },
            ],
        };
        // Fresh fact alone: its own stamp is the watermark.
        assert!(
            report
                .knowledge_watermark([("fd", "10.3.0")])
                .unwrap()
                .as_secs()
                < 100
        );
        // Baseline knowledge joins the display: the older answer wins.
        assert!(
            report
                .knowledge_watermark([("bat", "0.26.0"), ("fd", "10.3.0")])
                .unwrap()
                .as_secs()
                >= 1000
        );
        // The update the report itself named still counts as covered —
        // the same `record_for` rule the annotations use.
        assert!(report.knowledge_watermark([("bat", "0.26.1")]).is_some());
        // A crate the report cannot speak about voids the watermark:
        // moved past what it checked...
        assert_eq!(report.knowledge_watermark([("bat", "9.9.9")]), None);
        // ...or never covered at all. "Everything listed is backed" is
        // then false at any age.
        assert_eq!(
            report.knowledge_watermark([("fd", "10.3.0"), ("ghost", "1.0.0")]),
            None
        );
        // An empty display asserts nothing.
        assert_eq!(report.knowledge_watermark(std::iter::empty()), None);
    }

    #[test]
    fn absorb_stamps_facts_without_redating_the_baseline() {
        let mut report = Report::new(
            Path::new("/p"),
            vec![Checked {
                name: "bat".into(),
                current: v("0.26.0"),
                checked_at: None,
                latest: v("0.26.1"),
            }],
        )
        .unwrap();
        let baseline = report.checked_at.expect("Report::new stamps a baseline");
        report.absorb(vec![Checked {
            name: "bat".into(),
            current: v("0.26.1"),
            checked_at: Some(baseline + 100),
            latest: v("0.26.1"),
        }]);
        report.absorb(vec![Checked {
            name: "fd".into(),
            current: v("10.3.0"),
            checked_at: Some(baseline + 200),
            latest: v("10.3.0"),
        }]);
        assert_eq!(
            report.checked_at,
            Some(baseline),
            "a partial check re-dates nothing it did not renew"
        );
        let bat = report.crates.iter().find(|c| c.name == "bat").unwrap();
        assert_eq!(
            bat.current,
            v("0.26.1"),
            "the fact is replaced, not duplicated"
        );
        assert_eq!(bat.checked_at, Some(baseline + 100));
        let fd = report.crates.iter().find(|c| c.name == "fd").unwrap();
        assert_eq!(
            fd.checked_at,
            Some(baseline + 200),
            "a new fact joins under its own stamp"
        );
    }

    #[test]
    fn a_report_from_before_the_option_reads_its_baseline() {
        let raw = r#"{"checked_at":123,"prefix":"/usr/local","crates":[{"name":"bat","current":"0.26.0","latest":"0.26.1"}]}"#;
        let report: Report = serde_json::from_str(raw).unwrap();
        assert_eq!(
            report.checked_at,
            Some(123),
            "u64 -> Option<u64> reads old caches as Some(old_value)"
        );
        assert_eq!(report.crates[0].checked_at, None);
    }

    #[test]
    fn a_record_from_before_the_field_reads_as_baseline() {
        let raw = r#"{"name":"bat","current":"0.26.0","latest":"0.26.1"}"#;
        let checked: Checked = serde_json::from_str(raw).unwrap();
        assert_eq!(
            checked.checked_at, None,
            "old caches load; None means the report's baseline"
        );
    }

    #[test]
    fn age_description_is_coarse() {
        assert_eq!(describe_age(Duration::from_secs(5)), "just now");
        assert_eq!(describe_age(Duration::from_secs(90)), "1m ago");
        assert_eq!(describe_age(Duration::from_secs(7200)), "2h ago");
        assert_eq!(describe_age(Duration::from_secs(3 * 86_400 + 5)), "3d ago");
    }
}
