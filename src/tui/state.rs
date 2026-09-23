// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The TUI's model and dialog state: rows and filters, confirmations
//! and pending intents. State owned by the event loop, with the row
//! and confirmation model shared with the renderer.

use super::PendingMigrate;
use crate::manifest::Manifest;
use crate::report::{Report, Status};
use semver::Version;
use std::path::PathBuf;

/// One installed crate as the list shows it.
#[derive(Clone)]
pub struct Row {
    pub name: String,
    pub version: String,
    pub bins: Vec<String>,
    pub locked: bool,
    pub pinned: bool,
    /// Cargo's recorded `rustc` for the installed artefact — carried
    /// so a frozen migration plan snapshots the whole entry identity.
    pub built_with_rustc: Option<String>,
    /// The other prefixes carrying this crate, as facts rather than as
    /// presentation: the list suffix renders them through the shared
    /// `prefixes::describe` the CLI listing uses, the details panel
    /// renders its own line under the same sanitize policy — and
    /// neither surface parses a string built for the other.
    pub also: Vec<crate::prefixes::AlsoIn>,
    pub status: RowStatus,
}

/// What the last recorded update check says about a row — three states, the
/// third silence, never a guess (see `report::Status`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowStatus {
    UpToDate,
    Outdated(Version),
    Unknown,
}

/// Which rows the list shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Updates,
    Pinned,
}

impl Filter {
    pub const ALL: [Filter; 3] = [Filter::All, Filter::Updates, Filter::Pinned];

    pub fn index(self) -> usize {
        match self {
            Filter::All => 0,
            Filter::Updates => 1,
            Filter::Pinned => 2,
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Filter::All => Filter::Updates,
            Filter::Updates => Filter::Pinned,
            Filter::Pinned => Filter::All,
        }
    }

    pub(super) fn prev(self) -> Self {
        match self {
            Filter::All => Filter::Pinned,
            Filter::Updates => Filter::All,
            Filter::Pinned => Filter::Updates,
        }
    }
}

/// Whether a row belongs to a view. Updates means what `update --all`
/// will act on: a pinned crate is held back, so its backlog lives in
/// the always-counted Pinned tab instead.
pub(super) fn admits(filter: Filter, row: &Row) -> bool {
    match filter {
        Filter::All => true,
        Filter::Updates => matches!(row.status, RowStatus::Outdated(_)) && !row.pinned,
        Filter::Pinned => row.pinned,
    }
}

/// A destructive action waiting for a `y`.
pub struct Confirm {
    pub prompt: String,
    pub(super) action: OnConfirm,
}

impl Confirm {
    /// Does the panel above this question belong to it?
    ///
    /// A `build_report` is sticky: a failed build's log can still be on
    /// screen when an unrelated question opens. Reading "report plus
    /// question" as "the question's plan" made every confirmation
    /// borrow the arrows and, on a no, throw away a panel it never
    /// owned. The question says which panel is its own.
    pub fn owns_report(&self) -> bool {
        self.action.owns_report()
    }

    /// The one funnel: the prompt is Span-bound, and both builders
    /// interpolate strings the TUI does not control.
    pub(super) fn new(prompt: &str, action: OnConfirm) -> Self {
        Self {
            prompt: crate::text::sanitize(prompt),
            action,
        }
    }
}

/// What a confirmed `y` triggers. A removal decides its shape only at
/// the `y`: privilege is a property of the world, checked fresh.
impl OnConfirm {
    /// The questions that pin a plan of their own above themselves.
    fn owns_report(&self) -> bool {
        matches!(self, Self::ReinstallAll { .. } | Self::UpdateAll { .. })
    }
}

pub(super) enum OnConfirm {
    Migrate {
        name: String,
        /// The row's version string, for the result line.
        version: String,
        dest: PathBuf,
        /// The plan, frozen at the keypress from the very row on screen; the
        /// worker receives *this* snapshot — a fresh one after the `y` could
        /// bless a version the person never confirmed.
        snap: crate::MigrationSnapshot,
    },
    /// `u`: the plan the person just read. What travels to the build is
    /// the version the entry had when the question was asked, checked
    /// under the lock before anything is built; the version the
    /// question showed was for reading.
    Update { name: String, expected: String },
    /// `T`: the snapshot the person just agreed to rebuild. The apply
    /// re-checks each member against it — see `tui_reinstall_sweep`.
    ReinstallAll {
        planned: Vec<(String, crate::ReinstallPlan)>,
    },
    /// `U`: the plan the person just read, member by member.
    UpdateAll { planned: Vec<crate::PlannedUpdate> },
    /// `x`: the need for privilege is decided fresh at the `y` —
    /// the world may move while the prompt is open.
    Remove { name: String },
    /// `migrate --all`: the whole plan frozen at the keypress, one
    /// snapshot per row, complete or not at all.
    MigrateAll {
        dest: PathBuf,
        plan: Vec<PendingMigrate>,
    },
}

/// Commands that take over the terminal, run by the event loop after
/// the announcing frame.
///
/// Not a way of starting work: every one of these is the fallback
/// taken when a privileged step cannot run noninteractively, which is
/// to say when `sudo` caches nothing. The CLI can ask there; a captured
/// worker cannot.
#[derive(Clone)]
pub(super) enum PendingAction {
    Update(String),
    /// `install --reinstall --all`: `T`'s fallback. The sweep normally
    /// plans, asks and builds in the panel; this is where it goes when
    /// the prefix's first privileged lock cannot be prepared quietly.
    ReinstallAll,
    /// `install --reinstall`: same fallback case as the downgrade below
    /// — where sudo caches nothing, the rebuild leaves as the command
    /// that reads the entry, not as a plain install that would resolve
    /// a fresh version.
    Reinstall(String),
    /// `downgrade`: only as the fallback when captured placement cannot
    /// run (sudo caches nothing here). The command asks for a version
    /// again, which is the price of the handover — and it keeps the
    /// premise check the panel path makes under its own lock.
    Downgrade(String),
    UpdateAll,
    Install {
        crates: Vec<String>,
        locked: bool,
    },
    Remove(String),
    /// `pin` / `unpin`: a manifest write, so privileged like the rest.
    SetPinned {
        name: String,
        pinned: bool,
    },
}

/// The interface's answer to a worker waiting at a privileged step.
///
/// A refusal carries its reason, because there are several and they are
/// not interchangeable: a password that did not authenticate, a sudo
/// that authenticated and retained nothing, a probe that failed. One
/// bit made them all look alike and left the worker inventing a
/// sentence for whichever it guessed.
pub(super) enum AuthAnswer {
    Authorized,
    Refused(String),
}

/// What `c` reached: a build with its own answer, or a batch between
/// members — where there is nothing to interrupt and nothing to
/// SIGKILL, only a plan that will not continue.
pub(super) enum ActiveCancel {
    Build(crate::CancelOutcome),
    BatchBetweenMembers,
}

/// Whatever is running now, asked the same way whoever is asking.
///
/// A job used to hold a `BuildControl` because a job was one build. A
/// batch is one operation over many builds, each with its own control,
/// so the job holds the thing that knows which — and `c`, Ctrl-C, the
/// grace-period escalation and the auth gate all ask through here
/// rather than each learning what kind of job this is. The dummy
/// control this replaces was the model leaking: three of those four
/// doors were reaching a control that had never run anything.
pub(super) enum ActiveControl {
    Single(std::sync::Arc<crate::BuildControl>),
    Batch(std::sync::Arc<crate::BatchControl>),
}

impl ActiveControl {
    pub(super) fn request_cancel(&self) -> ActiveCancel {
        match self {
            Self::Single(control) => ActiveCancel::Build(control.request_cancel()),
            Self::Batch(control) => match control.request_cancel() {
                crate::BatchCancel::Member(answer) => ActiveCancel::Build(answer),
                // Kept, not flattened into `Accepted`: there is no
                // process to interrupt and none to kill, and a message
                // promising SIGKILL would be promising something that
                // cannot happen.
                crate::BatchCancel::BetweenMembers => ActiveCancel::BatchBetweenMembers,
            },
        }
    }

    pub(super) fn cancelled(&self) -> bool {
        match self {
            Self::Single(control) => control.cancelled(),
            Self::Batch(control) => control.cancelled(),
        }
    }

    /// A batch outlives the member `c` was too late for.
    pub(super) fn is_batch(&self) -> bool {
        matches!(self, Self::Batch(_))
    }
}

/// An in-place mutation waiting for the run loop, because it may need
/// a password and only the run loop owns the terminal to ask for one.
/// Whether one is actually wanted depends on sudo's timestamp, which
/// is sudo's business.
///
/// These are not builds: they take a lock, change one line of the
/// manifest and a file or two, and are over. The handoff they replace
/// existed for sudo's prompt and nothing else — the work was never the
/// reason to leave the interface.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum PendingInPlace {
    Remove(String),
    SetPinned { name: String, pinned: bool },
}

/// What a reload found — a value every caller must face: since the
/// degraded state exists, `Ok` no longer means "the list is fresh".
/// A flag would let the next caller forget; `#[must_use]` makes
/// forgetting visible at the call site, where the next lie would be
/// written ("nothing installed", "now at ...", "checked: 0 updates").
#[must_use]
pub(super) enum ReloadOutcome {
    Loaded,
    Degraded,
}

/// Manifest entries joined with the report, consulted per installed
/// version: a crate changed since the check comes out `Unknown`, not
/// stale.
pub(super) fn rows_from(
    manifest: &Manifest,
    report: Option<&Report>,
    also: &std::collections::BTreeMap<String, Vec<crate::prefixes::AlsoIn>>,
) -> Vec<Row> {
    manifest
        .crates
        .iter()
        .map(|(name, entry)| {
            let status = Version::parse(&entry.version)
                .ok()
                .and_then(|current| report?.status_for(name, &current))
                .map_or(RowStatus::Unknown, |s| match s {
                    Status::UpToDate => RowStatus::UpToDate,
                    Status::Outdated(v) => RowStatus::Outdated(v.clone()),
                });
            Row {
                name: name.clone(),
                version: entry.version.clone(),
                bins: entry.bins.clone(),
                locked: entry.locked,
                pinned: entry.pinned,
                built_with_rustc: entry.built_with_rustc.clone(),
                also: also.get(name).cloned().unwrap_or_default(),
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Checked;
    use crate::test_support::manifest;
    use crate::test_support::v;
    use std::path::Path;

    #[test]
    fn rows_carry_three_way_status() {
        let m = manifest(&[
            ("bat", "0.26.0"),
            ("fd", "10.3.0"),
            ("ripgrep", "14.1.1"),
            ("sd", "1.2.0"),
        ]);
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
                // Checked against an older version, and updated since to
                // exactly the version this report named: the report can
                // answer that one.
                Checked {
                    name: "fd".to_owned(),
                    current: v("10.2.0"),
                    checked_at: None,
                    latest: v("10.3.0"),
                },
                // Checked against an older version and updated past it,
                // to something this report never saw.
                Checked {
                    name: "sd".to_owned(),
                    current: v("1.0.0"),
                    checked_at: None,
                    latest: v("1.1.0"),
                },
            ],
        )
        .unwrap();
        let rows = rows_from(&m, Some(&report), &std::collections::BTreeMap::new());
        let status: Vec<(&str, &RowStatus)> =
            rows.iter().map(|r| (r.name.as_str(), &r.status)).collect();
        assert_eq!(status[0], ("bat", &RowStatus::Outdated(v("0.26.1"))));
        // Installed at the version the report named: current, by this
        // report's own answer — which is what keeps a list from
        // blanking the moment `U` lands its updates.
        assert_eq!(status[1], ("fd", &RowStatus::UpToDate));
        assert_eq!(status[2], ("ripgrep", &RowStatus::UpToDate));
        // Installed past what the report saw: unknown, and says so.
        assert_eq!(status[3], ("sd", &RowStatus::Unknown));

        // No report at all: everything unknown, nothing claimed.
        let rows = rows_from(&m, None, &std::collections::BTreeMap::new());
        assert!(rows.iter().all(|r| r.status == RowStatus::Unknown));
    }

    #[test]
    fn filter_cycles_and_indexes() {
        assert_eq!(Filter::All.next(), Filter::Updates);
        assert_eq!(Filter::Updates.next(), Filter::Pinned);
        assert_eq!(Filter::Pinned.next(), Filter::All);
        for (i, f) in Filter::ALL.iter().enumerate() {
            assert_eq!(f.index(), i);
            // prev is next's inverse — BackTab retraces Tab exactly.
            assert_eq!(f.next().prev(), *f);
        }
    }

    #[test]
    fn view_membership_is_consistent() {
        let row = |pinned: bool, status: RowStatus| Row {
            name: "x".into(),
            version: "1.0.0".into(),
            bins: vec!["x".into()],
            locked: false,
            pinned,
            built_with_rustc: None,
            also: Vec::new(),
            status,
        };
        let newer = Version::new(2, 0, 0);
        let pinned_behind = row(true, RowStatus::Outdated(newer.clone()));
        let pinned_current = row(true, RowStatus::UpToDate);
        let outdated = row(false, RowStatus::Outdated(newer));
        let current = row(false, RowStatus::UpToDate);

        // Updates is what `update --all` will act on: a pinned crate is
        // held back, so its backlog must not be counted there.
        assert!(admits(Filter::Updates, &outdated));
        assert!(!admits(Filter::Updates, &pinned_behind));
        assert!(!admits(Filter::Updates, &current));

        // Pinned is a state, not a relation to a newer version: a pin on
        // the latest release belongs there just as much.
        assert!(admits(Filter::Pinned, &pinned_behind));
        assert!(admits(Filter::Pinned, &pinned_current));
        assert!(!admits(Filter::Pinned, &outdated));
        // The remaining two cells of the 4-state matrix, so the test
        // closes it rather than samples it.
        assert!(!admits(Filter::Updates, &pinned_current));
        assert!(!admits(Filter::Pinned, &current));

        for r in [&pinned_behind, &pinned_current, &outdated, &current] {
            assert!(admits(Filter::All, r));
        }
    }
}
