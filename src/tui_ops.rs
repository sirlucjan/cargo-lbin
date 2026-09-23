//! The TUI's operation adapters: the same mechanisms and contracts the
//! CLI uses, shaped for a screen-owning frontend. Outcomes cross the
//! boundary as typed values or callbacks, and privileged work uses the
//! screen-owned policy.

use crate::build::{BuildCancelled, CancelOutcome};
use crate::build::{BuildControl, Frontend, LineKind, ShadowReport, install_and_commit};
use crate::lock::{Mode, StateLock};
use crate::manifest::Manifest;
use crate::report::Checked;
use crate::report::Report;
use crate::validate::InstallSpec;
use crate::{
    MigrateFrontend, MigrateOutcome, MigrationSnapshot, PinPolicy, ReinstallPlan, cache_dir,
    check_versions, duplicate_install_warnings, migrate_one, privileged, record_knowledge,
    refuse_diagonal, refuse_pinned, reinstall_plan,
};
use anyhow::{Context, Result, bail};
use semver::Version;
use std::path::{Path, PathBuf};

/// `pin`/`unpin` for a screen-owning frontend: one crate, no stdout,
/// outcome as data; nonblocking screen-owned lock, `cmd_set_pinned`
/// semantics — already-in-state is an answer, not a write.
#[cfg(feature = "tui")]
pub(crate) enum TuiSetPinned {
    /// The bit flipped and committed; the version, for the report line.
    Set {
        version: String,
    },
    /// Nothing to do — the manifest already agrees (it moved since the
    /// row was read, or the row was stale).
    Already,
    PrefixBusy,
}

#[cfg(feature = "tui")]
pub(crate) fn tui_set_pinned(prefix: &Path, name: &str, pinned: bool) -> Result<TuiSetPinned> {
    let policy = privileged::Policy::for_prefix(prefix).screen_owned();
    let Some(_lock) = StateLock::try_acquire_with(prefix, &Mode::Exclusive, policy, &mut |_| {})?
    else {
        return Ok(TuiSetPinned::PrefixBusy);
    };
    let mut manifest = Manifest::load(prefix)?;
    let Some(entry) = manifest.crates.get_mut(name) else {
        bail!("`{name}` is not in the manifest (changed since the list was read?)");
    };
    if entry.pinned == pinned {
        return Ok(TuiSetPinned::Already);
    }
    entry.pinned = pinned;
    let version = entry.version.clone();
    manifest.store_with_policy(prefix, policy)?;
    Ok(TuiSetPinned::Set { version })
}

/// `remove` for a screen-owning frontend: outcome as data, the UI owns
/// the words. Exclusive lock taken nonblocking — a blocking wait would
/// freeze the screen — and escalation is the caller's settled decision
/// (`sudo -n` at most beneath the screen).
#[cfg(feature = "tui")]
pub(crate) enum TuiRemove {
    Removed(Vec<String>),
    PrefixBusy,
}

#[cfg(feature = "tui")]
pub(crate) fn tui_remove_one(prefix: &Path, name: &str) -> Result<TuiRemove> {
    let policy = privileged::Policy::for_prefix(prefix).screen_owned();
    let Some(_lock) = StateLock::try_acquire_with(prefix, &Mode::Exclusive, policy, &mut |_| {})?
    else {
        return Ok(TuiRemove::PrefixBusy);
    };
    let mut manifest = Manifest::load(prefix)?;
    let Some(entry) = manifest.crates.remove(name) else {
        // The row came from a reload moments ago; a manifest that moved
        // on since is a real answer, not a skip to swallow.
        bail!("`{name}` is not in the manifest (changed since the list was read?)");
    };
    let bin_dir = prefix.join("bin");
    let paths: Vec<PathBuf> = entry.bins.iter().map(|b| bin_dir.join(b)).collect();
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    privileged::remove_files(policy, &refs)?;
    // The same commit order as the CLI: files first, bookkeeping after,
    // so a failure never records a removal that did not happen.
    manifest.store_with_policy(prefix, policy)?;
    Ok(TuiRemove::Removed(entry.bins))
}

/// `migrate_one` for a screen-owning frontend. The snapshot arrives
/// *frozen* from the keypress through the confirmation — a fresh one
/// after the `y` could bless a version the person never saw. Runs
/// through `MigrateFrontend::Captured`: same panel, cancel door, locks
/// and sudo roundtrip as an install; outcome as data.
#[cfg(feature = "tui")]
pub(crate) fn tui_migrate_one(
    source: &Path,
    dest: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BuildControl,
) -> Result<MigrateOutcome> {
    let cache = cache_dir()?;
    migrate_one(
        source,
        dest,
        &cache,
        name,
        snap,
        &mut MigrateFrontend::Captured {
            on_line,
            before_placement,
            control,
        },
    )
}

/// `downgrade` for the captured frontend: the authoritative check and
/// the build, under one exclusive lock.
///
/// The version was chosen against what the interface displayed, and a
/// list on screen is a snapshot — between the offer and the keypress
/// another process can update the crate, remove it, or flip its
/// `--locked`. Re-reading the rows would only refresh the same
/// snapshot, so the statement that matters is made here, holding the
/// lock the mutation itself holds: the manifest must still say
/// `expected`, or nothing is built. Otherwise a command called
/// downgrade could upgrade (current moved below the choice) or
/// resurrect a crate somebody just removed — the two cases
/// `cmd_downgrade` guards against for exactly this reason. `locked`
/// comes from that same fresh read, never from the row.
#[cfg(feature = "tui")]
pub(crate) fn tui_downgrade_one(
    prefix: &Path,
    name: &str,
    expected: &str,
    version: &Version,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BuildControl,
) -> Result<()> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    let fresh = manifest.crates.get(name).with_context(|| {
        format!("`{name}` was removed while a version was being chosen; press D again")
    })?;
    if fresh.version != expected {
        bail!(
            "`{name}` changed from {expected} to {} while a version was being chosen; \
             press D again",
            fresh.version
        );
    }
    if fresh.pinned {
        // A pin set while the list was open counts as changed state,
        // as it does everywhere else: the newer statement wins.
        bail!("`{name}` was pinned while a version was being chosen; `p` unpins it first");
    }
    let locked = fresh.locked;
    let mut frontend = Frontend::Captured {
        on_line,
        before_placement,
        control,
        checkpoint: None,
    };
    // A pinned crate is downgraded like any other: the pin is a
    // standing instruction, and an exact version is how it is restated
    // — which is precisely what the keypress supplied.
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        name,
        Some(version),
        locked,
        PinPolicy::Infer,
        ShadowReport::OnCommit,
        &mut frontend,
    )?;
    Ok(())
}

/// Cancellation for a batch of builds: the batch's own state, plus a
/// handle to whichever member is building now.
///
/// A `BuildControl` is one-way — once a build passes into placement it
/// cannot be reused — so a batch cannot have one. What a batch has is
/// an intention to stop, and a pointer to the member that intention
/// currently applies to. `c` sets the flag and asks the current member
/// to stop: a member still compiling ends as `Cancelled`, one already
/// placing finishes honestly, and either way the worker starts no one
/// after it.
#[cfg(feature = "tui")]
struct BatchState {
    stopping: bool,
    current: Option<std::sync::Arc<BuildControl>>,
}

#[cfg(feature = "tui")]
pub(crate) struct BatchControl {
    state: std::sync::Mutex<BatchState>,
}

#[cfg(feature = "tui")]
impl BatchControl {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(BatchState {
                stopping: false,
                current: None,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BatchState> {
        // A poisoned lock means a worker panicked mid-batch; the state
        // it left is still the truth about what is running.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// May the next member start, and if so, publish its control —
    /// as one step.
    ///
    /// Two steps would race: the worker could read "not stopping",
    /// `c` could arrive and cancel a member that has already finished,
    /// and the worker could then publish the next one and build it.
    /// Asking and publishing under the same lock makes "stopped" mean
    /// stopped.
    fn try_begin_member(&self, control: &std::sync::Arc<BuildControl>) -> bool {
        let mut state = self.state();
        if state.stopping {
            return false;
        }
        state.current = Some(std::sync::Arc::clone(control));
        true
    }

    /// That member is done. The slot is cleared, so a `c` arriving
    /// between members answers for the batch rather than for a build
    /// that has already ended.
    fn finish_member(&self) {
        self.state().current = None;
    }

    /// Has the batch been told to stop? True from the moment `c` is
    /// pressed, whether or not a member was building at the time.
    pub(crate) fn cancelled(&self) -> bool {
        self.state().stopping
    }

    /// Run `f` against the member building now, if there is one. The
    /// lock is not held while `f` runs — it may block on the interface
    /// — so this is for the worker's own use, where "the member
    /// building now" is the one that called in.
    pub(crate) fn with_current<R>(&self, f: impl FnOnce(&BuildControl) -> R) -> Option<R> {
        let current = self.state().current.clone();
        current.map(|control| f(&control))
    }

    /// `c`: record the batch-level stop intent, and ask the member in
    /// flight to stop too.
    ///
    /// The stop intent is always recorded. What the current member says
    /// is its own: a build still compiling accepts, one already placing
    /// is past the point, and the answer distinguishes the two so the
    /// interface can say which. Whether the intent changes the batch's
    /// final result depends on whether any member remains to be started
    /// — a cancel too late for the last member changes nothing, so that
    /// batch completed.
    pub(crate) fn request_cancel(&self) -> BatchCancel {
        let mut state = self.state();
        state.stopping = true;
        match state.current.clone() {
            Some(control) => {
                drop(state);
                BatchCancel::Member(control.request_cancel())
            }
            None => BatchCancel::BetweenMembers,
        }
    }
}

/// What `c` reached in a batch: a member, with that member's answer, or
/// the gap between two.
#[cfg(feature = "tui")]
pub(crate) enum BatchCancel {
    Member(CancelOutcome),
    BetweenMembers,
}

/// How the batch itself ended.
///
/// Separate from any member's outcome, because the two answer
/// different questions. A member that fails or is refused does not
/// end the batch; a cancel stops the plan whether or not the member
/// in flight noticed in time. And a plan that ran to its last member
/// completed, even if `c` arrived too late to stop that member —
/// there was nothing left to stop.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum InstallBatchEnd {
    Completed,
    Cancelled,
}

/// How one member of a batch ended.
///
/// `Refused` is not `Failed`, and the difference is the person's to
/// see: a crate whose build succeeded and whose placement could not be
/// authorized did not fail to build. Saying it did would send someone
/// looking at a compiler error that does not exist.
#[cfg(feature = "tui")]
pub(crate) enum MemberOutcome {
    Installed,
    /// The plan said one thing about this entry and the manifest now
    /// says another: not built, and not a failure either — the person
    /// confirmed something that is no longer there to do.
    Skipped(String),
    Failed(anyhow::Error),
    Refused(String),
    Cancelled,
}

/// A privileged step was reached and could not be authorized.
///
/// Typed so the classification survives the trip out of
/// `install_and_commit`, and carrying both what was being authorized
/// and why it was not: one bit across the channel made every refusal
/// look like every other, and a migration's retirement was being told
/// it had failed to place something.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) struct AuthorizationRefused {
    pub purpose: privileged::AuthPurpose,
    pub reason: String,
}

#[cfg(feature = "tui")]
impl std::fmt::Display for AuthorizationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.purpose {
            privileged::AuthPurpose::Placement => "placement",
            privileged::AuthPurpose::Retirement => "retiring the source installation",
            privileged::AuthPurpose::Mutation => "changing what is installed",
        };
        write!(f, "{what} could not be authorized: {}", self.reason)
    }
}

#[cfg(feature = "tui")]
impl std::error::Error for AuthorizationRefused {}

/// What a batch worker tells the interface as it goes.
#[cfg(feature = "tui")]
pub(crate) enum BatchStep<'a> {
    /// About to build this member. Which number it is, the caller
    /// already knows: it counts the starts.
    Started { name: &'a str },
    /// That member is done, classified where its error was still
    /// typed.
    Finished {
        name: &'a str,
        outcome: MemberOutcome,
    },
}

/// `install a b c` for the captured frontend: one operation, one lock,
/// one manifest.
///
/// This is `cmd_install`'s loop, reported instead of printed, and it is
/// a loop *here* rather than a queue in the interface for the sake of
/// the contract: the exclusive lock is taken before the manifest is
/// read and held until the last member commits, and the pin refusal and
/// the duplicate warnings are computed for the entire plan before the
/// first build starts. Three calls to `tui_install_one` would be three
/// operations wearing one name — a plan checked against a manifest that
/// could move under it, and a pinned crate discovered ten minutes in.
///
/// A member that does not install is reported and the loop carries on;
/// only a cancel ends it early. The batch is a plural, not a molecule:
/// the unit of atomicity is one crate, and a batch chooses an
/// iteration policy, never the unit.
///
/// Members are reported through `step`; the caller classifies, counts
/// and draws. Each member gets a fresh `BuildControl`, handed to the
/// batch's control so `c` reaches the build actually running.
#[cfg(feature = "tui")]
pub(crate) fn tui_install_batch(
    prefix: &Path,
    specs: &[InstallSpec],
    locked: bool,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BatchControl,
    step: &mut dyn FnMut(BatchStep),
) -> Result<InstallBatchEnd> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    // The whole plan, before the first build: a pin refusal, a typo or
    // a no-change member must not arrive after ten minutes of
    // compiling. Same gates as `cmd_install`: every plain install of a
    // pinned crate is refused, versioned or not, and a member whose
    // artifact specification does not change is redirected to
    // `--reinstall`.
    let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    refuse_pinned(prefix, &manifest, &names)?;
    for spec in specs {
        refuse_diagonal(prefix, &manifest, spec, locked)?;
    }
    for w in duplicate_install_warnings(prefix, &manifest, specs.iter().map(|s| s.name.as_str())) {
        on_line(LineKind::Warning, &w);
    }
    let mut end = InstallBatchEnd::Completed;
    for spec in specs {
        let member = std::sync::Arc::new(BuildControl::new());
        // Asking and publishing as one step: see `try_begin_member`.
        // If the member cannot begin here, the batch was already
        // cancelled with members still to go — a cancelled batch, not
        // a completed one.
        if !control.try_begin_member(&member) {
            end = InstallBatchEnd::Cancelled;
            break;
        }
        step(BatchStep::Started { name: &spec.name });
        let mut frontend = Frontend::Captured {
            on_line,
            before_placement,
            control: &member,
            checkpoint: None,
        };
        let result = install_and_commit(
            prefix,
            &cache,
            &mut manifest,
            &spec.name,
            spec.version.as_ref(),
            locked,
            PinPolicy::Infer,
            ShadowReport::OnCommit,
            &mut frontend,
        );
        control.finish_member();
        // Classified here, where the error is still typed: a cancel and
        // a placement refusal are not build failures, and the person
        // must not be sent looking for a compiler error that does not
        // exist.
        let outcome = match result {
            Ok(_) => MemberOutcome::Installed,
            Err(e) if e.downcast_ref::<BuildCancelled>().is_some() => MemberOutcome::Cancelled,
            // Only a refusal at *this* member's placement door makes it
            // "built, but not placed"; anything else is a failure.
            Err(e) => match e.downcast::<AuthorizationRefused>() {
                Ok(refused) if matches!(refused.purpose, privileged::AuthPurpose::Placement) => {
                    MemberOutcome::Refused(refused.reason)
                }
                Ok(other) => MemberOutcome::Failed(anyhow::Error::new(other)),
                Err(e) => MemberOutcome::Failed(e),
            },
        };
        if matches!(outcome, MemberOutcome::Cancelled) {
            end = InstallBatchEnd::Cancelled;
        }
        step(BatchStep::Finished {
            name: &spec.name,
            outcome,
        });
        // A failure or a refusal is about the member; a cancel is
        // about the remainder. What B's error says about C is nothing,
        // and the one voice that speaks for the rest of the plan is
        // the person cancelling it — the policy the sweeps have used
        // from the start, and the named list follows since 0.17.0.
        if matches!(end, InstallBatchEnd::Cancelled) {
            break;
        }
    }
    // Note what is *not* here: a final look at `control.cancelled()`. A
    // plan that reached its last member completed, even if `c` came too
    // late to stop that member — there was nothing after it to stop,
    // and calling that a cancelled batch would report a stop that
    // changed nothing.
    Ok(end)
}

/// One member of `U`'s plan: the entry as it was read, and what the
/// registry said about it.
#[cfg(feature = "tui")]
pub(crate) struct PlannedUpdate {
    pub name: String,
    pub current: String,
    pub latest: Version,
}

/// `U`'s check: what the registry says about every entry here.
///
/// Every entry is asked about, pinned ones included: a pin says do not
/// move this crate, not do not tell me about it, and a check that
/// skipped them would blank their status on every sweep. Which of them
/// may then be updated is the caller's question, not this one's. The
/// shared
/// lock is released before the network work — a check over a whole
/// prefix is the longest thing this tool does without building — and
/// the cancel token reaches `check_versions`, so a `c` stops the
/// remaining requests rather than only discarding their answers.
#[cfg(feature = "tui")]
pub(crate) fn tui_update_sweep_check(
    prefix: &Path,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Option<Vec<Checked>>> {
    let manifest = {
        let _lock = StateLock::acquire_with(
            prefix,
            &Mode::Shared,
            privileged::Policy::for_prefix(prefix).screen_owned(),
            &mut |_| {},
        )?;
        Manifest::load(prefix)?
    };
    let checked = check_versions(&manifest.crates, should_cancel)?;
    if let Some(checked) = &checked {
        // The report module's contract has promised this from the start: `U` cannot
        // ask the registry about a whole prefix and then pretend it did
        // not. A completed sweep is a full check and re-stamps the
        // baseline like `checkupdate`. A cancelled one records nothing
        // — a deliberate exception to knowledge-at-the-query: the sweep
        // answers as one statement or not at all. Salvaging the crates
        // it did ask about would be legal under the model; it is a
        // future decision, not an accident of this one.
        let stored = Report::new(prefix, checked.clone())
            .and_then(|report| cache_dir().and_then(|cache| report.store_full(&cache)));
        if let Err(e) = stored {
            eprintln!("warning: could not save update report: {e:#}");
        }
    }
    Ok(checked)
}

/// `update --all` for the captured frontend: the confirmed plan,
/// applied under one lock.
///
/// `apply_updates`' loop, reported instead of printed. Each member is
/// revalidated against the plan the person read — the version it had
/// then, and that it is still unpinned — because a plan about an entry
/// that has moved is a plan about something else. What is installed is
/// the newest, not the plan's `latest`: the plan said what the
/// registry held when it was read, and the person asked for the newest.
///
/// A failure does not end the sweep; a cancel does.
#[cfg(feature = "tui")]
pub(crate) fn tui_update_sweep(
    prefix: &Path,
    planned: &[PlannedUpdate],
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BatchControl,
    step: &mut dyn FnMut(BatchStep),
) -> Result<InstallBatchEnd> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    let mut end = InstallBatchEnd::Completed;
    for planned in planned {
        let name = planned.name.as_str();
        let member = std::sync::Arc::new(BuildControl::new());
        if !control.try_begin_member(&member) {
            end = InstallBatchEnd::Cancelled;
            break;
        }
        step(BatchStep::Started { name });
        let entry = manifest.crates.get(name);
        let unchanged = entry.is_some_and(|e| e.version == planned.current && !e.pinned);
        let Some(locked) = entry.map(|e| e.locked).filter(|_| unchanged) else {
            control.finish_member();
            step(BatchStep::Finished {
                name,
                outcome: MemberOutcome::Skipped("changed since the plan was confirmed".to_owned()),
            });
            continue;
        };
        let mut frontend = Frontend::Captured {
            on_line,
            before_placement,
            control: &member,
            checkpoint: None,
        };
        let result = install_and_commit(
            prefix,
            &cache,
            &mut manifest,
            name,
            None,
            locked,
            PinPolicy::Infer,
            ShadowReport::OnCommit,
            &mut frontend,
        );
        control.finish_member();
        let outcome = match result {
            Ok(_) => MemberOutcome::Installed,
            Err(e) if e.downcast_ref::<BuildCancelled>().is_some() => MemberOutcome::Cancelled,
            Err(e) => match e.downcast::<AuthorizationRefused>() {
                Ok(refused) if matches!(refused.purpose, privileged::AuthPurpose::Placement) => {
                    MemberOutcome::Refused(refused.reason)
                }
                Ok(other) => MemberOutcome::Failed(anyhow::Error::new(other)),
                Err(e) => MemberOutcome::Failed(e),
            },
        };
        let cancelled = matches!(outcome, MemberOutcome::Cancelled);
        step(BatchStep::Finished { name, outcome });
        if cancelled {
            end = InstallBatchEnd::Cancelled;
            break;
        }
    }
    Ok(end)
}

/// `T`'s plan: every managed entry, frozen as it stands.
///
/// Read under a shared lock and released — the question that follows
/// takes as long as a person takes, and no prefix waits on that. What
/// the apply does with a plan that has since moved is its own business;
/// see `tui_reinstall_sweep`.
#[cfg(feature = "tui")]
pub(crate) fn tui_reinstall_plan(prefix: &Path) -> Result<Vec<(String, ReinstallPlan)>> {
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Shared,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |_| {},
    )?;
    let manifest = Manifest::load(prefix)?;
    manifest
        .crates
        .keys()
        .map(|name| reinstall_plan(&manifest, name).map(|plan| (name.clone(), plan)))
        .collect()
}

/// `install --reinstall --all` for the captured frontend: the confirmed
/// plan, applied under one lock.
///
/// `apply_reinstalls`' loop, reported instead of printed, and the three
/// things that make it that operation rather than a similar one. The
/// exclusive lock is taken before the manifest is read and held to the
/// last member. Each member is re-checked against the plan the person
/// confirmed — version, pin and `--locked`, the three facts the plan
/// showed — and one that has moved since is skipped rather than
/// rebuilt against a state nobody agreed to. And a failure does not
/// end the sweep: a sweep answers "how many of these survived", which
/// requires asking about all of them.
#[cfg(feature = "tui")]
pub(crate) fn tui_reinstall_sweep(
    prefix: &Path,
    planned: &[(String, ReinstallPlan)],
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BatchControl,
    step: &mut dyn FnMut(BatchStep),
) -> Result<InstallBatchEnd> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    let mut end = InstallBatchEnd::Completed;
    for (name, plan) in planned {
        let member = std::sync::Arc::new(BuildControl::new());
        if !control.try_begin_member(&member) {
            end = InstallBatchEnd::Cancelled;
            break;
        }
        step(BatchStep::Started { name });
        let unchanged = manifest.crates.get(name).is_some_and(|e| {
            e.version == plan.version.to_string()
                && e.pinned == plan.pinned
                && e.locked == plan.locked
        });
        if !unchanged {
            control.finish_member();
            step(BatchStep::Finished {
                name,
                outcome: MemberOutcome::Skipped(
                    "state changed since the reinstall was confirmed".to_owned(),
                ),
            });
            continue;
        }
        let mut frontend = Frontend::Captured {
            on_line,
            before_placement,
            control: &member,
            checkpoint: None,
        };
        let result = install_and_commit(
            prefix,
            &cache,
            &mut manifest,
            name,
            Some(&plan.version),
            plan.locked,
            PinPolicy::Exactly(plan.pinned),
            ShadowReport::OnCommit,
            &mut frontend,
        );
        control.finish_member();
        let outcome = match result {
            Ok(_) => MemberOutcome::Installed,
            Err(e) if e.downcast_ref::<BuildCancelled>().is_some() => MemberOutcome::Cancelled,
            Err(e) => match e.downcast::<AuthorizationRefused>() {
                Ok(refused) if matches!(refused.purpose, privileged::AuthPurpose::Placement) => {
                    MemberOutcome::Refused(refused.reason)
                }
                Ok(other) => MemberOutcome::Failed(anyhow::Error::new(other)),
                Err(e) => MemberOutcome::Failed(e),
            },
        };
        // A cancel ends the sweep; a failure does not. The person
        // stopped the work, or one crate stopped building — and only
        // the first is a statement about the rest.
        let cancelled = matches!(outcome, MemberOutcome::Cancelled);
        step(BatchStep::Finished { name, outcome });
        if cancelled {
            end = InstallBatchEnd::Cancelled;
            break;
        }
    }
    Ok(end)
}

/// What `u`'s lookup found, planned from the manifest rather than from
/// a row drawn earlier.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum UpdatePlan {
    UpToDate { current: String },
    Outdated { current: String, latest: Version },
}

/// `u`'s first half: read the entry, then ask the registry about it.
///
/// The premise is taken here, not in the interface. A row is whatever
/// the last check drew; between then and now another process may have
/// pinned this crate, unpinned it or updated it, and the CLI's update
/// begins by loading the manifest under a shared lock for exactly that
/// reason. The lock is released before the network call — nobody waits
/// on a prefix for the duration of an index request.
///
/// The version policy is `check_versions`', not a second copy of it:
/// yanked releases, pre-releases and "newer than current" are decided
/// in one place, and a plan that disagreed with `r` about what counts
/// as an update would be its own kind of untruth.
#[cfg(feature = "tui")]
pub(crate) fn tui_update_plan(prefix: &Path, name: &str) -> Result<UpdatePlan> {
    let entry = {
        // Screen-owned and silent: this runs on a worker under the
        // alternate screen, where a notice printed by the lock would
        // land on a terminal nobody is looking at — and would corrupt
        // the one they are. The spinner already says the job is alive.
        let _lock = StateLock::acquire_with(
            prefix,
            &Mode::Shared,
            privileged::Policy::for_prefix(prefix).screen_owned(),
            &mut |_| {},
        )?;
        let manifest = Manifest::load(prefix)?;
        let entry = manifest
            .crates
            .get(name)
            .cloned()
            .with_context(|| format!("not installed: {name}"))?;
        refuse_pinned(prefix, &manifest, std::slice::from_ref(&name.to_owned()))?;
        entry
    };
    let current = entry.version.clone();
    let checked = check_versions([(&name.to_owned(), &entry)], || false)?
        .and_then(|mut c| c.pop())
        .with_context(|| format!("no version information for `{name}`"))?;
    record_knowledge(prefix, vec![checked.clone()]);
    Ok(if checked.is_outdated() {
        UpdatePlan::Outdated {
            current,
            latest: checked.latest,
        }
    } else {
        UpdatePlan::UpToDate { current }
    })
}

/// `update NAME` for the captured frontend: the newest release, against
/// the entry the plan was made about.
///
/// The CLI's update is three steps — look up, show, apply — and the
/// third revalidates what the first two assumed, because a person read
/// a plan in between. This is that third step. `expected` is what the
/// entry said when the plan was shown; if the entry has moved since,
/// the update the person agreed to is not the update that would
/// happen, and nothing is built.
///
/// The pin is re-checked here too, and the entry's `--locked` is
/// carried: an update changes the version and nothing else about how
/// this installation is built.
#[cfg(feature = "tui")]
pub(crate) fn tui_update_one(
    prefix: &Path,
    name: &str,
    expected: &str,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BuildControl,
) -> Result<()> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    refuse_pinned(prefix, &manifest, std::slice::from_ref(&name.to_owned()))?;
    let entry = manifest
        .crates
        .get(name)
        .with_context(|| format!("not installed: {name}"))?;
    if entry.version != expected {
        bail!(
            "`{name}` is {} now, not {expected}; the plan is out of date",
            entry.version
        );
    }
    let locked = entry.locked;
    let mut frontend = Frontend::Captured {
        on_line,
        before_placement,
        control,
        checkpoint: None,
    };
    // No version, exactly as `apply_updates` does it: the plan's target
    // was what to show the person, not what to pin the build to. If a
    // newer release landed while the question was on screen, this
    // installs it — the same answer the CLI gives, and the same one the
    // person asked for, which was "the newest".
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        name,
        None,
        locked,
        PinPolicy::Infer,
        ShadowReport::OnCommit,
        &mut frontend,
    )?;
    Ok(())
}

/// `install --reinstall` for the captured frontend: the entry, read
/// under the lock that will commit it.
///
/// Same specification as the CLI's — version, pin and `--locked` from
/// the manifest — and read *here* rather than passed in from the
/// interface, because the rows are a snapshot and this lock is not.
/// The panel's request carries only a name.
#[cfg(feature = "tui")]
pub(crate) fn tui_reinstall_one(
    prefix: &Path,
    name: &str,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BuildControl,
) -> Result<()> {
    let cache = cache_dir()?;
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    let plan = reinstall_plan(&manifest, name)?;
    let mut frontend = Frontend::Captured {
        on_line,
        before_placement,
        control,
        checkpoint: None,
    };
    // `Exactly`, not `Infer`: an exact version infers a pin, and this
    // operation must return the entry it found — an unpinned crate
    // rebuilt must not come back pinned.
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        name,
        Some(&plan.version),
        plan.locked,
        PinPolicy::Exactly(plan.pinned),
        ShadowReport::OnCommit,
        &mut frontend,
    )?;
    Ok(())
}

/// One crate for the TUI, end to end: same locking, pin refusal and
/// pipeline as `cmd_install`; the exclusive lock spans build and
/// placement — serialization per prefix is a documented invariant.
#[cfg(feature = "tui")]
pub(crate) fn tui_install_one(
    prefix: &Path,
    spec: &InstallSpec,
    locked: bool,
    on_line: &mut dyn FnMut(LineKind, &str),
    before_placement: &mut dyn FnMut(&Path) -> Result<()>,
    control: &BuildControl,
) -> Result<()> {
    let cache = cache_dir()?;
    // Same contract as placement: the one sudo here runs noninteractively
    // and its human lines go through the frontend's stream.
    let _lock = StateLock::acquire_with(
        prefix,
        &Mode::Exclusive,
        privileged::Policy::for_prefix(prefix).screen_owned(),
        &mut |s| on_line(LineKind::Notice, s),
    )?;
    let mut manifest = Manifest::load(prefix)?;
    // Same gates as `cmd_install`, single-member edition.
    refuse_pinned(prefix, &manifest, std::slice::from_ref(&spec.name))?;
    refuse_diagonal(prefix, &manifest, spec, locked)?;
    let mut frontend = Frontend::Captured {
        on_line,
        before_placement,
        control,
        checkpoint: None,
    };
    // The row's [also in …] annotation already showed the state; the
    // warning still lands in the panel, so the parity with the CLI is
    // in the record, not only in the table.
    for w in duplicate_install_warnings(prefix, &manifest, std::iter::once(spec.name.as_str())) {
        frontend.warning(&w);
    }
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        &spec.name,
        spec.version.as_ref(),
        locked,
        PinPolicy::Infer,
        ShadowReport::OnCommit,
        &mut frontend,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildPhase;
    use crate::manifest::Entry;
    use crate::test_support::{
        any_crate_fake, failing_fake, seeded_prefix, staging_fake, versioned_fake,
    };
    use std::fs;

    /// And the worker behind `t` finishes what the gate now lets
    /// through: a pinned, locked entry comes back pinned and locked, at
    /// the same version, with the binary rebuilt.
    #[cfg(feature = "tui")]
    #[test]
    fn a_captured_reinstall_returns_the_entry_it_found() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-reinstall-worker");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, true);
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let control = BuildControl::new();

        tui_reinstall_one(
            &prefix,
            "okcrate",
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .unwrap();

        let entry = Manifest::load(&prefix).unwrap().crates["okcrate"].clone();
        assert_eq!(
            entry.version, "0.1.0",
            "the entry's version, not the newest"
        );
        assert!(entry.pinned, "a pinned crate stays pinned");
        assert!(entry.locked, "and keeps its --locked");
        assert!(
            prefix.join("bin/okcrate").exists(),
            "and was actually rebuilt"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The batch worker is one operation: one lock, one manifest, the
    /// whole plan checked before the first build. The pin refusal is
    /// the visible proof — `install foo bar` with `bar` pinned refuses
    /// without building `foo`, exactly as the CLI does, where a queue
    /// of single installs would have built it first.
    #[cfg(feature = "tui")]
    #[test]
    fn an_install_batch_checks_the_whole_plan_before_the_first_build() {
        let root = std::env::temp_dir().join("cargo-lbin-test-batch-preflight");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "pinnedcrate", false, true);
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));
        let control = BatchControl::new();
        let specs =
            InstallSpec::parse_all(&["okcrate".to_owned(), "pinnedcrate".to_owned()]).unwrap();
        let mut started: Vec<String> = Vec::new();

        let err = tui_install_batch(
            &prefix,
            &specs,
            false,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| {
                if let BatchStep::Started { name, .. } = step {
                    started.push(name.to_owned());
                }
            },
        )
        .expect_err("a pinned member refuses the plan");
        assert!(format!("{err:#}").contains("pinned"), "{err:#}");
        assert!(
            started.is_empty(),
            "and nothing was built first: {started:?}"
        );
        assert!(
            !prefix.join("bin/okcrate").exists(),
            "the unpinned member was never placed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Members run in order under that one lock; a failure is the
    /// member's, and the members behind it still run — the batch ends
    /// early only for a cancel.
    #[cfg(feature = "tui")]
    #[test]
    fn an_install_batch_carries_past_a_failure() {
        let root = std::env::temp_dir().join("cargo-lbin-test-batch-stop");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&failing_fake(&root, "brokencrate"));
        let control = BatchControl::new();
        let specs = InstallSpec::parse_all(&[
            "okcrate".to_owned(),
            "brokencrate".to_owned(),
            "lastcrate".to_owned(),
        ])
        .unwrap();
        let mut steps: Vec<String> = Vec::new();

        tui_install_batch(
            &prefix,
            &specs,
            // `true` against entries seeded `locked = false`: these
            // tests are about ordering and cancel windows, not the
            // diagonal — a declared policy change keeps the gate local
            // and offline.
            true,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| match step {
                BatchStep::Started { name, .. } => steps.push(format!("start {name}")),
                BatchStep::Finished { name, outcome } => {
                    steps.push(format!(
                        "end {name} {}",
                        matches!(outcome, MemberOutcome::Installed)
                    ));
                }
            },
        )
        .expect("the batch itself does not fail; its members do");

        assert_eq!(
            steps,
            vec![
                "start okcrate".to_owned(),
                "end okcrate true".to_owned(),
                "start brokencrate".to_owned(),
                "end brokencrate false".to_owned(),
                "start lastcrate".to_owned(),
                "end lastcrate true".to_owned(),
            ],
            "brokencrate's failure is brokencrate's; lastcrate still runs"
        );
        assert_eq!(
            Manifest::load(&prefix).unwrap().crates.len(),
            2,
            "the members that succeeded are committed; the failure is not"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A cancel between members stops the batch: the next one never
    /// starts. Driven deterministically — the cancel arrives exactly in
    /// the gap, which is the window `try_begin_member` closes.
    #[cfg(feature = "tui")]
    #[test]
    fn a_cancel_between_members_starts_nobody_else() {
        let root = std::env::temp_dir().join("cargo-lbin-test-batch-gap-cancel");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));
        let control = BatchControl::new();
        let specs =
            InstallSpec::parse_all(&["okcrate".to_owned(), "nextcrate".to_owned()]).unwrap();
        let mut started: Vec<String> = Vec::new();

        tui_install_batch(
            &prefix,
            &specs,
            // `true`: policy change keeps the diagonal gate local and
            // offline (see the carries-past-a-failure test).
            true,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| match step {
                BatchStep::Started { name, .. } => started.push(name.to_owned()),
                // The gap: the member is done, the next has not begun.
                BatchStep::Finished { .. } => {
                    control.request_cancel();
                }
            },
        )
        .unwrap();

        assert_eq!(started, vec!["okcrate".to_owned()], "nextcrate never began");
        assert!(
            !prefix.join("bin/nextcrate").exists(),
            "and nothing of it was placed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A batch cancelled before it starts attempts nobody at all.
    #[cfg(feature = "tui")]
    #[test]
    fn a_batch_cancelled_up_front_attempts_nobody() {
        let root = std::env::temp_dir().join("cargo-lbin-test-batch-precancel");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));
        let control = BatchControl::new();
        control.request_cancel();
        let specs = InstallSpec::parse_all(&["okcrate".to_owned()]).unwrap();
        let mut started = 0usize;

        let end = tui_install_batch(
            &prefix,
            &specs,
            // `true`: policy change keeps the diagonal gate local and
            // offline (see the carries-past-a-failure test).
            true,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| {
                if matches!(step, BatchStep::Started { .. }) {
                    started += 1;
                }
            },
        )
        .unwrap();
        assert!(
            matches!(end, InstallBatchEnd::Cancelled),
            "and the batch says so: a stop with members left is cancelled"
        );
        assert_eq!(started, 0, "a stopped batch begins nobody");
        let _ = fs::remove_dir_all(&root);
    }

    /// A cancel that lands while a member is building. Whether that
    /// member ends cancelled or slips through is a race with its own
    /// build — a fake cargo finishes before any checkpoint — so what is
    /// asserted is what is guaranteed: the batch ends cancelled and
    /// nobody after it starts. The V1 bug was exactly that guarantee,
    /// missing: the flag was set and nobody read it.
    #[cfg(feature = "tui")]
    #[test]
    fn a_cancel_while_a_member_builds_stops_the_batch() {
        let root = std::env::temp_dir().join("cargo-lbin-test-batch-live-cancel");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));
        let control = BatchControl::new();
        let specs =
            InstallSpec::parse_all(&["okcrate".to_owned(), "nextcrate".to_owned()]).unwrap();
        let mut started: Vec<String> = Vec::new();
        let mut outcomes: Vec<String> = Vec::new();
        let mut asked = false;

        let end = tui_install_batch(
            &prefix,
            &specs,
            // `true`: policy change keeps the diagonal gate local and
            // offline (see the carries-past-a-failure test).
            true,
            // The cancel arrives while this member is building, once:
            // pressing `c` repeatedly is a different test, and would
            // escalate to SIGKILL instead of characterizing the first
            // press.
            &mut |_, _| {
                if !asked {
                    asked = true;
                    control.request_cancel();
                }
            },
            &mut |_| Ok(()),
            &control,
            &mut |step| match step {
                BatchStep::Started { name, .. } => started.push(name.to_owned()),
                BatchStep::Finished { outcome, .. } => outcomes.push(
                    match outcome {
                        MemberOutcome::Installed => "installed",
                        MemberOutcome::Skipped(_) => "skipped",
                        MemberOutcome::Failed(_) => "failed",
                        MemberOutcome::Refused(_) => "refused",
                        MemberOutcome::Cancelled => "cancelled",
                    }
                    .to_owned(),
                ),
            },
        )
        .unwrap();

        assert_eq!(started, vec!["okcrate".to_owned()], "nextcrate never began");
        assert_eq!(outcomes.len(), 1, "one member reported: {outcomes:?}");
        assert!(
            matches!(outcomes[0].as_str(), "installed" | "cancelled"),
            "and it either finished or was stopped, nothing else: {outcomes:?}"
        );
        assert!(
            matches!(end, InstallBatchEnd::Cancelled),
            "and the batch's own end carries it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The plan `u` showed is checked where it can be true: under the
    /// lock, against the entry it was made about. A crate that moved
    /// while the question was on screen is not the crate the person
    /// agreed to update.
    #[cfg(feature = "tui")]
    #[test]
    fn a_captured_update_refuses_a_plan_the_manifest_outgrew() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-update-stale");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, false);
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));
        let control = BuildControl::new();

        // The entry says 0.1.0; a plan made about 0.0.9 is out of date.
        let err = tui_update_one(
            &prefix,
            "okcrate",
            "0.0.9",
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .expect_err("the premise no longer holds");
        let text = format!("{err:#}");
        assert!(
            text.contains("is 0.1.0 now") && text.contains("out of date"),
            "{text}"
        );
        assert_eq!(
            Manifest::load(&prefix).unwrap().crates["okcrate"].version,
            "0.1.0",
            "and nothing was built"
        );

        // With the premise intact it updates, keeps `--locked` and
        // acquires no pin.
        tui_update_one(
            &prefix,
            "okcrate",
            "0.1.0",
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .unwrap();
        let entry = Manifest::load(&prefix).unwrap().crates["okcrate"].clone();
        assert_eq!(entry.version, "0.2.0");
        assert!(entry.locked, "the entry's build policy is carried");
        assert!(!entry.pinned, "an update is not a pin");
        let _ = fs::remove_dir_all(&root);
    }

    /// The plan is read from the manifest, not from a row: a pin set
    /// since the list was drawn still refuses, and a version the list
    /// has not caught up with is still the one planned against.
    #[cfg(feature = "tui")]
    #[test]
    fn an_update_plan_reads_the_manifest_not_the_screen() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-update-plan-source");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);

        // Pinned after the row was drawn: the plan refuses without
        // asking the registry anything.
        let mut m = Manifest::load(&prefix).unwrap();
        m.crates.get_mut("okcrate").unwrap().pinned = true;
        m.store(&prefix).unwrap();
        let err = tui_update_plan(&prefix, "okcrate").expect_err("a pin refuses an update");
        assert!(format!("{err:#}").contains("pinned"), "{err:#}");

        // And a crate this prefix does not manage is named as such,
        // rather than looked up.
        let err = tui_update_plan(&prefix, "nosuchcrate").expect_err("nothing to update");
        assert!(format!("{err:#}").contains("not installed"), "{err:#}");
        let _ = fs::remove_dir_all(&root);
    }

    /// The sweep re-checks each member against the plan the person
    /// confirmed, and a failure does not end it — a sweep answers "how
    /// many of these survived", which needs asking about all of them.
    #[cfg(feature = "tui")]
    #[test]
    fn a_reinstall_sweep_skips_what_moved_and_carries_on_past_failure() {
        let root = std::env::temp_dir().join("cargo-lbin-test-sweep-apply");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        {
            let mut m = Manifest::load(&prefix).unwrap();
            for (name, version) in [("movedcrate", "0.1.0"), ("brokencrate", "0.1.0")] {
                m.crates.insert(
                    name.to_owned(),
                    Entry {
                        version: version.to_owned(),
                        bins: vec![name.to_owned()],
                        locked: false,
                        pinned: false,
                        built_with_rustc: None,
                    },
                );
            }
            m.store(&prefix).unwrap();
        }
        let planned = tui_reinstall_plan(&prefix).unwrap();
        assert_eq!(planned.len(), 3, "the plan is the prefix");

        // Between the plan and the apply, one entry moves.
        let mut m = Manifest::load(&prefix).unwrap();
        m.crates.get_mut("movedcrate").unwrap().pinned = true;
        m.store(&prefix).unwrap();

        let _fake = crate::stage::FakeCargo::install(&failing_fake(&root, "brokencrate"));
        let control = BatchControl::new();
        let mut seen: Vec<String> = Vec::new();
        tui_reinstall_sweep(
            &prefix,
            &planned,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| {
                if let BatchStep::Finished { name, outcome } = step {
                    seen.push(format!(
                        "{name} {}",
                        match outcome {
                            MemberOutcome::Installed => "installed",
                            MemberOutcome::Skipped(_) => "skipped",
                            MemberOutcome::Failed(_) => "failed",
                            MemberOutcome::Refused(_) => "refused",
                            MemberOutcome::Cancelled => "cancelled",
                        }
                    ));
                }
            },
        )
        .unwrap();

        assert_eq!(
            seen,
            vec![
                "brokencrate failed".to_owned(),
                "movedcrate skipped".to_owned(),
                "okcrate installed".to_owned(),
            ],
            "every member was asked about, in the plan's order"
        );
        assert!(
            Manifest::load(&prefix).unwrap().crates["movedcrate"].pinned,
            "and the entry that moved was left exactly as it was found"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The sweep's apply re-checks each member against the plan the
    /// person read: an entry updated elsewhere, or pinned since, is
    /// skipped rather than updated against a premise that no longer
    /// holds. And a failure does not end it.
    #[cfg(feature = "tui")]
    #[test]
    fn an_update_sweep_skips_what_moved_and_carries_on() {
        let root = std::env::temp_dir().join("cargo-lbin-test-update-sweep");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        {
            let mut m = Manifest::load(&prefix).unwrap();
            for name in ["pinnedsince", "movedcrate"] {
                m.crates.insert(
                    name.to_owned(),
                    Entry {
                        version: "0.1.0".to_owned(),
                        bins: vec![name.to_owned()],
                        locked: false,
                        pinned: false,
                        built_with_rustc: None,
                    },
                );
            }
            m.store(&prefix).unwrap();
        }
        // The plan, as the person would have read it.
        let planned: Vec<PlannedUpdate> = ["okcrate", "pinnedsince", "movedcrate"]
            .into_iter()
            .map(|name| PlannedUpdate {
                name: name.to_owned(),
                current: "0.1.0".to_owned(),
                latest: Version::parse("0.2.0").unwrap(),
            })
            .collect();
        // Between the plan and the apply: one pinned, one updated.
        let mut m = Manifest::load(&prefix).unwrap();
        m.crates.get_mut("pinnedsince").unwrap().pinned = true;
        m.crates.get_mut("movedcrate").unwrap().version = "0.3.0".to_owned();
        m.store(&prefix).unwrap();

        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));
        let control = BatchControl::new();
        let mut seen: Vec<String> = Vec::new();
        tui_update_sweep(
            &prefix,
            &planned,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
            &mut |step| {
                if let BatchStep::Finished { name, outcome } = step {
                    seen.push(format!(
                        "{name} {}",
                        match outcome {
                            MemberOutcome::Installed => "updated",
                            MemberOutcome::Skipped(_) => "skipped",
                            MemberOutcome::Failed(_) => "failed",
                            MemberOutcome::Refused(_) => "refused",
                            MemberOutcome::Cancelled => "cancelled",
                        }
                    ));
                }
            },
        )
        .unwrap();

        assert_eq!(
            seen,
            vec![
                "okcrate updated".to_owned(),
                "pinnedsince skipped".to_owned(),
                "movedcrate skipped".to_owned(),
            ],
            "every member was asked about, in the plan's order"
        );
        let after = Manifest::load(&prefix).unwrap();
        assert_eq!(
            after.crates["okcrate"].version, "0.2.0",
            "the newest, built"
        );
        assert!(after.crates["pinnedsince"].pinned, "the pin was honoured");
        assert_eq!(
            after.crates["movedcrate"].version, "0.3.0",
            "and the entry that moved was left as it was found"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The statement `downgrade` makes is checked where it can be true:
    /// under the same exclusive lock as the mutation. The rows the
    /// person chose from are a snapshot, so a crate moved or removed in
    /// between must stop the build — otherwise a command called
    /// downgrade could upgrade, or resurrect what somebody just
    /// removed.
    #[cfg(feature = "tui")]
    #[test]
    fn a_tui_downgrade_refuses_a_premise_the_manifest_no_longer_holds() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-premise");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));
        let target = Version::parse("0.0.9").unwrap();
        let control = BuildControl::new();

        // The manifest says 0.1.0; a choice made against 0.3.0 is a
        // choice about a world that is not there.
        let err = tui_downgrade_one(
            &prefix,
            "okcrate",
            "0.3.0",
            &target,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .expect_err("a stale premise builds nothing");
        assert!(format!("{err:#}").contains("changed from"), "{err:#}");
        assert_eq!(
            Manifest::load(&prefix).unwrap().crates["okcrate"].version,
            "0.1.0",
            "and the prefix is untouched"
        );

        // A crate removed while the list sat open is the same answer.
        let gone = seeded_prefix(&root, "empty", "okcrate", false, false);
        {
            let mut m = Manifest::load(&gone).unwrap();
            m.crates.remove("okcrate");
            m.store(&gone).unwrap();
        }
        let err = tui_downgrade_one(
            &gone,
            "okcrate",
            "0.1.0",
            &target,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .expect_err("a removed crate is not resurrected");
        assert!(format!("{err:#}").contains("was removed"), "{err:#}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A downgrade lands pinned at the chosen version — what
    /// `install NAME@VERSION` does, and why this path reuses it. The
    /// entry starts unpinned, since the gate makes a pinned start
    /// unreachable. `locked` comes from the fresh read, not from the
    /// row the person was looking at.
    #[cfg(feature = "tui")]
    #[test]
    fn a_tui_downgrade_repins_at_the_chosen_version() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-pin");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, false);
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));
        let target = Version::parse("0.0.9").unwrap();
        let control = BuildControl::new();

        tui_downgrade_one(
            &prefix,
            "okcrate",
            "0.1.0",
            &target,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .unwrap();
        let entry = Manifest::load(&prefix).unwrap().crates["okcrate"].clone();
        assert_eq!(entry.version, "0.0.9", "the chosen version landed");
        assert!(
            entry.pinned,
            "the chosen version lands pinned, as `install NAME@VERSION` pins"
        );
        assert!(entry.locked, "and its --locked setting is carried over");
        let _ = fs::remove_dir_all(&root);
    }

    /// And the other side of that gate, mid-flight: a pin set while
    /// the list was open counts as changed state, so the apply refuses
    /// rather than override the newer statement.
    #[cfg(feature = "tui")]
    #[test]
    fn a_tui_downgrade_refuses_a_pin_set_meanwhile() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-pin-race");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, true);
        let target = Version::parse("0.0.9").unwrap();
        let control = BuildControl::new();
        let err = tui_downgrade_one(
            &prefix,
            "okcrate",
            "0.1.0",
            &target,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .expect_err("a pin set while choosing is changed state");
        assert!(format!("{err:#}").contains("pinned"), "{err:#}");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_captured_migration_moves_and_reports_data() {
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-captured");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", true, true);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&staging_fake(&root, "okcrate"));

        let control = BuildControl::new();
        let mut lines = 0usize;
        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let outcome = tui_migrate_one(
            &source,
            &dest,
            "okcrate",
            &snap,
            &mut |_, _| lines += 1,
            &mut |_| Ok(()),
            &control,
        )
        .unwrap();
        assert!(matches!(
            &outcome,
            MigrateOutcome::Moved {
                already_retired: false,
                version,
            } if version.to_string() == "0.1.0"
        ));
        assert!(lines > 0, "the captured frontend streamed cargo's lines");
        let entry = &Manifest::load(&dest).unwrap().crates["okcrate"];
        assert!(entry.pinned && entry.locked, "both bits travelled");
        assert!(
            !Manifest::load(&source)
                .unwrap()
                .crates
                .contains_key("okcrate"),
            "retired"
        );
        assert!(
            matches!(control.phase(), BuildPhase::Placement),
            "the migration crossed the same door an install does"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_frozen_plan_rejects_a_source_that_moved_on() {
        // The review scenario: confirm the row at 0.1.0, another process
        // updates first; the frozen snapshot travels and the checkpoint
        // rejects. The frozen plan guards the *source* — an unpinned
        // confirmation legitimately installs a newer latest at the
        // destination.
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-frozen");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&staging_fake(&root, "okcrate"));

        // Frozen from the state the person saw…
        let snap = MigrationSnapshot::from_parts(
            "okcrate",
            "0.1.0",
            vec!["okcrate".into()],
            false,
            false,
            None,
        )
        .unwrap();
        // …then the world moves on before the worker starts.
        let mut m = Manifest::load(&source).unwrap();
        m.crates.get_mut("okcrate").unwrap().version = "0.2.0".into();
        m.store(&source).unwrap();

        let control = BuildControl::new();
        let err = tui_migrate_one(
            &source,
            &dest,
            "okcrate",
            &snap,
            &mut |_, _| {},
            &mut |_| Ok(()),
            &control,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("aborting before the destination commits"),
            "the checkpoint rejected the stale plan: {err:#}"
        );
        assert!(
            Manifest::load(&dest).unwrap().crates.is_empty(),
            "nothing was migrated under a plan nobody confirmed"
        );
        assert_eq!(
            Manifest::load(&source).unwrap().crates["okcrate"].version,
            "0.2.0",
            "the source keeps its newer truth"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_cancelled_migration_touches_neither_prefix() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-cancel");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();

        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let control = std::sync::Arc::new(BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let source_w = source.clone();
        let dest_w = dest.clone();
        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let worker = std::thread::spawn(move || {
            tui_migrate_one(
                &source_w,
                &dest_w,
                "okcrate",
                &snap,
                &mut |_, _| {},
                &mut |_| Ok(()),
                &worker_control,
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(matches!(control.request_cancel(), CancelOutcome::Accepted));
        let err = worker.join().unwrap().expect_err("cancelled");
        assert!(
            err.downcast_ref::<BuildCancelled>().is_some(),
            "the migration's cancel is the same typed cancellation: {err:#}"
        );
        assert!(
            Manifest::load(&dest).unwrap().crates.is_empty(),
            "the destination committed nothing"
        );
        assert!(
            Manifest::load(&source)
                .unwrap()
                .crates
                .contains_key("okcrate"),
            "the source is untouched — a cancelled migration is a no-op"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
