//! The TUI's worker boundary: messages sent back from spawned work,
//! the UI-side `Job` state that owns their receivers, and pending build
//! intents waiting to cross that boundary. Polling and the collector -
//! the last cancel boundary - stay with `App` in mod.rs.

use super::batch::MigrateTarget;
use super::state::{ActiveControl, AuthAnswer};
use crate::api;
use crate::report::Checked;
use anyhow::Result;
use semver::Version;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

/// The render boundary in one function: every pipeline line becomes a
/// `BuildMsg` here — sanitized, because past this point it is
/// Span-bound and paths may hold ESC. A future producer cannot route
/// around it.
pub(super) fn build_msg(kind: crate::LineKind, line: &str) -> BuildMsg {
    let line = crate::text::sanitize(line);
    match kind {
        crate::LineKind::Cargo => BuildMsg::Cargo(line),
        crate::LineKind::Notice => BuildMsg::Notice(line),
        crate::LineKind::Warning => BuildMsg::Warning(line),
    }
}

/// Messages a build worker streams to the UI thread; the protocol
/// mirrors the pipeline's classification.
pub(super) enum BuildMsg {
    /// cargo's own output: parsed for the gauge, kept for the tail.
    Cargo(String),
    /// The pipeline narrating itself; promoted to the live status so
    /// "waiting for the state lock" beats a gauge frozen at zero.
    Notice(String),
    /// Kept past success: a shadowed binary does not stop being shadowed
    /// because the install succeeded.
    Warning(String),
    /// A privileged step wants sudo revalidated on a real terminal — for
    /// the named prefix: one worker can escalate for both prefixes of a
    /// migration, and the prompt must name the one asking. The worker
    /// blocks until the run loop answers.
    NeedAuth {
        target: PathBuf,
        purpose: crate::privileged::AuthPurpose,
    },
    /// A batch worker moving to the next member: the interface's cue to
    /// say `[2/3] bar` and to file what follows under that name.
    MemberStarted { name: String },
    /// A batch member is done, classified by the worker where its error
    /// was still typed.
    MemberDone {
        name: String,
        outcome: crate::MemberOutcome,
    },
    /// Finished, classified by the worker where the error is at hand: the
    /// UI must not guess "cancelled" from a phase flag a late `c` can set
    /// after cargo died of its own causes.
    Done(BuildOutcome),
}

/// How a build ended. `Cancelled` is a real pipeline outcome (the
/// typed marker), not a UI interpretation. `CompletedWithWarning` is
/// deliberately generic: the payload owns the specifics. `Migrated`
/// carries the version the destination actually committed — the frozen
/// plan's version is what was confirmed, not necessarily what was
/// installed, and the note must speak the result.
pub(super) enum BuildOutcome {
    Success,
    Migrated(Version),
    Cancelled,
    CompletedWithWarning(String),
    Failed(anyhow::Error),
}

/// What the job builds toward; the UI composes result lines from
/// this — the worker reports outcomes, never prose.
pub(super) enum BuildKind {
    Install,
    /// The newest release, started by `u` against a plan the person
    /// saw.
    Update,
    /// A rebuild of the entry as it stands, started by `t`. Same
    /// pipeline as an install, different word — and a record that
    /// called it an install would describe a version choice nobody
    /// made.
    Reinstall,
    /// An install of an exact older version, started from the panel's
    /// offer. It differs from `Install` only in what the record calls
    /// it — the pipeline underneath is the same — but a record that
    /// calls a downgrade an install describes an operation the person
    /// did not perform.
    Downgrade,
    /// Boxed: the variant is already the enum's largest, and the target
    /// rides in every build job.
    Migrate(Box<MigrateTarget>),
}

impl BuildKind {
    /// The word the UI speaks for this job — one definition, so a live
    /// panel and the finished report cannot name the same build
    /// differently.
    pub(super) fn verb(&self) -> &'static str {
        match self {
            BuildKind::Install => "install",
            BuildKind::Downgrade => "downgrade",
            BuildKind::Reinstall => "reinstall",
            BuildKind::Update => "update",
            // Tuple variant, tuple pattern: a pattern should not lie
            // about the shape.
            BuildKind::Migrate(_) => "migrate",
        }
    }
}

/// A one-shot's whole cancel model: a shared flag. Running, cancel
/// requested (flag set, slot still held until the worker returns —
/// single-flight stands), finished or cancelled. No grace, no
/// escalation, no "too late": those belong to builds, which have a
/// child process and a placement door — a one-shot has neither.
pub(super) type CancelFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

pub(super) fn cancel_flag() -> CancelFlag {
    CancelFlag::default()
}

/// Background work in flight — at most one, so the busy label is
/// unambiguous.
pub(super) enum Job {
    Check {
        rx: Receiver<Result<Option<Vec<Checked>>>>,
        cancel: CancelFlag,
    },
    /// The read-only audit on a worker thread (slow storage; the UI stays
    /// responsive on principle), in the build's framed panel; findings
    /// land in the sticky report — the CLI's severity split.
    Verify {
        rx: Receiver<Result<crate::VerifyReport>>,
        started: std::time::Instant,
        cancel: CancelFlag,
    },
    Search {
        query: String,
        rx: Receiver<Result<Vec<api::Hit>>>,
        cancel: CancelFlag,
    },
    /// `U`'s check: the registry asked about every entry here, pinned
    /// included — it is an update check and leaves one behind, which is
    /// why it is named for what it does rather than for the plan it
    /// happens to enable. The longest thing this tool does without
    /// building, and cancellable down to the individual request.
    UpdateSweepCheck {
        rx: Receiver<Result<Option<Vec<Checked>>>>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    /// `T`'s plan: every managed entry, frozen. No network — the sweep
    /// asks the registry nothing, which is the whole point of it.
    ReinstallPlan {
        rx: Receiver<Result<Vec<(String, crate::ReinstallPlan)>>>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    /// `u`'s lookup: what the registry says now, for one crate. The
    /// interface's own row is the last recorded update check, and an
    /// update is worth planning against the registry rather than
    /// against whatever was true an hour ago.
    UpdatePlan {
        name: String,
        rx: Receiver<Result<crate::UpdatePlan>>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    /// The candidate lookup behind `D`: a read-only question to the
    /// index, cancellable like every other one-shot. The choice it
    /// feeds is a value for an action already chosen, not a browser.
    Downgrade {
        name: String,
        current: String,
        rx: Receiver<Result<Vec<Version>>>,
        cancel: CancelFlag,
    },
    /// A captured build streaming over `rx`: an install, an update, a
    /// reinstall, a downgrade, a migration, or one member of a batch or
    /// sweep. What it is building is in `kind`.
    Build {
        name: String,
        rx: Receiver<BuildMsg>,
        /// The run loop's answer to `BuildMsg::NeedAuth`.
        auth_tx: Sender<AuthAnswer>,
        /// Units started, not finished: cargo announces a unit when it
        /// begins.
        units_started: usize,
        /// The unit last announced by cargo.
        current: Option<String>,
        /// Rolling tail for the failure panel.
        tail: VecDeque<String>,
        /// The last pipeline notice, shown as the live status until
        /// cargo speaks again.
        status_note: Option<String>,
        /// Warnings; shown past a success, dropped on failure — a rollback
        /// removed the binaries they described.
        warnings: Vec<String>,
        /// When the worker spawned; lock waiting counts on purpose — the
        /// clock answers "how long has this operation been running".
        started: std::time::Instant,
        /// Set by `poll_job` on a revalidation request — carrying the prefix
        /// the escalation is for; answered by the run loop.
        needs_auth: Option<(PathBuf, crate::privileged::AuthPurpose)>,
        /// Whatever is running: one build's cancel state machine, or a
        /// batch's, which knows which member holds it now.
        control: ActiveControl,
        /// What is being built toward.
        kind: BuildKind,
        /// When an accepted cancel's grace runs out; one-shot. The run loop
        /// escalates because the worker cannot be trusted to reach its own
        /// sweep — a member holding inherited stderr can wedge it inside
        /// `read_until`.
        cancel_deadline: Option<std::time::Instant>,
    },
}

impl Job {
    pub(super) fn label(&self) -> String {
        match self {
            Job::Check { cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    "cancelling the update check…".to_owned()
                } else {
                    "checking crates.io for updates…".to_owned()
                }
            }
            Job::Verify { cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    // "requested", not "cancelling": the audit is not
                    // interrupted — it finishes and its report is
                    // discarded. The label must not suggest a power the
                    // door does not have.
                    "verify: cancel requested…".to_owned()
                } else {
                    "verifying the prefix…".to_owned()
                }
            }
            Job::Search { query, cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    format!("search `{query}`: cancel requested…")
                } else {
                    format!("searching crates.io for `{query}`…")
                }
            }
            Job::UpdateSweepCheck { cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    "update check: cancel requested…".to_owned()
                } else {
                    "checking crates.io for updates…".to_owned()
                }
            }
            Job::ReinstallPlan { cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    "reading the prefix: cancel requested…".to_owned()
                } else {
                    "reading what is installed here…".to_owned()
                }
            }
            Job::UpdatePlan { name, cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    format!("version lookup for `{name}`: cancel requested…")
                } else {
                    format!("checking crates.io for an update to `{name}`…")
                }
            }
            Job::Downgrade { name, cancel, .. } => {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    format!("version lookup for `{name}`: cancel requested…")
                } else {
                    format!("asking crates.io which versions precede `{name}`…")
                }
            }
            Job::Build { name, .. } => format!("building {name}…"),
        }
    }
}

/// The authorization door both workers hand to the pipeline —
/// placement and retirement alike.
///
/// Three answers, and each is a policy: a cancelled build refuses
/// rather than asking for a password it no longer needs; fresh
/// credentials pass silently, which is the common case when sudo's
/// timestamp is still warm from earlier work; otherwise the interface
/// is asked to collect them — this is where a placement's password is
/// normally collected — and the worker waits for its verdict. A denial that answers a cancel *is*
/// the cancel — a refusal keeps its own name only when it is one.
pub(super) fn auth_gate(
    control: &crate::BuildControl,
    tx: &Sender<BuildMsg>,
    auth_rx: &Receiver<AuthAnswer>,
    escalating: &Path,
    purpose: crate::privileged::AuthPurpose,
) -> Result<()> {
    if control.cancelled() {
        return Err(anyhow::Error::new(crate::BuildCancelled));
    }
    match crate::privileged::credentials_fresh() {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = tx.send(BuildMsg::NeedAuth {
                target: escalating.to_path_buf(),
                purpose,
            });
            match auth_rx.recv() {
                Ok(AuthAnswer::Authorized) => Ok(()),
                Ok(AuthAnswer::Refused(_)) if control.cancelled() => {
                    Err(anyhow::Error::new(crate::BuildCancelled))
                }
                // Typed, and carrying both what was being authorized and
                // why it was not: a batch tells a refused placement from
                // a failed build, and a migration's retirement must not
                // be told it failed to place anything.
                Ok(AuthAnswer::Refused(reason)) => {
                    Err(anyhow::Error::new(crate::AuthorizationRefused {
                        purpose,
                        reason,
                    }))
                }
                Err(_) => anyhow::bail!("the interface went away mid-authorization"),
            }
        }
        Err(e) => Err(e),
    }
}

/// A build the run loop still has to start, because starting one needs
/// the terminal: the escalation preflight may have to ask for a
/// password. `intent` says which build this is — see [`BuildIntent`] —
/// and travels with the request to the worker.
pub(super) struct PendingBuild {
    pub(super) spec: String,
    pub(super) locked: bool,
    pub(super) intent: BuildIntent,
}

/// What the queued build is, which decides both the worker it reaches
/// and the word the record uses.
///
/// They differ only in where the version and the pin come from: an
/// install takes both from the line the person typed, a downgrade
/// takes the version from a keypress against a premise, a reinstall
/// takes everything from the manifest entry and changes none of it,
/// and an update carries the observed version only as a premise and
/// asks for the newest again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum BuildIntent {
    Install,
    /// The version the offer was computed against, re-checked under the
    /// worker's lock.
    Downgrade(String),
    Reinstall,
    /// The half of `u`'s plan that governs: what the entry said when the
    /// question was asked. Only this travels. The target the question
    /// showed does not — the build asks for "the newest" again, as the
    /// CLI does, so a release landing while the question waited is
    /// installed rather than skipped.
    Update {
        expected: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_render_boundary_sanitizes_every_kind() {
        // build_msg IS the boundary — this guards the exact function whose
        // removal would re-open the hole.
        let hostile = "warning: `foo` shadowed by /tmp/\u{1b}]0;pwned\u{7}/foo";
        for kind in [
            crate::LineKind::Cargo,
            crate::LineKind::Notice,
            crate::LineKind::Warning,
        ] {
            let line = match build_msg(kind, hostile) {
                BuildMsg::Cargo(l) | BuildMsg::Notice(l) | BuildMsg::Warning(l) => l,
                BuildMsg::NeedAuth { .. }
                | BuildMsg::Done(_)
                | BuildMsg::MemberStarted { .. }
                | BuildMsg::MemberDone { .. } => {
                    panic!("a line kind maps to a line message")
                }
            };
            assert!(
                !line.chars().any(char::is_control),
                "no control byte crosses the boundary: {line:?}"
            );
            assert!(line.contains("shadowed"), "the words do survive");
        }
    }
}
