// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The build and placement pipeline.
//!
//! One crate from an unprivileged stage build to its manifest commit:
//! the crate is the unit of execution and commit, a batch is only an
//! iteration policy above it. Builds run as the invoking user;
//! escalation happens at the placement door. Cancellation is a typed
//! outcome.

use crate::PinPolicy;
use crate::hints::pasteable_prefix;
use crate::manifest::{Entry, Manifest};
use crate::validate::validate_name;
use crate::{install_needs_privilege, privileged, shadow, stage};
use anyhow::{Context, Result, bail};
use semver::Version;
use std::path::{Path, PathBuf};

/// Binaries the old entry provided that the new build no longer does;
/// without cleanup an update would strand them, the manifest already
/// having forgotten them.
fn obsolete_bins(old: &[String], new: &[String]) -> Vec<String> {
    old.iter().filter(|b| !new.contains(b)).cloned().collect()
}

/// New names this build introduces — the only ones removed when an
/// operation fails before its manifest commit: an overwritten pre-owned
/// name stays (a retry replaces it), a leftover new one would collide
/// as seemingly unmanaged.
fn newly_introduced_bins(old: &[String], new: &[String]) -> Vec<String> {
    obsolete_bins(new, old)
}

/// Undo bookkeeping for a partial install: the new names, and which
/// were placed. Complete only because placement is atomic (same-dir
/// temp + rename); if that ever changes, this rollback develops a hole.
struct RollbackSet {
    /// Names absent from the previous manifest entry for this crate.
    new_names: Vec<String>,
    /// Destinations among `new_names` that were actually placed.
    placed: Vec<PathBuf>,
}

impl RollbackSet {
    /// Snapshot *before* the new entry is inserted; any later, every name
    /// looks pre-owned and the set comes out empty.
    fn snapshot(manifest: &Manifest, name: &str, new_bins: &[String]) -> Self {
        let old_bins = manifest
            .crates
            .get(name)
            .map(|e| e.bins.clone())
            .unwrap_or_default();
        Self {
            new_names: newly_introduced_bins(&old_bins, new_bins),
            placed: Vec::new(),
        }
    }

    /// Record a successful placement; only new names become rollback state.
    fn note_placed(&mut self, bin: &str, dest: PathBuf) {
        if self.new_names.iter().any(|n| n == bin) {
            self.placed.push(dest);
        }
    }
}

/// Best-effort removal of newly placed binaries after a failure before
/// the manifest commit. Never errors: the original failure propagates
/// unmasked (and the likely cause — sudo trouble — would sink these
/// removals too). Per-file, survivors named: a leftover new binary
/// would greet the retry as "already exists and is not managed".
///
/// Recovery also leans on `check_collisions` owning by *name* and
/// `remove_files` being `rm -f`. Content checksums would break the
/// former — if ever added, verify on remove only.
fn rollback_new_bins(policy: privileged::Policy, placed: &[PathBuf]) {
    if placed.is_empty() {
        return;
    }
    eprintln!("rolling back newly installed binaries");
    for path in placed {
        if privileged::remove_files(policy, &[path.as_path()]).is_err() {
            eprintln!(
                "warning: could not remove {}; remove it manually before retrying",
                path.display()
            );
        }
    }
}

/// Refuse to clobber anything we do not own: a destination must not
/// exist, or the manifest must say this very crate installed it —
/// checked before placement, so the existing file stays untouched.
fn check_collisions(
    manifest: &Manifest,
    name: &str,
    bins: &[String],
    bin_dir: &Path,
) -> Result<()> {
    for bin in bins {
        let owned_by_self = manifest
            .crates
            .get(name)
            .is_some_and(|e| e.bins.contains(bin));
        if owned_by_self {
            continue;
        }
        if let Some((other, _)) = manifest
            .crates
            .iter()
            .find(|(n, e)| n.as_str() != name && e.bins.contains(bin))
        {
            bail!("binary `{bin}` is already provided by crate `{other}`");
        }
        let dest = bin_dir.join(bin);
        // symlink_metadata: a dangling symlink still occupies the name.
        if dest.symlink_metadata().is_ok() {
            // With Err-path rollback, only a hard kill between placement and
            // commit produces this; out of scope for auto-recovery, so the error
            // names the manual way out.
            bail!(
                "{} already exists and is not managed by cargo-lbin \
                 (if it is a leftover from an interrupted run, remove it and retry)",
                dest.display()
            );
        }
    }
    Ok(())
}

/// What a captured line is, decided where it is spoken: a frontend
/// cannot reconstruct note-vs-warning from anonymous text.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineKind {
    /// cargo's own output, streamed under the gauge.
    Cargo,
    /// The pipeline narrating itself: "installed …", "waiting for the
    /// state lock…". Presentation-worthy while it happens.
    Notice,
    /// Something the person should still see when everything else went
    /// fine — a shadowed binary does not stop being shadowed because
    /// the install succeeded.
    Warning,
}

/// The phases one in-place build moves through; the numeric values are
/// the atomic encoding, nothing more.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum BuildPhase {
    /// Building — or not even started yet. Cancellable.
    Building = 0,
    /// Placement has begun: privileged writes may be in flight. Too late
    /// to cancel; placement is seconds, not minutes.
    Placement = 1,
    /// A cancel was accepted while building: SIGTERM to cargo's group; the
    /// worker sees it at the placement door at the latest.
    Cancelling = 2,
}

/// Shared control between the UI thread and one build. The phase moves
/// only by compare-and-swap, so a cancel and the step into placement
/// have exactly one winner.
#[cfg(feature = "tui")]
pub(crate) struct BuildControl {
    phase: std::sync::atomic::AtomicU8,
    /// cargo's process group id: 0 before the spawn and again from the
    /// reap — a collected group id is the kernel's to reuse, so it is
    /// withdrawn right after `wait()`. A tiny reuse window remains (load,
    /// lose the CPU, signal a stranger); closing it needs pidfds — named
    /// here instead of denied.
    pgid: std::sync::atomic::AtomicI32,
}

/// What a cancel request found; the UI phrases each differently.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum CancelOutcome {
    /// The request won: the build will not reach placement. SIGTERM went
    /// to the group, or the spawn announcement delivers it (see `spawned`).
    Accepted,
    /// Already cancelling with the group still standing — escalated to
    /// SIGKILL: the person asked twice.
    Killed,
    /// Already cancelling, nothing left to signal: not yet spawned or
    /// already reaped.
    AlreadyStopping,
    /// Placement had already begun; nothing was signalled.
    TooLate,
}

#[cfg(feature = "tui")]
impl BuildControl {
    const ORD: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;

    pub fn new() -> Self {
        Self {
            phase: std::sync::atomic::AtomicU8::new(BuildPhase::Building as u8),
            pgid: std::sync::atomic::AtomicI32::new(0),
        }
    }

    pub(crate) fn phase(&self) -> BuildPhase {
        match self.phase.load(Self::ORD) {
            0 => BuildPhase::Building,
            1 => BuildPhase::Placement,
            _ => BuildPhase::Cancelling,
        }
    }

    /// Whether a cancel is accepted — a cheap courtesy read, never the
    /// placement decision, which belongs to `begin_placement`'s CAS alone.
    pub fn cancelled(&self) -> bool {
        self.phase() == BuildPhase::Cancelling
    }

    /// The worker announces cargo's group, straight from the spawn.
    /// Store-then-check mirrors `request_cancel`'s swap-then-load, so the
    /// deferred signal is delivered at least once; twice is harmless.
    pub fn spawned(&self, pgid: i32) {
        self.pgid.store(pgid, Self::ORD);
        if self.cancelled() {
            Self::signal(pgid, libc::SIGTERM);
        }
    }

    /// Done signalling the group: leader reaped and, on cancellation,
    /// survivors swept — a group id stays alive with *any* member. From
    /// here the id must not be signalled again.
    pub fn reaped(&self) {
        self.pgid.store(0, Self::ORD);
    }

    /// The one-way door into placement, crossed right before the first
    /// write that would need rolling back; refusal means a cancel won.
    pub fn begin_placement(&self) -> Result<()> {
        self.phase
            .compare_exchange(
                BuildPhase::Building as u8,
                BuildPhase::Placement as u8,
                Self::ORD,
                Self::ORD,
            )
            .map(|_| ())
            .map_err(|_| anyhow::Error::new(BuildCancelled))
    }

    /// A cancel from the UI thread. First request: SIGTERM to the
    /// group. Second: SIGKILL. After placement began: refused.
    pub fn request_cancel(&self) -> CancelOutcome {
        match self.phase.compare_exchange(
            BuildPhase::Building as u8,
            BuildPhase::Cancelling as u8,
            Self::ORD,
            Self::ORD,
        ) {
            Ok(_) => {
                let pgid = self.pgid.load(Self::ORD);
                if pgid != 0 {
                    Self::signal(pgid, libc::SIGTERM);
                }
                CancelOutcome::Accepted
            }
            Err(current) if current == BuildPhase::Cancelling as u8 => {
                let pgid = self.pgid.load(Self::ORD);
                if pgid == 0 {
                    // Nothing left to signal: not yet spawned or already reaped.
                    // "SIGKILL sent" here would be a lie the UI repeats.
                    CancelOutcome::AlreadyStopping
                } else {
                    Self::signal(pgid, libc::SIGKILL);
                    CancelOutcome::Killed
                }
            }
            Err(_) => CancelOutcome::TooLate,
        }
    }

    /// Negative pid: the whole group. The result is ignored on purpose —
    /// ESRCH means already gone, which is the goal.
    fn signal(pgid: i32, sig: i32) {
        // SAFETY: kill(2) with a negative pid signals a process group;
        // no memory is touched and any error is an acceptable no-op.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// Marker error for a person-cancelled build. A distinct type: the
/// worker classifies by downcast — guessing from the phase flag would
/// present cargo's own death as "cancelled" in one race.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) struct BuildCancelled;

#[cfg(feature = "tui")]
impl std::fmt::Display for BuildCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("build cancelled")
    }
}

#[cfg(feature = "tui")]
impl std::error::Error for BuildCancelled {}

/// How a build-and-place operation talks to the person while it runs.
pub(crate) enum Frontend<'a> {
    /// The CLI owns the terminal: cargo output is inherited verbatim,
    /// notes go to stdout, warnings to stderr, and sudo prompts where it
    /// must.
    Terminal,
    /// Terminal plus a caller-supplied checkpoint at the placement door —
    /// migrate's early source revalidation, run at every prefix,
    /// escalating or not (unlike `before_placement`).
    Checkpointed {
        checkpoint: &'a mut dyn FnMut() -> Result<()>,
    },
    /// A screen-owning frontend (the TUI): every line is forwarded,
    /// classified at the source — a frontend cannot reconstruct that from
    /// text. Placement waits on a credential checkpoint: a hidden password
    /// prompt would hang the alternate screen.
    #[cfg(feature = "tui")]
    Captured {
        on_line: &'a mut dyn FnMut(LineKind, &str),
        /// Called with the prefix the escalation is *for*: one worker can
        /// revalidate for both prefixes of a migration, and the prompt must
        /// name the right one.
        before_placement: &'a mut dyn FnMut(&Path) -> Result<()>,
        /// The cancel state machine shared with the UI thread.
        control: &'a BuildControl,
        /// Composed *after* the placement door: only the worker that crossed
        /// it runs the checkpoint, still ahead of anything committing — the
        /// captured migration's early source revalidation.
        checkpoint: Option<&'a mut dyn FnMut() -> Result<()>>,
    },
    /// Keeps the lifetime honest when the tui feature is off.
    #[cfg(not(feature = "tui"))]
    #[allow(dead_code)]
    Never(std::marker::PhantomData<&'a ()>),
}

impl Frontend<'_> {
    /// The build itself: terminal-inherited or captured, one call site.
    #[cfg_attr(not(feature = "tui"), allow(unused_variables))]
    fn build(
        &mut self,
        name: &str,
        version: Option<&Version>,
        locked: bool,
        stage_dir: &Path,
        cache: &Path,
    ) -> Result<stage::Built> {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => {
                stage::build(name, version, locked, stage_dir)
            }
            #[cfg(feature = "tui")]
            Frontend::Captured {
                on_line, control, ..
            } => {
                // Courtesy check only: a cancel accepted while the lock
                // was being waited on should not cost a spawn. The race
                // (cancel landing mid-spawn) is closed by `spawned`,
                // not here.
                if control.cancelled() {
                    return Err(anyhow::Error::new(BuildCancelled));
                }
                stage::build_captured(
                    name,
                    version,
                    locked,
                    stage_dir,
                    &cache.join("logs"),
                    &mut |l| on_line(LineKind::Cargo, l),
                    // Spawn announcement, reap withdrawal and the
                    // cancelled-vs-failed classification all live inside
                    // `build_captured` — the only place that holds the
                    // child and its exit status.
                    control,
                )
            }
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// Pipeline notes a person should read — "installed …", "removed
    /// obsolete …". stdout in the terminal, forwarded when captured.
    fn note(&mut self, s: &str) {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => println!("{s}"),
            #[cfg(feature = "tui")]
            Frontend::Captured { on_line, .. } => on_line(LineKind::Notice, s),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// Warnings: stderr in the terminal; captured with their kind — a
    /// warning melted into anonymous text is a warning lost.
    pub(crate) fn warning(&mut self, s: &str) {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => eprintln!("{s}"),
            #[cfg(feature = "tui")]
            Frontend::Captured { on_line, .. } => on_line(LineKind::Warning, s),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// The cancel door, crossed unconditionally right before the first
    /// write needing rollback — distinct from `before_placement` (sudo
    /// only), so "too late" means the same at every prefix. Deliberately
    /// last: a cancel arriving during the blocking sudo wait must win.
    fn placement_begins(&mut self) -> Result<()> {
        match self {
            Frontend::Terminal => Ok(()),
            Frontend::Checkpointed { checkpoint } => checkpoint(),
            #[cfg(feature = "tui")]
            Frontend::Captured {
                control,
                checkpoint,
                ..
            } => {
                // Order is the contract: CAS first (ownership), checkpoint second —
                // still before the first write needing rollback, where the migrate
                // contract promises its early revalidation. The brief
                // TooLate-while-untouched window is the price of one-way doors.
                control.begin_placement()?;
                if let Some(checkpoint) = checkpoint {
                    checkpoint()?;
                }
                Ok(())
            }
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// The placement checkpoint: make sure the credentials placement
    /// needs exist *now*, whatever "now" costs to find out.
    ///
    /// This is where the password is collected, not before the build.
    /// A terminal frontend prompts through `preauthorize`; a captured
    /// one hands the request to the interface, which suspends the
    /// screen and asks. Called only when placement will escalate, and
    /// re-probed here rather than trusted from before the build — the
    /// privileged sites re-check writability, and a build can outlive a
    /// credential timestamp.
    fn before_placement(&mut self, prefix: &Path) -> Result<()> {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => {
                privileged::preauthorize(prefix, true, privileged::AuthPurpose::Placement)
            }
            #[cfg(feature = "tui")]
            Frontend::Captured {
                before_placement, ..
            } => before_placement(prefix),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }
}

/// The placement door: collect the credentials placement will spend,
/// then cross the cancel door — in that order, and only now.
///
/// This is the whole of "late authorization": the build is behind us,
/// nothing privileged has happened yet, and the password (if one is
/// wanted at all) is asked for here. A refusal returns before the
/// first write, so the prefix and the manifest are untouched; the
/// caller adds the words for that.
fn authorize_placement(escalates: bool, prefix: &Path, frontend: &mut Frontend<'_>) -> Result<()> {
    if escalates {
        frontend.before_placement(prefix)?;
    }
    frontend.placement_begins()
}

/// When the shadow scan speaks for an install.
///
/// An ordinary install's result *is* the state the moment it commits,
/// so it reports then. A migration has a second phase that changes
/// what stands on `PATH` — it retires the source copy, or fails to and
/// deliberately leaves it — and nothing before that phase knows which.
/// So it defers: the caller scans the real state once the retirement
/// has answered, and says what it finds instead of what it predicted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShadowReport {
    OnCommit,
    Deferred,
}

/// Build one crate, verify destination ownership, place, clean up
/// obsolete binaries and commit the manifest — all before the next
/// crate, so a mid-batch failure never leaves installed files
/// unrecorded. Returns the version the manifest committed — for a
/// `None` request, whatever cargo's resolution picked, which no caller
/// could know beforehand. A fresh leased run per crate, removed only
/// after the commit: a shared stage let stale binaries fail builds and
/// let a different `--locked` be skipped as "already installed", and
/// the nonce-fresh name means there is never anything to wipe first.
// Nine arguments like `place_and_commit`, same reason: one install's
// parameters; a struct would be built only to be destructured here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_and_commit(
    prefix: &Path,
    cache: &Path,
    manifest: &mut Manifest,
    name: &str,
    version: Option<&Version>,
    locked: bool,
    pin: PinPolicy,
    shadow: ShadowReport,
    frontend: &mut Frontend<'_>,
) -> Result<Version> {
    // Revalidate even post-CLI-check: on updates `name` comes from the
    // manifest, and a hand-edited path-like name must not steer the
    // remove_dir_all below.
    validate_name(name)?;
    // A captured frontend forbids prompting: privileged calls run
    // `sudo -n`, so a wanted password becomes a loud panel error, not a
    // prompt hung beneath the alternate screen.
    let policy = match frontend {
        Frontend::Terminal | Frontend::Checkpointed { .. } => {
            privileged::Policy::for_prefix(prefix)
        }
        #[cfg(feature = "tui")]
        Frontend::Captured { .. } => privileged::Policy::for_prefix(prefix).screen_owned(),
        #[cfg(not(feature = "tui"))]
        Frontend::Never(_) => unreachable!(),
    };
    // Read-only, and early on purpose: a prefix this policy may not
    // write is named before minutes are spent building for it. No
    // password is collected here — that happens at the placement door,
    // after the build. Enforcement proper lives at every privileged
    // call site.
    install_needs_privilege(policy, prefix)?;
    // Per-run stage: the state lock serializes per *prefix*, so two runs
    // on different prefixes may build the same crate — one wiping the
    // other's stage must be structurally impossible. The nonce makes it
    // so even across PID reuse, which also retires the pre-wipe: a
    // fresh name has nothing to clear. The namespace is stage-v2 — see
    // RUN_NAMESPACE for why the formats do not share a directory.
    let run_dir = cache
        .join(stage::RUN_NAMESPACE)
        .join(stage::new_run_dir_name().context("naming the build stage")?);
    // Held across the whole install and inherited by every child
    // spawned from here on — cargo, rustc, build scripts — so the
    // lease's lifetime is exactly "someone may still write to this
    // stage", not this process's.
    let lease = stage::Lease::acquire(&run_dir)?;
    let stage_dir = run_dir.join(name);
    let built = frontend.build(name, version, locked, &stage_dir, cache);
    // A cancel's stage is evidence of nothing — and neither is its
    // run, so the creator's cleanup runs, veto included: an inheritor
    // still writing keeps the run alive. Every other build failure
    // keeps its run (and lease file) as forensics; the lease itself is
    // released when the last holder exits, and verify/clean take it
    // from there.
    #[cfg(feature = "tui")]
    let built = match built {
        Err(e) => {
            if e.downcast_ref::<BuildCancelled>().is_some() {
                stage::release_and_remove_run(lease, &run_dir);
            }
            return Err(e);
        }
        Ok(built) => built,
    };
    #[cfg(not(feature = "tui"))]
    let built = built?;
    check_collisions(manifest, name, &built.bins, &prefix.join("bin"))?;
    // Only names this crate did not provide before: carried-over names
    // were reported when they were new.
    let new_bins: Vec<String> = built
        .bins
        .iter()
        .filter(|b| {
            !manifest
                .crates
                .get(name)
                .is_some_and(|e| e.bins.contains(b))
        })
        .cloned()
        .collect();
    if shadow == ShadowReport::OnCommit {
        for w in shadow_warnings(prefix, &new_bins) {
            frontend.warning(&w);
        }
    }

    // Snapshot before `place_and_commit` inserts the new manifest entry;
    // see `RollbackSet::snapshot` for why the order is load-bearing.
    let mut rollback = RollbackSet::snapshot(manifest, name, &built.bins);
    // Resolved against the entry as it still is: under Infer an exact
    // version pins, and an existing pin is carried — never dropped by a
    // rewrite. Under Exactly the caller already promised.
    let pinned = match pin {
        PinPolicy::Infer => {
            version.is_some() || manifest.crates.get(name).is_some_and(|e| e.pinned)
        }
        PinPolicy::Exactly(pinned) => pinned,
    };
    // The checkpoint sits between the last unprivileged step and the
    // first privileged one, re-probed now (not before the build): the
    // privileged sites re-check writability, and a stale answer could
    // skip the checkpoint right before `sudo -n` finds a new need.
    // Fallible as one unit with the sudo revalidation: BuildCancelled
    // from anywhere before placement means no stage left — a cancel's
    // stage is evidence of nothing; every other refusal keeps its stage
    // like any pipeline failure.
    let checkpoints = (|| -> Result<()> {
        let escalates = install_needs_privilege(policy, prefix)?;
        authorize_placement(escalates, prefix, frontend)
    })();
    // A refusal here lands after cargo has said "Installed package" about
    // the *stage*, so the answer says which of the two happened: the
    // build did, the placement did not.
    let checkpoints = checkpoints.context(
        "the build finished, but placement did not begin: no files were placed and the \
         manifest is unchanged",
    );
    #[cfg(feature = "tui")]
    if let Err(e) = checkpoints {
        if e.downcast_ref::<BuildCancelled>().is_some() {
            stage::release_and_remove_run(lease, &run_dir);
        }
        return Err(e);
    }
    #[cfg(not(feature = "tui"))]
    checkpoints?;
    // The version about to become the manifest's truth, held before
    // `place_and_commit` consumes `built`: the stage's verified
    // .crates2.json is the only honest answer to "what was installed" —
    // for a `None` request nothing upstream knows it.
    let installed = built.version.clone();
    if let Err(err) = place_and_commit(
        prefix,
        policy,
        manifest,
        name,
        built,
        locked,
        pinned,
        &mut rollback,
        frontend,
    ) {
        rollback_new_bins(policy, &rollback.placed);
        return Err(err);
    }
    // Run removal is deliberately last: a stage surviving a failure is
    // forensic evidence of exactly the build that caused it — and it
    // keeps its `.lease`, so once this process (and every inheritor)
    // exits, the run is released and becomes exactly what verify and
    // clean are for. A success removes the run through the creator's
    // cleanup, veto included: "until the last process that may still
    // write to the stage exits" binds cargo-lbin's own hand too, not
    // just a later clean's.
    stage::release_and_remove_run(lease, &run_dir);
    Ok(installed)
}

/// One warning per binary that a PATH entry outside the prefix already
/// provides, naming file, owner (if the package manager says) and PATH
/// order. Warning, not refusal (see `shadow`); only for names new to
/// this crate — shadowing that arises later is external drift, and
/// re-warning on every update would be the price of catching it.
fn shadow_warnings(prefix: &Path, bins: &[String]) -> Vec<String> {
    // Install frontends print these raw, so the severity word travels in
    // the string; the verify-side adapter owns its Finding framing — no
    // "warning: warning:".
    shadow::notes(prefix, bins)
        .into_iter()
        .map(|n| format!("warning: {n}"))
        .collect()
}

/// Everything between first privileged placement and manifest commit,
/// fallible as one unit: the single caller rolls back on any `Err`,
/// without cleanup code at every `?`.
#[allow(clippy::too_many_arguments)]
fn place_and_commit(
    prefix: &Path,
    policy: privileged::Policy,
    manifest: &mut Manifest,
    name: &str,
    built: stage::Built,
    locked: bool,
    pinned: bool,
    rollback: &mut RollbackSet,
    frontend: &mut Frontend<'_>,
) -> Result<()> {
    let bin_dir = prefix.join("bin");
    // Open and verify every staged source as the user first; root then
    // copies our vetted descriptors via /proc, never a pathname the stage
    // could swap underneath us.
    let verified: Vec<privileged::VerifiedSource> = built
        .bin_paths
        .iter()
        .map(|p| privileged::VerifiedSource::open(p))
        .collect::<Result<_>>()?;
    for (src, bin) in verified.iter().zip(&built.bins) {
        let dest = bin_dir.join(bin);
        privileged::install_verified(policy, src, &dest, "755")?;
        rollback.note_placed(bin, dest);
    }
    drop(verified);
    let installed: Vec<PathBuf> = built.bins.iter().map(|b| bin_dir.join(b)).collect();
    let installed_refs: Vec<&Path> = installed.iter().map(PathBuf::as_path).collect();
    privileged::restorecon(policy, &installed_refs);

    if let Some(old) = manifest.crates.get(name) {
        let obsolete = obsolete_bins(&old.bins, &built.bins);
        if !obsolete.is_empty() {
            let paths: Vec<PathBuf> = obsolete.iter().map(|b| bin_dir.join(b)).collect();
            let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
            privileged::remove_files(policy, &refs)?;
            frontend.note(&format!(
                "removed obsolete binaries: {}",
                obsolete.join(", ")
            ));
        }
    }

    let bins_list = built.bins.join(", ");
    // `pinned` arrives resolved (see `install_and_commit`), read before
    // this function's mutable borrow of the manifest began.
    commit_entry(
        manifest,
        prefix,
        policy,
        name,
        Entry {
            version: built.version.to_string(),
            bins: built.bins,
            locked,
            pinned,
            // The build's own testimony rides the same commit as its
            // bytes.
            built_with_rustc: built.rustc,
        },
    )?;
    // Announced only after the commit — an "installed" before `store`
    // could be followed by its own undoing — and keyed off the committed
    // state, which is what `unpin` would change.
    let pin_note = if pinned {
        match pasteable_prefix(prefix) {
            Some(arg) => format!(" [pinned; `cargo lbin unpin {name} {arg}` to allow updates]"),
            None => " [pinned; unpin to allow updates]".to_owned(),
        }
    } else {
        String::new()
    };
    frontend.note(&format!(
        "installed {name} {} -> {} ({bins_list}){pin_note}",
        built.version,
        bin_dir.display(),
    ));
    Ok(())
}

/// Insert `entry` and persist as one unit; on a failed store the
/// in-memory manifest is restored to what is on disk — the invariant
/// that makes continuing a batch after a failure sound.
fn commit_entry(
    manifest: &mut Manifest,
    prefix: &Path,
    policy: privileged::Policy,
    name: &str,
    entry: Entry,
) -> Result<()> {
    let previous = manifest.crates.insert(name.to_owned(), entry);
    if let Err(err) = manifest.store_with_policy(prefix, policy) {
        if let Some(old) = previous {
            manifest.crates.insert(name.to_owned(), old);
        } else {
            manifest.crates.remove(name);
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::manifest_with;
    #[cfg(feature = "tui")]
    use crate::verify::scan_stale_stages;
    #[cfg(feature = "tui")]
    use std::fs;

    #[test]
    fn commit_entry_restores_memory_on_store_failure() {
        let tmp = std::env::temp_dir().join("cargo-lbin-test-commit-entry");
        let _ = std::fs::remove_dir_all(&tmp);
        let prefix = tmp.join("prefix");
        // A regular file where the manifest directory should be makes the
        // store fail after the in-memory insert.
        std::fs::create_dir_all(prefix.join("share")).unwrap();
        std::fs::write(prefix.join("share/cargo-lbin"), b"").unwrap();

        let entry = |v: &str| Entry {
            version: v.to_owned(),
            bins: vec!["foo".to_owned()],
            locked: false,
            pinned: false,
            built_with_rustc: None,
        };
        // Update of an existing crate: the old entry must come back.
        let mut m = manifest_with(&["foo"]);
        assert!(
            commit_entry(
                &mut m,
                &prefix,
                privileged::Policy::for_prefix(&prefix),
                "foo",
                entry("2.0.0"),
            )
            .is_err()
        );
        assert_eq!(m.crates["foo"].version, "1.0.0");
        // Fresh install: the name must disappear again.
        let mut m = Manifest::default();
        assert!(
            commit_entry(
                &mut m,
                &prefix,
                privileged::Policy::for_prefix(&prefix),
                "foo",
                entry("2.0.0"),
            )
            .is_err()
        );
        assert_eq!(m.crates.keys().collect::<Vec<_>>(), Vec::<&String>::new());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn obsolete_is_old_minus_new() {
        let old = vec!["foo".to_owned(), "fooctl".to_owned()];
        let new = vec!["foo".to_owned()];
        assert_eq!(obsolete_bins(&old, &new), vec!["fooctl".to_owned()]);
        assert_eq!(obsolete_bins(&new, &old), [] as [String; 0]);
        assert_eq!(obsolete_bins(&old, &old), [] as [String; 0]);
    }

    #[test]
    fn newly_introduced_is_new_minus_old() {
        let old = vec!["foo".to_owned()];
        let new = vec!["foo".to_owned(), "fooctl".to_owned()];
        assert_eq!(newly_introduced_bins(&old, &new), vec!["fooctl".to_owned()]);
        // Fresh install: everything is new, rollback covers the full set.
        assert_eq!(newly_introduced_bins(&[], &new), new);
        // Pure version bump: nothing is new, rollback removes nothing —
        // the overwritten binaries stay, recoverable via the manifest.
        assert_eq!(newly_introduced_bins(&new, &new), [] as [String; 0]);
    }

    #[test]
    fn rollback_set_tracks_only_new_names_actually_placed() {
        let mut manifest = Manifest::default();
        manifest.crates.insert(
            "foo".to_owned(),
            Entry {
                version: "1.0.0".to_owned(),
                bins: vec!["foo".to_owned()],
                locked: false,
                pinned: false,
                built_with_rustc: None,
            },
        );
        let new_bins = vec!["foo".to_owned(), "fooctl".to_owned(), "fooadmin".to_owned()];
        let bin_dir = Path::new("/nonexistent/bin");

        let mut set = RollbackSet::snapshot(&manifest, "foo", &new_bins);
        // `foo` is pre-owned: overwriting it is recoverable, never rolled
        // back — the manifest still claims the name.
        set.note_placed("foo", bin_dir.join("foo"));
        assert_eq!(set.placed, [] as [PathBuf; 0]);
        // `fooctl` is new and was placed: rollback state until the commit.
        set.note_placed("fooctl", bin_dir.join("fooctl"));
        assert_eq!(set.placed, vec![bin_dir.join("fooctl")]);
        // `fooadmin` is new but its placement failed before `note_placed`;
        // atomic placement guarantees nothing exists on disk, so the set
        // rightly never learns about it.

        // Unknown crate: a fresh install marks every name as new.
        let fresh = RollbackSet::snapshot(&Manifest::default(), "bar", &new_bins);
        assert_eq!(fresh.new_names, new_bins);
        assert_eq!(fresh.placed, [] as [PathBuf; 0]);
    }

    #[test]
    fn collisions_are_detected_before_placement() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-collision");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut manifest = Manifest::default();
        manifest.crates.insert(
            "owner".to_owned(),
            Entry {
                version: "1.0.0".to_owned(),
                bins: vec!["shared".to_owned()],
                locked: false,
                pinned: false,
                built_with_rustc: None,
            },
        );

        // Same crate re-providing its own binary: fine.
        assert!(check_collisions(&manifest, "owner", &["shared".to_owned()], &dir).is_ok());
        // Another crate claiming it: error naming the owner.
        let err = check_collisions(&manifest, "intruder", &["shared".to_owned()], &dir)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("owner"),
            "error should name the owning crate: {err}"
        );
        // Unmanaged file on disk: error.
        std::fs::write(dir.join("stray"), b"").unwrap();
        assert!(check_collisions(&manifest, "newcrate", &["stray".to_owned()], &dir).is_err());
        // Nonexistent destination: fine.
        assert!(check_collisions(&manifest, "newcrate", &["fresh".to_owned()], &dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn cancel_and_placement_have_exactly_one_winner() {
        // Cancel first: the door refuses with the typed cancellation; a
        // second cancel with no live group reports "already stopping".
        let control = BuildControl::new();
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        assert!(control.cancelled());
        let door = control.begin_placement().unwrap_err();
        assert!(
            door.downcast_ref::<BuildCancelled>().is_some(),
            "cancel won the race, and says so by type"
        );
        assert!(matches!(
            control.request_cancel(),
            CancelOutcome::AlreadyStopping
        ));

        // Placement first: the door is one-way and a cancel after it is
        // told so, with nothing signalled.
        let control = BuildControl::new();
        control.begin_placement().unwrap();
        assert!(matches!(control.request_cancel(), CancelOutcome::TooLate));
        assert!(!control.cancelled(), "TooLate never flips the phase");

        // A cancel accepted before the spawn is not lost: `spawned` routes
        // the deferred SIGTERM.
        let control = BuildControl::new();
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        assert!(control.cancelled());
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_door_settles_ownership_before_the_checkpoint_runs() {
        // A lost race must not so much as probe: the checkpoint of a
        // cancelled operation is never consulted.
        let control = BuildControl::new();
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        let mut ran = 0usize;
        let mut frontend = Frontend::Captured {
            on_line: &mut |_, _| {},
            before_placement: &mut |_| Ok(()),
            control: &control,
            checkpoint: Some(&mut || {
                ran += 1;
                Ok(())
            }),
        };
        let err = frontend.placement_begins().unwrap_err();
        assert!(err.downcast_ref::<BuildCancelled>().is_some());
        let _ = frontend;
        assert_eq!(ran, 0, "a cancelled operation never probes");

        // The winner runs it, exactly once, after the door.
        let control = BuildControl::new();
        let mut ran = 0usize;
        let mut frontend = Frontend::Captured {
            on_line: &mut |_, _| {},
            before_placement: &mut |_| Ok(()),
            control: &control,
            checkpoint: Some(&mut || {
                ran += 1;
                Ok(())
            }),
        };
        frontend.placement_begins().unwrap();
        let _ = frontend;
        assert_eq!(
            ran, 1,
            "the worker that crossed the door runs the checkpoint"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_second_cancel_kills_a_term_ignoring_build() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-cancel-escalate");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("fakebin");
        let prefix = root.join("prefix");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();

        // A leader that ignores SIGTERM: the first cancel is accepted
        // and changes nothing; only the escalation ends it. This is the
        // Killed arm of `request_cancel`, live.
        let script = fake_bin.join("cargo");
        fs::write(&script, "#!/bin/sh\ntrap '' TERM\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let control = std::sync::Arc::new(BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let cache = root.join("cache");
        let prefix_w = prefix.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            let mut manifest = Manifest::default();
            install_and_commit(
                &prefix_w,
                &cache,
                &mut manifest,
                "stubborncrate",
                None,
                false,
                PinPolicy::Infer,
                ShadowReport::OnCommit,
                &mut Frontend::Captured {
                    on_line: &mut |_, _| {},
                    before_placement: &mut |_| Ok(()),
                    control: &worker_control,
                    checkpoint: None,
                },
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        // The escalation needs a spawned, still-living group; poll until
        // the second cancel finds one rather than racing the spawn.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match control.request_cancel() {
                CancelOutcome::Killed => break,
                CancelOutcome::AlreadyStopping if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => panic!("expected Killed before the deadline, got {other:?}"),
            }
        }
        let err = worker.join().unwrap().expect_err("SIGKILL ended the build");
        assert!(
            err.downcast_ref::<BuildCancelled>().is_some(),
            "an escalated cancel is still the typed cancellation: {err:#}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "SIGKILL ended the build, not the sleep"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_cancel_sweeps_group_members_that_ignore_sigterm() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-cancel-sweep");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("fakebin");
        let prefix = root.join("prefix");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();

        // The sweep's reason: a member ignoring TERM keeps the group id
        // alive past the leader's reap; the stray records its pid so the
        // test watches it die.
        let stray_pid_file = root.join("stray.pid");
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 sh -c 'trap \"\" TERM; echo $$ > \"{pidfile}\"; while :; do sleep 0.1; done' &\n\
                 sleep 30\n",
                pidfile = stray_pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let control = std::sync::Arc::new(BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let cache = root.join("cache");
        let cache_w = cache.clone();
        let prefix_w = prefix.clone();
        let worker = std::thread::spawn(move || {
            let mut manifest = Manifest::default();
            install_and_commit(
                &prefix_w,
                &cache_w,
                &mut manifest,
                "straycrate",
                None,
                false,
                PinPolicy::Infer,
                ShadowReport::OnCommit,
                &mut Frontend::Captured {
                    on_line: &mut |_, _| {},
                    before_placement: &mut |_| Ok(()),
                    control: &worker_control,
                    checkpoint: None,
                },
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        let err = worker.join().unwrap().expect_err("cancelled");
        assert!(err.downcast_ref::<BuildCancelled>().is_some(), "{err:#}");

        // The stray must not survive the sweep. SIGKILL delivery is
        // asynchronous, so poll briefly instead of asserting an instant.
        let stray_pid: u32 = fs::read_to_string(&stray_pid_file)
            .expect("the stray recorded its pid before the cancel")
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PathBuf::from(format!("/proc/{stray_pid}")).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the TERM-ignoring group member survived the cancel sweep"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Either the stray died before the creator's cleanup asked, and
        // the run went with the cancel — or it was still holding the
        // inherited lease at that moment and vetoed the removal, which
        // is the feature, not a leak: with the last inheritor now gone
        // the run is exactly the ownerless debris verify names and
        // clean removes.
        let leftovers: Vec<PathBuf> = fs::read_dir(cache.join(crate::stage::RUN_NAMESPACE))
            .map(|entries| entries.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        match leftovers.as_slice() {
            [] => {}
            [run] => {
                // Waited for, not asserted at once: the build waited on
                // its direct child, but the fake cargo is `sh` running
                // `sleep`, and when sh forks rather than execs, the
                // sleeper — holding the inherited lease — dies from the
                // group's SIGTERM a moment after its parent does. On a
                // two-core runner that moment is wide enough to probe
                // into.
                assert!(
                    crate::stage::eventually(std::time::Duration::from_secs(10), || {
                        crate::stage::probe_lease(run) == crate::stage::LeaseState::Released
                    }),
                    "a vetoed run is released once its last inheritor exits"
                );
                assert!(
                    scan_stale_stages(&cache).unwrap().contains(run),
                    "and it is then ownerless debris, not an orphan nobody names"
                );
            }
            other => panic!("one build leaves at most one run: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn escalation_unwedges_a_partial_line_holding_the_pipe() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-cancel-partial");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("fakebin");
        let prefix = root.join("prefix");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();

        // The nastiest stray writes a newline-less fragment and holds the
        // pipe: read_until walks past the cancel check into a blocking read.
        // The escalation SIGKILLs the group and the worker unwedges into the
        // typed cancellation.
        let stray_pid_file = root.join("stray.pid");
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 sh -c 'trap \"\" TERM; printf partial >&2; echo $$ > \"{pidfile}\"; while :; do sleep 1; done' &\n\
                 sleep 30\n",
                pidfile = stray_pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let control = std::sync::Arc::new(BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let cache = root.join("cache");
        let prefix_w = prefix.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            let mut manifest = Manifest::default();
            install_and_commit(
                &prefix_w,
                &cache,
                &mut manifest,
                "partialcrate",
                None,
                false,
                PinPolicy::Infer,
                ShadowReport::OnCommit,
                &mut Frontend::Captured {
                    on_line: &mut |_, _| {},
                    before_placement: &mut |_| Ok(()),
                    control: &worker_control,
                    checkpoint: None,
                },
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        // Give SIGTERM time to take the leader while the worker sits wedged
        // — the exact state the escalation exists for.
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(matches!(control.request_cancel(), CancelOutcome::Killed));
        let err = worker.join().unwrap().expect_err("cancelled");
        assert!(
            err.downcast_ref::<BuildCancelled>().is_some(),
            "the unwedged worker still reports the typed cancellation: {err:#}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the escalation ended the wedge, not the sleep"
        );
        let stray_pid: u32 = fs::read_to_string(&stray_pid_file)
            .expect("the stray recorded its pid")
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PathBuf::from(format!("/proc/{stray_pid}")).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pipe-holding stray survived the escalation"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_cancel_stops_a_running_captured_build() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-cancel-running");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("fakebin");
        let prefix = root.join("prefix");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();

        // A fake cargo that would take far longer than this test is
        // allowed to: only a delivered signal ends it early.
        let script = fake_bin.join("cargo");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let control = std::sync::Arc::new(BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let cache = root.join("cache");
        let cache_w = cache.clone();
        let prefix_w = prefix.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            let mut manifest = Manifest::default();
            install_and_commit(
                &prefix_w,
                &cache_w,
                &mut manifest,
                "slowcrate",
                None,
                false,
                PinPolicy::Infer,
                ShadowReport::OnCommit,
                &mut Frontend::Captured {
                    on_line: &mut |_, _| {},
                    before_placement: &mut |_| Ok(()),
                    control: &worker_control,
                    checkpoint: None,
                },
            )
        });
        // Give the worker time to spawn; an earlier cancel is also correct,
        // it just would not exercise this path.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        let result = worker.join().unwrap();
        let err = result.expect_err("a cancelled build never commits");
        assert!(
            err.downcast_ref::<BuildCancelled>().is_some(),
            "the outcome is a typed cancellation, not an anonymous failure: {err:#}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "SIGTERM to the group ended the build, not the sleep"
        );
        assert!(
            !Manifest::path(&prefix).exists() || Manifest::load(&prefix).unwrap().crates.is_empty(),
            "nothing was recorded"
        );
        // A cancellation is not a diagnosis: no failure log is written…
        let logs = cache.join("logs");
        assert!(
            !logs.exists() || fs::read_dir(&logs).unwrap().next().is_none(),
            "a cancelled build writes no failure log"
        );
        // …and the whole run is removed rather than kept as evidence.
        // Either the stray died before the creator's cleanup asked, and
        // the run went with the cancel — or it was still holding the
        // inherited lease at that moment and vetoed the removal, which
        // is the feature, not a leak: with the last inheritor now gone
        // the run is exactly the ownerless debris verify names and
        // clean removes.
        let leftovers: Vec<PathBuf> = fs::read_dir(cache.join(crate::stage::RUN_NAMESPACE))
            .map(|entries| entries.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        match leftovers.as_slice() {
            [] => {}
            [run] => {
                // Waited for, not asserted at once: the build waited on
                // its direct child, but the fake cargo is `sh` running
                // `sleep`, and when sh forks rather than execs, the
                // sleeper — holding the inherited lease — dies from the
                // group's SIGTERM a moment after its parent does. On a
                // two-core runner that moment is wide enough to probe
                // into.
                assert!(
                    crate::stage::eventually(std::time::Duration::from_secs(10), || {
                        crate::stage::probe_lease(run) == crate::stage::LeaseState::Released
                    }),
                    "a vetoed run is released once its last inheritor exits"
                );
                assert!(
                    scan_stale_stages(&cache).unwrap().contains(run),
                    "and it is then ownerless debris, not an orphan nobody names"
                );
            }
            other => panic!("one build leaves at most one run: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// The door itself: with escalation needed, credentials are asked
    /// for *before* the cancel door and before any write, and a refusal
    /// stops there.
    ///
    /// Tested here rather than through `install_and_commit`, because
    /// escalation cannot be provoked portably: these tests run as
    /// whatever user CI provides, and a user who can write the prefix
    /// never reaches the door at all. What the pipeline contributes is
    /// the door's *position* — after the build, before the first write
    /// — which is the call site's single line.
    #[cfg(feature = "tui")]
    #[test]
    fn the_placement_door_asks_first_and_a_refusal_stops_there() {
        let seen = std::cell::RefCell::new(Vec::new());
        // One control per scenario: `placement_begins` is a one-way door
        // and a second crossing is refused by design.
        let control = BuildControl::new();
        let prefix = Path::new("/usr/local");

        // Escalation needed, credentials granted: the door is knocked
        // on, and only then is the cancel door crossed.
        let mut frontend = Frontend::Captured {
            on_line: &mut |_, _| {},
            before_placement: &mut |p: &Path| {
                seen.borrow_mut().push(format!("auth:{}", p.display()));
                Ok(())
            },
            control: &control,
            checkpoint: Some(&mut || {
                seen.borrow_mut().push("checkpoint".to_owned());
                Ok(())
            }),
        };
        authorize_placement(true, prefix, &mut frontend).unwrap();
        assert_eq!(
            seen.borrow().as_slice(),
            ["auth:/usr/local".to_owned(), "checkpoint".to_owned()],
            "the password comes first, the cancel door second"
        );

        // A prefix that escalates for nothing is never asked.
        seen.borrow_mut().clear();
        let fresh = BuildControl::new();
        let mut frontend = Frontend::Captured {
            on_line: &mut |_, _| {},
            before_placement: &mut |p: &Path| {
                seen.borrow_mut().push(format!("auth:{}", p.display()));
                Ok(())
            },
            control: &fresh,
            checkpoint: Some(&mut || {
                seen.borrow_mut().push("checkpoint".to_owned());
                Ok(())
            }),
        };
        authorize_placement(false, prefix, &mut frontend).unwrap();
        assert_eq!(
            seen.borrow().as_slice(),
            ["checkpoint".to_owned()],
            "no escalation, no credentials"
        );

        // A denied password stops the operation at the door: the cancel
        // door is never crossed, so nothing downstream can begin.
        seen.borrow_mut().clear();
        let third = BuildControl::new();
        let mut refusing = Frontend::Captured {
            on_line: &mut |_, _| {},
            before_placement: &mut |_| bail!("sudo authentication failed"),
            control: &third,
            checkpoint: Some(&mut || {
                seen.borrow_mut().push("checkpoint".to_owned());
                Ok(())
            }),
        };
        let err = authorize_placement(true, prefix, &mut refusing)
            .expect_err("a denied password is a refusal");
        assert!(
            format!("{err:#}").contains("sudo authentication failed"),
            "{err:#}"
        );
        assert!(
            seen.borrow().is_empty(),
            "a refusal at the door reaches nothing past it: {:?}",
            seen.borrow()
        );
    }
}
