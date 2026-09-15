//! Interactive front end over the same commands the CLI runs.
//!
//! The TUI adds no logic of its own: `r` is `checkupdate`, `u`/`U` are
//! `update`, `i` is `install`, `x` is `remove`, `s` is `search`, `v` is
//! `verify`. Nothing happens unless a key asks for it — no polling, no
//! refresh or network access on start.
//!
//! Two ways of running a command. Some take over the real terminal
//! (`u`/`U` updates, multi-crate installs, and a downgrade only where
//! captured placement cannot run): the TUI steps
//! aside, runs the command as the CLI would, waits for Enter, and comes
//! back. Others run in place: a single-crate install or migrate builds
//! behind the framed panel with its cancel door and sudo roundtrip, and
//! a removal or pin flip that needs no escalation never leaves the
//! screen. The `Terminal` is created once and kept across handoffs —
//! `ratatui::try_init()` stacks a panic hook per call. `checkupdate`,
//! `search`, `v` and the version lookup behind `D` run on a one-shot
//! thread while the list stays navigable.
//!
//! A manifest the validated loader refuses does not keep the TUI out:
//! the session starts degraded — empty list, mutating actions refused,
//! `r` repurposed as a plain reload retry — because `v`, the key that
//! explains what is wrong, must not vanish exactly when something is.

mod ui;

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use semver::Version;

use crate::api;
use crate::lock::{Mode, StateLock};
use crate::manifest::{Entry, Manifest};
use crate::progress;
use crate::report::{Checked, Report, Status};
use crate::validate::InstallSpec;

/// How often the input poll wakes up to look for finished background work.
const TICK: Duration = Duration::from_millis(100);

/// Grace an accepted cancel gets before the run loop escalates to
/// SIGKILL: enough for a well-behaved tree to fold after SIGTERM,
/// short enough not to hold the person hostage.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// One installed crate as the list shows it.
#[derive(Clone)]
pub struct Row {
    pub name: String,
    pub version: String,
    pub bins: Vec<String>,
    pub locked: bool,
    pub pinned: bool,
    /// The other prefixes carrying this crate, as facts rather than as
    /// presentation: the list suffix renders them through the shared
    /// `prefixes::describe` the CLI listing uses, the details panel
    /// renders its own line under the same sanitize policy — and
    /// neither surface parses a string built for the other.
    pub also: Vec<crate::prefixes::AlsoIn>,
    pub status: RowStatus,
}

/// What the last `checkupdate` says about a row — three states, the
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

    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Updates,
            Filter::Updates => Filter::Pinned,
            Filter::Pinned => Filter::All,
        }
    }

    fn prev(self) -> Self {
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
fn admits(filter: Filter, row: &Row) -> bool {
    match filter {
        Filter::All => true,
        Filter::Updates => matches!(row.status, RowStatus::Outdated(_)) && !row.pinned,
        Filter::Pinned => row.pinned,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    /// Succeeded, but something around it did not — yellow: neither
    /// routine nor a failure.
    Warning,
    Error,
}

pub struct Message {
    pub text: String,
    pub kind: MessageKind,
}

/// What the footer input line is collecting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputPurpose {
    Install,
    Search,
}

pub struct Input {
    pub purpose: InputPurpose,
    pub buffer: String,
}

/// A destructive action waiting for a `y`.
pub struct Confirm {
    pub prompt: String,
    action: OnConfirm,
}

impl Confirm {
    /// The one funnel: the prompt is Span-bound, and both builders
    /// interpolate strings the TUI does not control.
    fn new(prompt: &str, action: OnConfirm) -> Self {
        Self {
            prompt: crate::text::sanitize(prompt),
            action,
        }
    }
}

/// What a confirmed `y` triggers. A removal decides its shape only at
/// the `y`: privilege is a property of the world, checked fresh.
enum OnConfirm {
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
    /// `x`: the shape (in place or handoff) is decided fresh at the `y` —
    /// the world may move while the prompt is open.
    Remove { name: String },
    /// `migrate --all`: the whole plan frozen at the keypress, one
    /// snapshot per row, complete or not at all.
    MigrateAll {
        dest: PathBuf,
        plan: Vec<PendingMigrate>,
    },
}

/// Commands that take over the terminal; queued by key handlers, run
/// by the event loop after the announcing frame.
#[derive(Clone)]
enum PendingAction {
    Update(String),
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

/// The render boundary in one function: every pipeline line becomes a
/// `BuildMsg` here — sanitized, because past this point it is
/// Span-bound and paths may hold ESC. A future producer cannot route
/// around it.
fn build_msg(kind: crate::LineKind, line: &str) -> BuildMsg {
    let line = crate::text::sanitize(line);
    match kind {
        crate::LineKind::Cargo => BuildMsg::Cargo(line),
        crate::LineKind::Notice => BuildMsg::Notice(line),
        crate::LineKind::Warning => BuildMsg::Warning(line),
    }
}

/// Messages a build worker streams to the UI thread; the protocol
/// mirrors the pipeline's classification.
enum BuildMsg {
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
    NeedAuth(PathBuf),
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
enum BuildOutcome {
    Success,
    Migrated(Version),
    Cancelled,
    CompletedWithWarning(String),
    Failed(anyhow::Error),
}

/// What the job builds toward; the UI composes result lines from
/// this — the worker reports outcomes, never prose.
enum BuildKind {
    Install,
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
    fn verb(&self) -> &'static str {
        match self {
            BuildKind::Install => "install",
            BuildKind::Downgrade => "downgrade",
            // Tuple variant, tuple pattern: a pattern should not lie
            // about the shape.
            BuildKind::Migrate(_) => "migrate",
        }
    }
}

/// A confirmed migration on its way to `start_migrate`; the
/// destination preflight may need the terminal.
struct PendingMigrate {
    name: String,
    version: String,
    dest: PathBuf,
    snap: crate::MigrationSnapshot,
}

/// A confirmed `M`: `migrate --all` as a queue of the very single
/// migrations `m` runs — each its own unit, preflight and cancel door.
/// The batch owns the tally and one final summary: the CLI's
/// "reported, and the batch moves on", in the TUI's shape.
struct MigrateBatch {
    dest: PathBuf,
    queue: std::collections::VecDeque<PendingMigrate>,
    total: usize,
    moved: usize,
    /// `(name, lines)` — destination committed, source not retired: the
    /// Incomplete reason plus that member's warnings.
    warned: Vec<(String, Vec<String>)>,
    /// `(name, lines)` — the same diagnostics a single failure panel
    /// gets; the already-installed refusal counts as a shortfall, as on
    /// the CLI.
    failed: Vec<(String, Vec<String>)>,
    /// `(name, warnings)` — fully migrated members whose build spoke
    /// warnings: the summary must not launder them.
    noticed: Vec<(String, Vec<String>)>,
    /// The last mid-batch reload failure, resurfaced at the summary.
    reload_error: Option<String>,
}

/// Did `start_migrate` actually start a worker? A refusal carries its
/// reason — the summary must not overwrite it.
enum StartOutcome {
    Started,
    Refused(String),
}

/// What the escalation preflight found; see `preflight_escalation`.
enum Preflight {
    /// No escalation ahead, or credentials validated and warm.
    Ready,
    /// sudo validated but does not cache credentials: a captured
    /// `sudo -n` would be asked a question it cannot voice.
    NoCache,
    /// Something failed; the message is returned, not swallowed — the
    /// caller decides where it must survive.
    Reported(String),
}

/// Where a migration is headed.
struct MigrateTarget {
    version: String,
    dest: PathBuf,
}

/// A build's sticky report, held until dismissed: a failure carries
/// the tail and log path, a success its warnings — either would be
/// wasted by a message that scrolls away.
pub struct BuildReport {
    pub title: String,
    pub lines: Vec<String>,
    pub failed: bool,
}

/// Output lines kept for the failure panel; the full text is in the
/// log file.
const BUILD_TAIL: usize = 40;

/// A one-shot's whole cancel model: a shared flag. Running, cancel
/// requested (flag set, slot still held until the worker returns —
/// single-flight stands), finished or cancelled. No grace, no
/// escalation, no "too late": those belong to builds, which have a
/// child process and a placement door — a one-shot has neither.
type CancelFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

fn cancel_flag() -> CancelFlag {
    CancelFlag::default()
}

impl App {
    /// Idempotent on purpose: a second press changes nothing — there is
    /// no escalation to offer.
    fn request_oneshot_cancel(cancel: &CancelFlag) {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The one meaning of c over a running job. Builds escalate through
    /// `BuildControl`; one-shots set their flag and nothing more; with no
    /// job, silence — nothing else could be meant.
    fn cancel_pressed(&mut self) {
        match &self.job {
            Some(Job::Build { name, control, .. }) => {
                let name = name.clone();
                match control.request_cancel() {
                    crate::CancelOutcome::Accepted => {
                        self.arm_cancel_grace();
                        self.info(&format!("cancelling {name}… (c again sends SIGKILL)"));
                    }
                    crate::CancelOutcome::Killed => {
                        self.warn(&format!("SIGKILL sent to the {name} build"));
                    }
                    crate::CancelOutcome::AlreadyStopping => {
                        self.info("the build is already stopping");
                    }
                    crate::CancelOutcome::TooLate => {
                        self.info("placement already started; too late to cancel");
                    }
                }
            }
            // One-shots: a flag, not the build's machinery — no
            // grace, no escalation, no "too late". The slot stays
            // held until the worker returns: the check worker
            // exits between index requests, search is bounded by
            // the agent's timeouts, and verify may sit in the
            // shared-lock wait — none of which this door
            // interrupts; results arriving after the flag are
            // discarded.
            Some(Job::Check { cancel, .. }) => {
                Self::request_oneshot_cancel(cancel);
                self.info("cancelling the update check…");
            }
            Some(Job::Verify { cancel, .. }) => {
                Self::request_oneshot_cancel(cancel);
                self.info("verify: cancel requested — the result will be discarded");
            }
            Some(Job::Search { query, cancel, .. }) => {
                let note = format!("search `{query}`: cancel requested…");
                Self::request_oneshot_cancel(cancel);
                self.info(&note);
            }
            Some(Job::Downgrade { name, cancel, .. }) => {
                let note = format!("version lookup for `{name}`: cancel requested…");
                Self::request_oneshot_cancel(cancel);
                self.info(&note);
            }
            None => {}
        }
    }
}

/// Background work in flight — at most one, so the busy label is
/// unambiguous.
enum Job {
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
    /// The candidate lookup behind `D`: a read-only question to the
    /// index, cancellable like every other one-shot. The choice it
    /// feeds is a value for an action already chosen, not a browser.
    Downgrade {
        name: String,
        current: String,
        rx: Receiver<Result<Vec<Version>>>,
        cancel: CancelFlag,
    },
    /// A captured single-crate install streaming over `rx`.
    Build {
        name: String,
        rx: Receiver<BuildMsg>,
        /// The run loop's answer to `BuildMsg::NeedAuth`.
        auth_tx: Sender<bool>,
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
        needs_auth: Option<PathBuf>,
        /// The cancel state machine shared with the worker.
        control: std::sync::Arc<crate::BuildControl>,
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
    fn label(&self) -> String {
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

/// Hits the details panel shows; `api::search` guarantees no more.
const SEARCH_HITS: usize = 6;

/// A build the run loop still has to start, because starting one needs
/// the terminal: the escalation preflight may have to ask for a
/// password. `expect` carries a downgrade's premise — the version the
/// manifest must still hold — down to the worker, where it is checked
/// under the build's own lock.
struct PendingBuild {
    spec: String,
    locked: bool,
    expect: Option<String>,
}

/// The versions `D` offers for the selected crate, and the version they
/// are older than. The list is `index::downgrade_candidates`' own —
/// strictly older, non-yanked, pre-releases only for a pre-release
/// current — so the panel cannot become a browser of the whole
/// history; it is the choice the command already makes, shown where
/// the build will run.
pub struct DowngradeChoice {
    pub name: String,
    pub current: String,
    pub versions: Vec<Version>,
    /// Older releases the list does not show, so the footer can say so
    /// rather than pretending the offer is the whole history.
    pub older: usize,
}

/// How many versions the panel offers: what a single digit can name.
/// The CLI's prompt reads a line and can afford ten; a keypress is one
/// character, and 1-9 is the idiom the search overlay already uses.
const DOWNGRADE_CHOICES: usize = 9;

/// A finished search, shown until dismissed; digits pick a hit.
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<api::Hit>,
    /// Installed version per hit, read when the result was presented.
    pub installed: BTreeMap<String, String>,
}

pub struct App {
    prefix: PathBuf,
    cache: PathBuf,
    /// Ctrl-C during a build: quit once the worker reports back —
    /// leaving earlier would orphan a dying cargo.
    quit_after_build: bool,
    /// Every manifest entry, in manifest (alphabetical) order.
    rows: Vec<Row>,
    /// Age of the report the rows' statuses came from; `None` = no report.
    pub report_age: Option<Duration>,
    pub filter: Filter,
    /// Index into `visible()`, not into `rows`.
    pub selected: usize,
    pub message: Option<Message>,
    pub input: Option<Input>,
    pub confirm: Option<Confirm>,
    /// A confirmed migration waiting for the run loop (the preflight may
    /// need the terminal).
    pending_migrate: Option<PendingMigrate>,
    /// A running `M` batch; `finish_build` feeds it, `c` ends it.
    migrate_batch: Option<MigrateBatch>,
    pub search_result: Option<SearchResult>,
    pub downgrade_choice: Option<DowngradeChoice>,
    /// A build's sticky report pinned to the details panel until dismissed.
    pub build_report: Option<BuildReport>,
    /// The loader's refusal — the degraded state: list empty, mutating
    /// actions refused, `v` reachable. `None` again once a reload
    /// succeeds.
    pub manifest_error: Option<String>,
    /// Sticky-report scroll offset, reset by `pin_report`, clamped at
    /// draw time; a report that hides its own tail is no durable record.
    pub report_scroll: u16,
    /// The report panel's real scroll bound, written back by the
    /// renderer each frame (a `Cell`: `draw` takes `&App`). The key
    /// handler clamps against it, so the offset can no longer run past
    /// the tail and demand symmetric presses on the way back; the
    /// renderer still clamps for display, so a stale frame's bound
    /// costs one keypress, never a wrong picture.
    pub report_scroll_max: std::cell::Cell<u16>,
    pub show_help: bool,
    pending: Option<PendingAction>,
    /// A captured install waiting for the run loop (sudo preauth first).
    pending_build: Option<PendingBuild>,
    job: Option<Job>,
    /// Frame counter; drives the gauge spinner.
    ticks: usize,
    should_quit: bool,
}

/// Entry point for `cargo lbin tui`. Owns the terminal; teardown is
/// attempted whether or not the loop erred, and the loop's result is
/// reported first.
pub fn run(prefix: &Path) -> Result<()> {
    // Fail before raw mode as a plain error, not a garbled screen.
    let mut app = App::new(prefix)?;
    // `try_init` over `init`: a refused terminal is a normal error, and
    // its staged failure restores whatever did succeed.
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            let _ = ratatui::try_restore();
            return Err(e).context("initializing the TUI");
        }
    };

    let run_result = app.run(&mut terminal);
    let cursor_result = terminal.show_cursor();
    let restore_result = ratatui::try_restore();

    run_result?;
    cursor_result.context("showing the cursor on exit")?;
    restore_result.context("restoring the terminal on exit")?;
    Ok(())
}

/// What a reload found — a value every caller must face: since the
/// degraded state exists, `Ok` no longer means "the list is fresh".
/// A flag would let the next caller forget; `#[must_use]` makes
/// forgetting visible at the call site, where the next lie would be
/// written ("nothing installed", "now at ...", "checked: 0 updates").
#[must_use]
enum ReloadOutcome {
    Loaded,
    Degraded,
}

impl App {
    fn new(prefix: &Path) -> Result<Self> {
        let mut app = Self {
            prefix: prefix.to_path_buf(),
            cache: crate::cache_dir()?,
            rows: Vec::new(),
            report_age: None,
            filter: Filter::All,
            selected: 0,
            message: None,
            input: None,
            confirm: None,
            pending_migrate: None,
            migrate_batch: None,
            search_result: None,
            downgrade_choice: None,
            build_report: None,
            manifest_error: None,
            report_scroll: 0,
            report_scroll_max: std::cell::Cell::new(0),
            pending_build: None,
            ticks: 0,
            show_help: false,
            pending: None,
            job: None,
            should_quit: false,
            quit_after_build: false,
        };
        // Both outcomes are a session: degraded starts are by design.
        let _ = app.reload()?;
        Ok(app)
    }

    /// Re-read manifest and report, rebuild the rows — at start, after
    /// handoffs, before checks and search results: each is a moment the
    /// person is about to see or act on the prefix. The lock is held only
    /// for the read.
    fn reload(&mut self) -> Result<ReloadOutcome> {
        // A search result marks hits installed — a fact about the prefix,
        // stale the moment anything re-reads it; `finish_search` reloads
        // first and sets the new result after. A downgrade offer goes for
        // the same reason: it is "older than <this version>", and the
        // reload may be what changed that version.
        self.search_result = None;
        self.downgrade_choice = None;
        let report = match Report::load(&self.cache, &self.prefix) {
            Ok(report) => report,
            Err(e) => {
                self.warn(&format!("update report unreadable: {e:#}"));
                None
            }
        };
        self.apply_report(report.as_ref())
    }

    /// Rebuild rows from a fresh manifest and the given report — possibly
    /// one that failed to persist; what the index answered is shown.
    fn apply_report(&mut self, report: Option<&Report>) -> Result<ReloadOutcome> {
        let manifest = {
            let _lock = StateLock::acquire(&self.prefix, &Mode::Shared)?;
            // The loader's refusal is a state to present, not a reason to keep
            // the TUI out: verify exists to diagnose what `load` refuses, and a
            // session that dies on startup takes the v key with it. Degrade,
            // recover on the next successful reload.
            match Manifest::load(&self.prefix) {
                Ok(manifest) => {
                    self.manifest_error = None;
                    manifest
                }
                Err(e) => {
                    self.manifest_error = Some(crate::text::sanitize(&format!("{e:#}")));
                    self.rows.clear();
                    self.report_age = None;
                    self.clamp_selection();
                    self.error(
                        "the manifest cannot be loaded — v lists the findings; \
                         mutating actions are disabled until it is repaired \
                         (r retries the load)",
                    );
                    return Ok(ReloadOutcome::Degraded);
                }
            }
        };
        // Once per reload, lockless by design (see the prefixes module).
        let also = crate::prefixes::also_installed(&self.prefix);
        self.rows = rows_from(&manifest, report, &also);
        self.report_age = report.map(Report::age);
        self.clamp_selection();
        Ok(ReloadOutcome::Loaded)
    }

    /// Rows under the current filter, paired with their index in `rows`.
    pub fn visible(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| admits(self.filter, r))
            .collect()
    }

    /// Is a build job running? While one runs the hint bar swaps to the
    /// build's controls — controls, not promises: past the placement door
    /// `c` is `TooLate`, and each press's outcome is the runtime's to
    /// state.
    pub fn build_running(&self) -> bool {
        matches!(self.job, Some(Job::Build { .. }))
    }

    /// A one-shot (check, verify, search) holds the slot; the footer
    /// advertises its cancel door.
    pub fn oneshot_running(&self) -> bool {
        matches!(
            self.job,
            Some(
                Job::Check { .. } | Job::Verify { .. } | Job::Search { .. } | Job::Downgrade { .. }
            )
        )
    }

    /// Anything running or queued — the union every in-place mutation and
    /// the jump consult: a batch between members has an empty job slot
    /// and a full queue, and both count.
    fn anything_running(&self) -> bool {
        self.job.is_some()
            || self.migrate_batch.is_some()
            || self.pending_migrate.is_some()
            || self.pending_build.is_some()
            || self.pending.is_some()
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.visible().get(self.selected).copied()
    }

    pub fn updates_available(&self) -> usize {
        // Aligned with update --all semantics: pinned crates are held back
        // and counted separately. Meaning, not outcome — `U` computes fresh.
        self.rows
            .iter()
            .filter(|r| matches!(r.status, RowStatus::Outdated(_)) && !r.pinned)
            .count()
    }

    pub fn pinned_count(&self) -> usize {
        self.rows.iter().filter(|r| r.pinned).count()
    }

    /// Pinned crates the last check found newer versions for.
    pub fn pinned_outdated(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.pinned && matches!(r.status, RowStatus::Outdated(_)))
            .count()
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    /// Rows the report says nothing about — counted so "0 updates" never
    /// implies "all current".
    pub fn not_checked(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::Unknown)
            .count()
    }

    pub fn busy(&self) -> Option<String> {
        self.job.as_ref().map(Job::label)
    }

    /// The transient panel's frame title, following the job it hosts:
    /// the shape is shared, the name is not.
    pub fn transient_panel_title(&self) -> &'static str {
        match &self.job {
            Some(Job::Verify { .. }) => " Verify ",
            _ => " Build ",
        }
    }

    /// The live gauge line for a running build, or `None`. The spinner
    /// keeps the line visibly alive between units — one large crate can
    /// compile for minutes without a new `Compiling` line.
    pub fn build_progress(&self) -> Option<String> {
        const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
        // A running verify borrows the build's framed panel: one mechanism,
        // so the two cannot drift apart by hand.
        if let Some(Job::Verify {
            started, cancel, ..
        }) = &self.job
        {
            let frame = FRAMES[self.ticks % FRAMES.len()];
            // The panel tells the same truth as the footer label: after c
            // the state is cancel-requested, not "still checking".
            let state = if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "cancel requested"
            } else {
                "checking"
            };
            return Some(format!(
                "{frame} verify: {state} {} · elapsed {}",
                self.prefix.display(),
                format_elapsed(started.elapsed())
            ));
        }
        let Some(Job::Build {
            name,
            units_started,
            current,
            status_note,
            started,
            ..
        }) = &self.job
        else {
            return None;
        };
        let frame = FRAMES[self.ticks % FRAMES.len()];
        let elapsed = format_elapsed(started.elapsed());
        // A pipeline notice is the live truth — "waiting for the state
        // lock..." beats a gauge frozen at zero; the clock runs through it.
        if let Some(note) = status_note {
            return Some(format!("{frame} {name}: {note} · elapsed {elapsed}"));
        }
        let unit_word = if *units_started == 1 { "unit" } else { "units" };
        let mut line = format!("{frame} building {name} · {units_started} {unit_word}");
        if let Some(current) = current {
            let _ = write!(line, " · compiling {current}");
        }
        let _ = write!(line, " · elapsed {elapsed}");
        Some(line)
    }

    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    fn clamp_selection(&mut self) {
        let len = self.visible().len();
        self.selected = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            self.ticks = self.ticks.wrapping_add(1);
            self.escalate_overdue_cancel();
            terminal.draw(|frame| ui::draw(frame, self))?;

            if let Some(action) = self.pending.take() {
                self.run_in_terminal(terminal, &action)?;
                continue;
            }
            if let Some(req) = self.pending_build.take() {
                self.start_build(terminal, &req)?;
                continue;
            }
            if let Some(req) = self.pending_migrate.take() {
                let member = req.name.clone();
                if let StartOutcome::Refused(reason) = self.start_migrate(terminal, req)? {
                    match self.migrate_batch.as_mut() {
                        // A batch cannot wait on a worker that never existed: the refusal is
                        // recorded as a failure where the summary will not overwrite it.
                        Some(batch) => {
                            batch.failed.push((member, vec![reason]));
                            self.finalize_migrate_batch(Some("aborted"));
                        }
                        None => self.error(&reason),
                    }
                }
                continue;
            }
            if matches!(
                &self.job,
                Some(Job::Build {
                    needs_auth: Some(_),
                    ..
                })
            ) {
                self.answer_auth(terminal)?;
            }

            if event::poll(TICK)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
            self.poll_job()?;
        }
        Ok(())
    }

    /// Steps out of the TUI, runs the command as the CLI would, steps
    /// back in; the wait for Enter lets the person read the output.
    /// The cursor is shown explicitly (sudo would otherwise prompt at an
    /// invisible one); `try_restore` because handing over a terminal
    /// still in raw mode would be worse than aborting.
    fn run_in_terminal(
        &mut self,
        terminal: &mut DefaultTerminal,
        action: &PendingAction,
    ) -> Result<()> {
        terminal.show_cursor()?;
        ratatui::try_restore().context("leaving the TUI")?;
        println!();
        let outcome = match action {
            PendingAction::Downgrade(name) => crate::cmd_downgrade(&self.prefix, name),
            PendingAction::Update(name) => {
                crate::cmd_update(&self.prefix, std::slice::from_ref(name), false, false)
            }
            PendingAction::UpdateAll => crate::cmd_update(&self.prefix, &[], true, false),
            PendingAction::Install { crates, locked } => {
                crate::cmd_install(&self.prefix, crates, *locked)
            }
            PendingAction::Remove(name) => {
                crate::cmd_remove(&self.prefix, std::slice::from_ref(name))
            }
            PendingAction::SetPinned { name, pinned } => {
                crate::cmd_set_pinned(&self.prefix, std::slice::from_ref(name), *pinned)
            }
        };
        if let Err(e) = &outcome {
            eprintln!("error: {e:#}");
        }
        eprint!("\n[press Enter to return] ");
        let _ = std::io::stdin().read_line(&mut String::new());
        enable_raw_mode().context("re-entering raw mode")?;
        std::io::stdout()
            .execute(EnterAlternateScreen)
            .context("re-entering the alternate screen")?;
        terminal.clear()?;

        // The command may have changed everything; reload shows that as
        // "not checked". Degraded or loaded, the outcome line below still
        // stands — the footer carries the degraded fact.
        let _ = self.reload()?;
        match outcome {
            Ok(()) => self.info(&format!("{} finished", action_label(action))),
            Err(e) => self.error(&format!("{} failed: {e:#}", action_label(action))),
        }
        Ok(())
    }

    /// What the escalation preflight found for one prefix; each captured
    /// workflow maps the outcomes to what it can offer.
    fn preflight_escalation(terminal: &mut DefaultTerminal, prefix: &Path) -> Result<Preflight> {
        let policy = crate::privileged::Policy::for_prefix(prefix);
        // The pipeline's union (bin + state) plus the lock file — the
        // worker's first privileged touch: a migration into /usr/local may
        // prepare the first lock ever there, ahead of any NeedAuth, and a
        // cold sudo must be asked, not fail `sudo -n`.
        let escalate = match crate::placement_needs_privilege(policy, prefix) {
            Ok(escalate) => escalate,
            Err(e) => {
                return Ok(Preflight::Reported(format!("{e:#}")));
            }
        };
        if !escalate {
            return Ok(Preflight::Ready);
        }
        let fresh = match crate::privileged::credentials_fresh() {
            Ok(fresh) => fresh,
            Err(e) => {
                return Ok(Preflight::Reported(format!("{e:#}")));
            }
        };
        let prefix = prefix.to_path_buf();
        if !fresh
            && let Err(e) =
                Self::suspended(terminal, || crate::privileged::preauthorize(&prefix, true))?
        {
            return Ok(Preflight::Reported(format!("{e:#}")));
        }
        // Right after validation the timestamp should be warm; a sudo that
        // does not cache is detected now, not by the worker's `sudo -n`.
        match crate::privileged::credentials_fresh() {
            Ok(true) => Ok(Preflight::Ready),
            Ok(false) => Ok(Preflight::NoCache),
            Err(e) => Ok(Preflight::Reported(format!("{e:#}"))),
        }
    }

    /// Can captured placement run? `false` means the attempt is over:
    /// either it was reported, or it left as a terminal handoff.
    ///
    /// Captured placement runs `sudo -n`; a non-caching sudo would be
    /// asked a question it cannot voice, so the work falls back to the
    /// terminal instead of a doomed build. A downgrade hands over *as a
    /// downgrade*: `install NAME@VERSION` there would place the same
    /// version without the premise check, and the command that owns
    /// that check is the one to run — it asks for a version again,
    /// which is the price of the handover.
    fn escalation_ready(
        &mut self,
        terminal: &mut DefaultTerminal,
        req: &PendingBuild,
        name: &str,
    ) -> Result<bool> {
        match Self::preflight_escalation(terminal, &self.prefix.clone())? {
            Preflight::Ready => Ok(true),
            Preflight::NoCache => {
                self.info("sudo does not cache credentials here; handing the terminal over");
                self.pending = Some(match &req.expect {
                    Some(_) => PendingAction::Downgrade(name.to_owned()),
                    None => PendingAction::Install {
                        crates: vec![req.spec.clone()],
                        locked: req.locked,
                    },
                });
                Ok(false)
            }
            Preflight::Reported(message) => {
                self.error(&message);
                Ok(false)
            }
        }
    }

    /// A captured install. The run loop calls this because only it owns
    /// the terminal: a stale credential prompt happens here, up front —
    /// never inside the alternate screen.
    fn start_build(&mut self, terminal: &mut DefaultTerminal, req: &PendingBuild) -> Result<()> {
        let (raw_spec, locked) = (req.spec.as_str(), req.locked);
        let spec = match InstallSpec::parse_all(std::slice::from_ref(&raw_spec.to_owned())) {
            Ok(mut specs) => specs.remove(0),
            Err(e) => {
                self.error(&format!("{e:#}"));
                return Ok(());
            }
        };
        if self.refused_by_advisory_pin_check(&spec) {
            return Ok(());
        }
        // A new attempt supersedes the previous report — a stale failure
        // over a fresh run would report on the wrong world.
        self.build_report = None;
        if !self.escalation_ready(terminal, req, &spec.name)? {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel();
        let (auth_tx, auth_rx) = mpsc::channel();
        let control = std::sync::Arc::new(crate::BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let prefix = self.prefix.clone();
        let name = spec.name.clone();
        let worker_tx = tx.clone();
        // A downgrade's premise travels into the worker, where the lock
        // that decides it is held.
        let expect = req.expect.clone();
        std::thread::spawn(move || {
            let line_tx = worker_tx.clone();
            let control = worker_control;
            let mut on_line = |k: crate::LineKind, l: &str| {
                let _ = line_tx.send(build_msg(k, l));
            };
            let mut before_placement = |escalating: &Path| {
                // A cancelled build must not ask for a password: refuse instead of
                // raising NeedAuth; the run loop guards the other side of the race.
                if control.cancelled() {
                    return Err(anyhow::Error::new(crate::BuildCancelled));
                }
                match crate::privileged::credentials_fresh() {
                    // The common case: the up-front validation is still
                    // fresh and placement proceeds without a word.
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        let _ = worker_tx.send(BuildMsg::NeedAuth(escalating.to_path_buf()));
                        match auth_rx.recv() {
                            Ok(true) => Ok(()),
                            // A denial answering a cancel *is* the cancel; a real refusal keeps
                            // its own name.
                            Ok(false) if control.cancelled() => {
                                Err(anyhow::Error::new(crate::BuildCancelled))
                            }
                            Ok(false) => anyhow::bail!("sudo authentication failed"),
                            Err(_) => {
                                anyhow::bail!("the interface went away mid-authorization")
                            }
                        }
                    }
                    Err(e) => Err(e),
                }
            };
            // One door per intent: an install places what was asked for,
            // a downgrade first makes the statement its name implies —
            // under the lock, not against a snapshot.
            let result = match (&expect, &spec.version) {
                (Some(expected), Some(version)) => crate::tui_downgrade_one(
                    &prefix,
                    &spec.name,
                    expected,
                    version,
                    &mut on_line,
                    &mut before_placement,
                    &control,
                ),
                _ => crate::tui_install_one(
                    &prefix,
                    &spec,
                    locked,
                    &mut on_line,
                    &mut before_placement,
                    &control,
                ),
            };
            // Classified here, once, by downcast — see `BuildOutcome`.
            let outcome = match result {
                Ok(()) => BuildOutcome::Success,
                Err(e) if e.downcast_ref::<crate::BuildCancelled>().is_some() => {
                    BuildOutcome::Cancelled
                }
                Err(e) => BuildOutcome::Failed(e),
            };
            let _ = tx.send(BuildMsg::Done(outcome));
        });
        self.job = Some(Job::Build {
            name,
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control,
            // The premise is what makes this a downgrade, so it is also
            // what the record answers to.
            kind: if req.expect.is_some() {
                BuildKind::Downgrade
            } else {
                BuildKind::Install
            },
            cancel_deadline: None,
        });
        Ok(())
    }

    /// Start an in-place migration: the same job, gauge, cancel door and
    /// sudo roundtrip as an install — a frontend to `migrate`, not a
    /// second migrate. Whether a worker started travels back with its
    /// reason: a batch must both end and *record* the refusal.
    fn start_migrate(
        &mut self,
        terminal: &mut DefaultTerminal,
        req: PendingMigrate,
    ) -> Result<StartOutcome> {
        let PendingMigrate {
            name,
            version,
            dest,
            snap,
        } = req;
        if self.job.is_some() {
            return Ok(StartOutcome::Refused(
                "another operation is already running".to_owned(),
            ));
        }
        // A new attempt supersedes the last report; a cancelled outcome
        // returns before any later cleanup.
        self.build_report = None;
        // The destination's preflight before the worker exists: its lock may
        // be the first ever prepared there, so a cold sudo is asked here on
        // the suspended terminal. The source keeps its late, in-flight
        // revalidation — warming it before a long build buys nothing.
        match Self::preflight_escalation(terminal, &dest)? {
            Preflight::Ready => {}
            // No terminal handoff for migrate, on purpose; the CLI is the
            // interactive shape.
            Preflight::NoCache => {
                return Ok(StartOutcome::Refused(
                    "sudo does not cache credentials here; migrate via the CLI, \
                     which prompts interactively"
                        .to_owned(),
                ));
            }
            Preflight::Reported(message) => return Ok(StartOutcome::Refused(message)),
        }
        let (tx, rx) = mpsc::channel();
        let (auth_tx, auth_rx) = mpsc::channel();
        let control = std::sync::Arc::new(crate::BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let source = self.prefix.clone();
        let dest_w = dest.clone();
        let worker_name = name.clone();
        let worker_tx = tx.clone();
        std::thread::spawn(move || {
            let line_tx = worker_tx.clone();
            let control = worker_control;
            let result = crate::tui_migrate_one(
                &source,
                &dest_w,
                &worker_name,
                &snap,
                &mut |k: crate::LineKind, l: &str| {
                    let _ = line_tx.send(build_msg(k, l));
                },
                &mut |escalating: &Path| {
                    // Same auth roundtrip as an install; a denial answering a cancel is
                    // the cancel.
                    if control.cancelled() {
                        return Err(anyhow::Error::new(crate::BuildCancelled));
                    }
                    match crate::privileged::credentials_fresh() {
                        Ok(true) => Ok(()),
                        Ok(false) => {
                            let _ = worker_tx.send(BuildMsg::NeedAuth(escalating.to_path_buf()));
                            match auth_rx.recv() {
                                Ok(true) => Ok(()),
                                Ok(false) if control.cancelled() => {
                                    Err(anyhow::Error::new(crate::BuildCancelled))
                                }
                                Ok(false) => anyhow::bail!("sudo authentication failed"),
                                Err(_) => {
                                    anyhow::bail!("the interface went away mid-authorization")
                                }
                            }
                        }
                        Err(e) => Err(e),
                    }
                },
                &control,
            );
            // Classified once, by type and data — the UI never guesses.
            let outcome = match result {
                Ok(crate::MigrateOutcome::Moved { version, .. }) => BuildOutcome::Migrated(version),
                Ok(crate::MigrateOutcome::Incomplete(reason)) => {
                    BuildOutcome::CompletedWithWarning(reason)
                }
                Err(e) if e.downcast_ref::<crate::BuildCancelled>().is_some() => {
                    BuildOutcome::Cancelled
                }
                Err(e) => BuildOutcome::Failed(e),
            };
            let _ = tx.send(BuildMsg::Done(outcome));
        });
        self.job = Some(Job::Build {
            name,
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control,
            kind: BuildKind::Migrate(Box::new(MigrateTarget { version, dest })),
            cancel_deadline: None,
        });
        Ok(StartOutcome::Started)
    }

    /// The worker hit the checkpoint with a stale timestamp — the build
    /// outlived it. Revalidate on the real terminal; arm the one-shot
    /// grace deadline (a repeat keeps the original — double-`c` escalates
    /// through `request_cancel` itself).
    fn arm_cancel_grace(&mut self) {
        if let Some(Job::Build {
            cancel_deadline, ..
        }) = &mut self.job
        {
            cancel_deadline.get_or_insert(std::time::Instant::now() + CANCEL_GRACE);
        }
    }

    /// The cancel's dead-man switch, every tick: once the grace expires,
    /// SIGKILL goes out from this thread — the worker cannot be trusted
    /// to reach its own sweep. One-shot; Killed is worth a line.
    fn escalate_overdue_cancel(&mut self) {
        let due = matches!(
            &self.job,
            Some(Job::Build {
                cancel_deadline: Some(d),
                ..
            }) if std::time::Instant::now() >= *d
        );
        if !due {
            return;
        }
        let outcome = if let Some(Job::Build {
            control,
            cancel_deadline,
            ..
        }) = &mut self.job
        {
            *cancel_deadline = None;
            Some(control.request_cancel())
        } else {
            None
        };
        if matches!(outcome, Some(crate::CancelOutcome::Killed)) {
            self.warn("the build ignored the cancel; SIGKILL sent");
        }
    }

    fn answer_auth(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // A cancel that landed after NeedAuth: answer no without suspending
        // the screen — nobody types a password for a build that will never
        // place.
        if let Some(Job::Build {
            control,
            auth_tx,
            needs_auth,
            ..
        }) = &mut self.job
            && control.cancelled()
        {
            *needs_auth = None;
            let _ = auth_tx.send(false);
            return Ok(());
        }
        // The prefix comes from the request, never the app: a migration
        // escalates for its *destination* while `self.prefix` is the source.
        let Some(target) = self.job.as_mut().and_then(|job| {
            let Job::Build { needs_auth, .. } = job else {
                return None;
            };
            needs_auth.take()
        }) else {
            return Ok(());
        };
        let outcome = Self::suspended(terminal, || crate::privileged::preauthorize(&target, true))?;
        let ok = match outcome {
            Ok(()) => match crate::privileged::credentials_fresh() {
                Ok(true) => true,
                // Validated and instantly stale: this sudo does not cache, and the
                // noninteractive placement ahead cannot ask — fail loudly now.
                Ok(false) => {
                    self.warn(
                        "sudo did not retain credentials; noninteractive placement cannot proceed",
                    );
                    false
                }
                Err(e) => {
                    self.warn(&format!("{e:#}"));
                    false
                }
            },
            Err(e) => {
                self.warn(&format!("{e:#}"));
                false
            }
        };
        if let Some(Job::Build { auth_tx, .. }) = &mut self.job {
            // `needs_auth` was taken with the target above; only the answer
            // remains.
            let _ = auth_tx.send(ok);
        }
        Ok(())
    }

    /// The other prefix of the known pair — the gate `m`/`M` share.
    /// Symmetric on purpose: the candidate's own view must name us back,
    /// or a custom prefix on a HOME-less system would pass the first
    /// half.
    fn known_pair_dest(&self) -> Option<PathBuf> {
        let others = crate::prefixes::known_others(&self.prefix);
        match others.as_slice() {
            [dest]
                if crate::prefixes::known_others(dest)
                    .iter()
                    .any(|p| p == &self.prefix) =>
            {
                Some(dest.clone())
            }
            _ => None,
        }
    }

    /// Advisory precheck for `m`: already installed at the destination?
    /// Fresh, silent, nonblocking — nobody should confirm a migration the
    /// authoritative refusal will bounce. Busy or unreadable answers
    /// "not occupied": the backend stays the judge.
    fn destination_occupied(&mut self, dest: &Path, name: &str) -> bool {
        let advisory = StateLock::try_acquire_with(
            dest,
            &Mode::Shared,
            crate::privileged::Policy::for_prefix(dest).screen_owned(),
            &mut |_| {},
        );
        if let Ok(Some(_lock)) = advisory
            && let Ok(manifest) = Manifest::load(dest)
            && let Some(entry) = manifest.crates.get(name)
        {
            self.error(&format!(
                "{name} is already installed at {} ({}); remove one side first \
                 (no --force by design)",
                dest.display(),
                entry.version
            ));
            return true;
        }
        false
    }

    /// Advisory pin check before anyone types a password. Silent,
    /// nonblocking: busy yields "not now", never a frozen UI; the
    /// authoritative pass runs in the worker. True = stop here (message
    /// already shown).
    fn refused_by_advisory_pin_check(&mut self, spec: &InstallSpec) -> bool {
        if spec.version.is_some() {
            return false;
        }
        let advisory = StateLock::try_acquire_with(
            &self.prefix,
            &Mode::Shared,
            crate::privileged::Policy::for_prefix(&self.prefix).screen_owned(),
            &mut |_| {},
        );
        if let Ok(Some(_lock)) = advisory {
            match Manifest::load(&self.prefix) {
                Ok(m) if m.crates.get(&spec.name).is_some_and(|e| e.pinned) => {
                    self.error(&format!(
                        "{} is pinned; `p` unpins it, or name a version to re-pin",
                        spec.name
                    ));
                    return true;
                }
                Ok(_) => {}
                Err(e) => {
                    self.error(&format!("{e:#}"));
                    return true;
                }
            }
        }
        false
    }

    /// Leaves the TUI, runs `f` on the real terminal, re-enters. The
    /// outer Result is the handover itself; `f`'s result is the inner
    /// value.
    fn suspended<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> T) -> Result<T> {
        terminal.show_cursor()?;
        ratatui::try_restore().context("leaving the TUI")?;
        println!();
        let value = f();
        enable_raw_mode().context("re-entering raw mode")?;
        std::io::stdout()
            .execute(EnterAlternateScreen)
            .context("re-entering the alternate screen")?;
        terminal.clear()?;
        Ok(value)
    }

    /// One batch member finished; tally, advance or wrap up. A cancel
    /// ends the whole batch — continuing silently would be guessing
    /// intent.
    fn finish_batch_step(
        &mut self,
        name: &str,
        target: &MigrateTarget,
        outcome: BuildOutcome,
        tail: &VecDeque<String>,
        warnings: Vec<String>,
    ) {
        // The footer speaks the result where there is one: a moved
        // member announces Migrated's payload — an unpinned 0.1.0 that
        // arrived as 0.2.0 must be announced as 0.2.0; a bare Success on
        // a migrate job (a worker bug by the classification's contract)
        // claims no version rather than inventing one; every other
        // outcome identifies the member by the source version it was
        // confirmed at.
        let spoken = match &outcome {
            BuildOutcome::Migrated(installed) => format!(" {installed}"),
            BuildOutcome::Success => String::new(),
            _ => format!(" {}", target.version),
        };
        // Per-crate reload keeps the list truthful; a hard failure is
        // recorded on the batch. Degraded is not an error here: the summary
        // and the footer's state line both carry it.
        let reload_error = match self.reload() {
            Ok(_) => None,
            Err(e) => Some(format!("{e:#}")),
        };
        let cancelled = matches!(outcome, BuildOutcome::Cancelled);
        let (done, total) = {
            let Some(batch) = self.migrate_batch.as_mut() else {
                return;
            };
            if let Some(e) = reload_error {
                batch.reload_error = Some(e);
            }
            // The same diagnostics contract as a single job, member by member:
            // warnings not laundered, terse failures get the tail, Incomplete
            // reasons travel with their member's warnings.
            match outcome {
                BuildOutcome::Success | BuildOutcome::Migrated(_) => {
                    batch.moved += 1;
                    if !warnings.is_empty() {
                        batch.noticed.push((name.to_owned(), warnings));
                    }
                }
                BuildOutcome::Cancelled => {}
                BuildOutcome::CompletedWithWarning(reason) => {
                    let mut lines = vec![crate::text::sanitize(&reason)];
                    lines.extend(warnings);
                    batch.warned.push((name.to_owned(), lines));
                }
                BuildOutcome::Failed(e) => {
                    batch
                        .failed
                        .push((name.to_owned(), Self::failure_lines(&e, tail)));
                }
            }
            (
                batch.moved + batch.warned.len() + batch.failed.len(),
                batch.total,
            )
        };
        if cancelled {
            self.finalize_migrate_batch(Some("cancelled"));
            return;
        }
        self.info(&format!("[{done}/{total}] {name}{spoken} processed"));
        let next = self
            .migrate_batch
            .as_mut()
            .and_then(|batch| batch.queue.pop_front());
        match next {
            Some(req) => self.pending_migrate = Some(req),
            None => self.finalize_migrate_batch(None),
        }
    }

    /// The batch's one summary: counts in the footer, shortfalls in a
    /// panel — the already-installed refusal counted as on the CLI.
    fn finalize_migrate_batch(&mut self, ended_early: Option<&str>) {
        let Some(batch) = self.migrate_batch.take() else {
            return;
        };
        let MigrateBatch {
            dest,
            queue,
            total,
            moved,
            warned,
            failed,
            noticed,
            reload_error,
        } = batch;
        // Tally plus queue must add up to the plan: a refused member sits
        // in `failed`, so "not attempted" is exactly what is still queued.
        let mut summary = format!("migrated {moved} of {total} to {}", dest.display());
        if let Some(how) = ended_early {
            let unprocessed = queue.len();
            // write!, not push_str(&format!(..)): no second allocation.
            let _ = std::fmt::Write::write_fmt(
                &mut summary,
                format_args!(" ({how}; {unprocessed} not attempted)"),
            );
        }
        if warned.is_empty() && failed.is_empty() && noticed.is_empty() && reload_error.is_none() {
            if ended_early.is_some() {
                self.warn(&summary);
            } else {
                self.info(&summary);
            }
            return;
        }
        let mut lines = Vec::new();
        let section = |lines: &mut Vec<String>, header: &str, entries: &[(String, Vec<String>)]| {
            if entries.is_empty() {
                return;
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(header.to_owned());
            for (name, entry_lines) in entries {
                lines.push(crate::text::sanitize(&format!("  {name}:")));
                for line in entry_lines {
                    lines.push(crate::text::sanitize(&format!("    {line}")));
                }
            }
        };
        section(&mut lines, "failed:", &failed);
        section(
            &mut lines,
            "destination committed, source not retired:",
            &warned,
        );
        section(&mut lines, "migrated, with build warnings:", &noticed);
        if let Some(e) = reload_error {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(crate::text::sanitize(&format!(
                "(and a mid-batch list reload failed: {e})"
            )));
        }
        self.pin_report(BuildReport {
            title: format!("migrate --all: {moved} of {total} migrated"),
            lines,
            failed: !failed.is_empty(),
        });
        self.warn(&format!(
            "{summary} — details in the panel; Esc/Enter dismisses"
        ));
    }

    /// The diagnostics a failure shows, single job and batch member
    /// alike: the error chain, plus the tail when the chain is terse —
    /// the compiler's actual message often lives there.
    fn failure_lines(e: &anyhow::Error, tail: &VecDeque<String>) -> Vec<String> {
        let text = format!("{e:#}");
        let mut lines: Vec<String> = text.lines().map(crate::text::sanitize).collect();
        if lines.len() <= 1 {
            lines.extend(tail.iter().rev().take(8).rev().cloned());
        }
        lines
    }

    /// Warnings as they arrive, not as a post-mortem. The worker emits
    /// them before the work they describe — a cross-prefix duplicate is
    /// announced before the build starts — and holding them until
    /// `Done` would turn a heads-up into "by the way, you just created
    /// a second copy". This surfaces each one on receipt and keeps it
    /// readable while the build continues; it promises no more than
    /// that, because nothing here delays the worker: a fast enough
    /// build can deliver `Warning` and `Done` into the same poll, and
    /// buying a guaranteed frame would mean letting a warning shape
    /// execution — the opposite of informing without gating. The panel
    /// is updated in place rather than re-pinned, so a growing list
    /// does not yank a reader's scroll back to the top; a build start
    /// already cleared any older report, so anything pinned here is
    /// this build's own.
    fn show_live_warnings(&mut self, name: &str, kind: &BuildKind, warnings: &[String]) {
        let report = BuildReport {
            title: format!("{} {name}: warnings", kind.verb()),
            lines: warnings.to_vec(),
            failed: false,
        };
        match &mut self.build_report {
            Some(existing) => *existing = report,
            None => self.pin_report(report),
        }
    }

    /// A finished captured install: reload, then speak in the pipeline's
    /// own words when it left any.
    fn finish_build(
        &mut self,
        name: &str,
        kind: &BuildKind,
        outcome: BuildOutcome,
        tail: &VecDeque<String>,
        warnings: Vec<String>,
    ) {
        let verb = kind.verb();
        // The live warning panel was this build's running record; every
        // branch below pins whatever the finished build deserves, and a
        // cancel deserves none. Clearing here keeps a stale live panel
        // from outliving the job it belonged to.
        self.build_report = None;
        // A batch owns its members' presentation: tallies, one summary.
        if self.migrate_batch.is_some()
            && let BuildKind::Migrate(target) = kind
        {
            self.finish_batch_step(name, target, outcome, tail, warnings);
            return;
        }
        // A cancelled build ended exactly as asked: no panel, no log (the
        // pipeline wrote none, removed the stage), nothing to reload. The
        // classification is the worker's, by type — a cancel that lost every
        // race arrives as the Success or Failed it truly was.
        if matches!(outcome, BuildOutcome::Cancelled) {
            self.info(&format!("{verb} {name} cancelled"));
            return;
        }
        // The pipeline's error outranks a reload error: the tail and log
        // path are the diagnosis. On success the roles flip — the reload
        // *is* the remaining work.
        let reload = self.reload();
        match outcome {
            BuildOutcome::Cancelled => unreachable!("returned above"),
            outcome @ (BuildOutcome::Success | BuildOutcome::Migrated(_)) => {
                // Composed from the job's data: the worker reports outcomes, the UI
                // owns the words; the migrate note speaks the version the
                // destination committed (Migrated's payload), never the
                // frozen plan's.
                let note = match kind {
                    BuildKind::Install | BuildKind::Downgrade => tail
                        .iter()
                        .rev()
                        .find(|l| l.starts_with("installed "))
                        .cloned()
                        .unwrap_or_else(|| format!("{verb} {name} finished")),
                    BuildKind::Migrate(target) => match &outcome {
                        BuildOutcome::Migrated(installed) => {
                            format!("migrated {name} {installed} to {}", target.dest.display())
                        }
                        // A migrate worker classifies every move as
                        // Migrated; a bare Success here would be a
                        // worker bug — claim no version rather than
                        // invent one from the plan.
                        _ => format!("migrated {name} to {}", target.dest.display()),
                    },
                };
                // A failed reload does not eat the outcome: the report is pinned
                // first, the reload complains after.
                if warnings.is_empty() {
                    match reload {
                        // Degraded included: the note states the operation's true outcome;
                        // the footer's state line carries the degraded fact.
                        Ok(_) => self.info(&note),
                        Err(e) => self.error(&format!("{note} — but reload failed: {e:#}")),
                    }
                } else {
                    // Captured but never shown would make the classification pointless:
                    // the panel keeps them until acknowledged.
                    let mut lines = warnings;
                    if let Err(e) = &reload {
                        lines.push(String::new());
                        lines.push(crate::text::sanitize(&format!(
                            "(and the list reload failed: {e:#})"
                        )));
                    }
                    self.pin_report(BuildReport {
                        title: format!("{verb} {name}: warnings"),
                        lines,
                        failed: false,
                    });
                    self.warn(&format!(
                        "{note} — with warnings in the panel; Esc/Enter dismisses"
                    ));
                }
            }
            BuildOutcome::CompletedWithWarning(reason) => {
                // The build half succeeded and the disk changed — reload already
                // reflects it. The reason is a payload shown whole in the wrapping
                // panel: multi-sentence by design, and a truncated footer line must
                // not be its only copy. Not a failure: unfinished, not broken.
                let mut lines: Vec<String> = vec![crate::text::sanitize(&reason)];
                if !warnings.is_empty() {
                    lines.push(String::new());
                    lines.extend(warnings);
                }
                if let Err(e) = &reload {
                    lines.push(String::new());
                    lines.push(crate::text::sanitize(&format!(
                        "(and the list reload failed: {e:#})"
                    )));
                }
                self.pin_report(BuildReport {
                    title: format!("{verb} {name}: completed with a warning"),
                    lines,
                    failed: false,
                });
                self.warn(&format!(
                    "{verb} {name} finished with a warning — details in the panel; \
                     Esc/Enter dismisses"
                ));
            }
            BuildOutcome::Failed(e) => {
                // An anyhow chain carries paths too; same boundary rule, shared with
                // the batch.
                let mut lines = Self::failure_lines(&e, tail);
                // No warnings here on purpose: they described binaries the rollback
                // removed — "foo is shadowed" is not true of an install that did not
                // happen.
                if let Err(re) = reload {
                    lines.push(crate::text::sanitize(&format!(
                        "(and the list reload failed: {re:#})"
                    )));
                }
                self.pin_report(BuildReport {
                    // The operation's own word, not "build": the failure may be
                    // placement or the manifest commit, so the title must not
                    // narrow it — and not "install" either, which would rename
                    // what the person asked for.
                    title: format!("{verb} {name} failed"),
                    lines,
                    failed: true,
                });
                self.error(&format!(
                    "{verb} {name} failed — details in the panel; Esc/Enter dismisses"
                ));
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Quit — but never orphan cargo: with a build running the exit is a
            // cancel first, leave when the worker reports back. Placement cannot
            // be cancelled, so there the exit waits it out.
            if let Some(Job::Build { control, .. }) = &self.job {
                match control.request_cancel() {
                    crate::CancelOutcome::Accepted => {
                        self.arm_cancel_grace();
                        self.info(
                            "cancelling; quitting when the build stops (Ctrl-C again: SIGKILL)",
                        );
                    }
                    crate::CancelOutcome::Killed => {
                        self.warn("SIGKILL sent; quitting when the build stops");
                    }
                    crate::CancelOutcome::AlreadyStopping => {
                        self.info("the build is already stopping; quitting when it does");
                    }
                    crate::CancelOutcome::TooLate => {
                        self.info("placement in progress; quitting when it finishes");
                    }
                }
                self.quit_after_build = true;
            } else {
                self.should_quit = true;
            }
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.build_report.is_some() && self.input.is_none() && self.confirm.is_none() {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.build_report = None;
                    self.message = None;
                    return;
                }
                // While the report owns the panel the arrows scroll it — its
                // tail must be reachable. Down-moves clamp against the bound
                // the renderer wrote back last frame (not a guessed cap — the
                // old guess once hid the tail of a line that wrapped fifteen
                // ways), so the offset never runs past the tail and Up answers
                // on the first press.
                KeyCode::Up => {
                    self.report_scroll = self.report_scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.report_scroll = self
                        .report_scroll
                        .saturating_add(1)
                        .min(self.report_scroll_max.get());
                    return;
                }
                KeyCode::PageUp => {
                    self.report_scroll = self.report_scroll.saturating_sub(5);
                    return;
                }
                KeyCode::PageDown => {
                    self.report_scroll = self
                        .report_scroll
                        .saturating_add(5)
                        .min(self.report_scroll_max.get());
                    return;
                }
                KeyCode::Home => {
                    self.report_scroll = 0;
                    return;
                }
                KeyCode::End => {
                    self.report_scroll = self.report_scroll_max.get();
                    return;
                }
                _ => {}
            }
        }
        if self.confirm.is_some() {
            self.on_key_confirm(key);
        } else if self.input.is_some() {
            self.on_key_input(key);
        } else {
            self.on_key_list(key);
        }
    }

    fn on_key_list(&mut self, key: KeyEvent) {
        // The degraded gate: only the keys that help — audit, retry, help,
        // quit. Refusing here says why once, instead of each worker
        // discovering it noisily. `r` is repurposed to a plain reload retry:
        // the network cannot help a broken manifest.
        if self.manifest_error.is_some() {
            match key.code {
                // B on purpose: the jump is how one *leaves* a broken prefix — a
                // gate that lets you in but not out would be a trap.
                KeyCode::Char('q' | 'v' | '?' | 'B') | KeyCode::Esc => {}
                // The cancel door the degraded verify opened: v works
                // here on purpose, so c must reach its running job — the
                // gate swallowing it would answer a cancel with repair
                // instructions.
                KeyCode::Char('c') if self.oneshot_running() => {}
                KeyCode::Char('r') => {
                    match self.reload() {
                        Ok(ReloadOutcome::Loaded) => {
                            self.info("the manifest loads again");
                        }
                        // Still broken: apply_report set the message; the footer shows the
                        // state either way.
                        Ok(ReloadOutcome::Degraded) => {}
                        Err(e) => self.error(&format!("reload failed: {e:#}")),
                    }
                    return;
                }
                _ => {
                    self.error(
                        "the manifest cannot be loaded — repair it by hand \
                         (v lists the findings), then r retries the load",
                    );
                    return;
                }
            }
        }
        match key.code {
            KeyCode::Char('q') => {
                if matches!(self.job, Some(Job::Build { .. })) {
                    self.error("a build is running; c cancels it, Ctrl-C cancels and quits");
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Esc => {
                if self.downgrade_choice.take().is_some() || self.search_result.take().is_some() {
                    // Dismissing a result is fine mid-build; only the exit is held back.
                } else if matches!(self.job, Some(Job::Build { .. })) {
                    self.error("a build is running; c cancels it, Ctrl-C cancels and quits");
                } else {
                    self.should_quit = true;
                }
            }
            // Cancel and stay. Builds escalate: first press SIGTERMs the
            // group, a second SIGKILLs. One-shots set a flag and nothing
            // more. With no job, silence — nothing else could be meant.
            KeyCode::Char('c') => self.cancel_pressed(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.visible().len().saturating_sub(1);
            }
            KeyCode::Tab => {
                self.filter = self.filter.next();
                self.clamp_selection();
            }
            KeyCode::BackTab => {
                self.filter = self.filter.prev();
                self.clamp_selection();
            }
            KeyCode::Char('r') => self.start_check(),
            KeyCode::Char('v') => self.start_verify(),
            KeyCode::Char('s') => self.open_input(InputPurpose::Search),
            // With a search result up, a digit picks a hit and opens the install
            // line — editable, so `--locked` can still be added.
            // With an offer up, a digit names the version — the same idiom
            // the search overlay uses, and the reason the list stops at
            // nine.
            KeyCode::Char(c @ '1'..='9') if self.downgrade_choice.is_some() => {
                let pick = c
                    .to_digit(10)
                    .and_then(|d| usize::try_from(d).ok())
                    .and_then(|d| d.checked_sub(1));
                if let Some(i) = pick {
                    self.pick_downgrade(i);
                }
            }
            KeyCode::Char(c @ '1'..='9') if self.search_result.is_some() => {
                let pick = c
                    .to_digit(10)
                    .and_then(|d| usize::try_from(d).ok())
                    .and_then(|d| d.checked_sub(1))
                    .and_then(|i| self.search_result.as_ref()?.hits.get(i))
                    .map(|h| h.name.clone());
                if let Some(name) = pick {
                    self.input = Some(Input {
                        purpose: InputPurpose::Install,
                        buffer: name,
                    });
                }
            }
            KeyCode::Char('i') => self.open_input(InputPurpose::Install),
            KeyCode::Enter | KeyCode::Char('u') => {
                if let Some(name) = self.selected_name() {
                    self.queue(PendingAction::Update(name));
                }
            }
            // No gate at all: `update --all` reads the manifest and asks the
            // index itself — a stale "0 updates" must not stop a command that
            // would find two. If there is nothing to do, the command says so.
            KeyCode::Char('U') => self.queue(PendingAction::UpdateAll),
            // Toggle from what the row shows; the command re-reads under the
            // lock, so a pin changed elsewhere is reported, not overwritten.
            KeyCode::Char('p') => self.pin_selected(),
            // The version choice happens here: a known action wanting one
            // value, which is what this interface is for. The build that
            // follows runs in the panel, with `c` and the warning record
            // the terminal handoff could not give it.
            KeyCode::Char('D') => self.start_downgrade(),
            KeyCode::Char('x') => self.remove_selected(),
            KeyCode::Char('B') => self.jump_to_other_prefix(),
            // Migrate to the other prefix of the known pair — and only there: a
            // custom prefix has the CLI's explicit --to, not a TUI path picker.
            // The gate is symmetric (see `other_known_prefix`).
            // Both migrate keys live in their own methods: the dispatch table
            // stays a table.
            KeyCode::Char('m') => self.migrate_selected(),
            KeyCode::Char('M') => self.migrate_everything(),
            _ => {}
        }
    }

    /// `m`: migrate the selected crate; the plan is frozen from the row
    /// on screen.
    fn migrate_selected(&mut self) {
        // Cloned out of the borrow: the advisory precheck below needs
        // `&mut self`.
        if let Some(row) = self.selected_row().cloned() {
            match self.known_pair_dest() {
                Some(dest) => {
                    // "Already on the other side" is a plain error line, decided on a
                    // *fresh* advisory read — the row's cached `[also in ...]` is a
                    // lockless snapshot that can be minutes stale. Advisory both ways: a
                    // racing install falls through to migrate_one's authoritative
                    // refusal, surfacing as Failed — the window accepted over a typed
                    // variant.
                    if self.destination_occupied(&dest, &row.name) {
                        return;
                    }
                    // Frozen here, from the row on screen: the plan confirmed is byte
                    // for byte the plan revalidated.
                    let snap = match crate::MigrationSnapshot::from_parts(
                        &row.name,
                        &row.version,
                        row.bins.clone(),
                        row.locked,
                        row.pinned,
                    ) {
                        Ok(snap) => snap,
                        Err(e) => {
                            self.error(&format!("{e:#}"));
                            return;
                        }
                    };
                    // The prompt is the contract: for a pinned crate the
                    // shown version is what the destination gets, for an
                    // unpinned one it is the source state being retired —
                    // and the difference is said out loud, because
                    // "1.2.3 here, latest there" is exactly what the
                    // person may want to veto.
                    let prompt = format!(
                        "migrate {} {}: {} -> {}? {} there, then retired here [y/N]",
                        row.name,
                        row.version,
                        self.prefix.display(),
                        dest.display(),
                        if row.pinned {
                            "the exact pinned version is rebuilt"
                        } else {
                            "the latest version is installed"
                        }
                    );
                    self.confirm = Some(Confirm::new(
                        &prompt,
                        OnConfirm::Migrate {
                            name: row.name.clone(),
                            version: row.version.clone(),
                            dest,
                            snap,
                        },
                    ));
                }
                None => self.info(
                    "TUI migrate covers the /usr/local <-> ~/.local pair; \
                     migrate elsewhere via the CLI: cargo lbin migrate NAME --to PREFIX",
                ),
            }
        }
    }

    /// The whole prefix: `migrate --all` as a queue of the single
    /// migrations `m` runs. Frozen here, one snapshot per row, complete
    /// or not at all — silently dropping an unparseable row would migrate
    /// a different set than confirmed.
    fn migrate_everything(&mut self) {
        let Some(dest) = self.known_pair_dest() else {
            self.info(
                "TUI migrate covers the /usr/local <-> ~/.local pair; \
                 migrate elsewhere via the CLI: cargo lbin migrate --all --to PREFIX",
            );
            return;
        };
        // `--all` means all crates *now*, not as of the last reload: a crate
        // installed since would be silently absent, and no checkpoint can
        // reject a member the plan never had. Races after this moment belong
        // to the per-crate revalidation.
        match self.reload() {
            Err(e) => {
                self.error(&format!("cannot plan the batch: {e:#}"));
                return;
            }
            // Degraded is not "nothing to migrate": the count is unknown.
            Ok(ReloadOutcome::Degraded) => return,
            Ok(ReloadOutcome::Loaded) => {}
        }
        if self.rows.is_empty() {
            self.info("nothing to migrate");
            return;
        }
        let mut plan = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            match crate::MigrationSnapshot::from_parts(
                &row.name,
                &row.version,
                row.bins.clone(),
                row.locked,
                row.pinned,
            ) {
                Ok(snap) => plan.push(PendingMigrate {
                    name: row.name.clone(),
                    version: row.version.clone(),
                    dest: dest.clone(),
                    snap,
                }),
                Err(e) => {
                    self.error(&format!("{e:#}"));
                    return;
                }
            }
        }
        let prompt = format!(
            "migrate all {} crate(s): {} -> {}? pinned crates keep their exact \
             version, unpinned get the latest, then retired here; c cancels the \
             batch [y/N]",
            plan.len(),
            self.prefix.display(),
            dest.display()
        );
        self.confirm = Some(Confirm::new(&prompt, OnConfirm::MigrateAll { dest, plan }));
    }

    /// `x`: remove the selected crate, after the usual confirmation.
    fn remove_selected(&mut self) {
        if let Some(row) = self.selected_row() {
            let prompt = format!("remove {} ({})? [y/N]", row.name, row.bins.join(", "));
            self.confirm = Some(Confirm::new(
                &prompt,
                OnConfirm::Remove {
                    name: row.name.clone(),
                },
            ));
        }
    }

    /// `p`: pin or unpin — a manifest write, so the same shape decision
    /// as `x`, made at the keypress (its own `y`). Escalation queues the
    /// handoff; otherwise in place — the most trivial mutation least
    /// deserves a screen flip. The running guard is the in-place
    /// family's.
    fn pin_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let name = row.name.clone();
        let pinned = !row.pinned;
        if self.anything_running() {
            self.error("an operation is running or queued; finish or cancel it first");
            return;
        }
        let policy = crate::privileged::Policy::for_prefix(&self.prefix);
        // The state half only: a pin never touches bin, and a read-only bin
        // must not force a handoff for it.
        let escalate = match crate::state_needs_privilege(policy, &self.prefix) {
            Ok(escalate) => escalate,
            Err(e) => {
                self.error(&format!("{e:#}"));
                return;
            }
        };
        if escalate {
            self.queue(PendingAction::SetPinned { name, pinned });
            return;
        }
        let verb = if pinned { "pinned" } else { "unpinned" };
        match crate::tui_set_pinned(&self.prefix, &name, pinned) {
            Ok(crate::TuiSetPinned::Set { version }) => {
                match self.reload() {
                    Err(e) => {
                        self.error(&format!("{verb} {name}, but the reload failed: {e:#}"));
                        return;
                    }
                    // The pin landed and then the manifest would not load back — the
                    // degraded message is the headline.
                    Ok(ReloadOutcome::Degraded) => return,
                    Ok(ReloadOutcome::Loaded) => {}
                }
                self.info(&format!("{verb} {name} at {version}"));
            }
            Ok(crate::TuiSetPinned::Already) => {
                // The manifest already agrees — the row was stale; reload so the
                // screen agrees too.
                match self.reload() {
                    Err(e) => {
                        self.error(&format!("{e:#}"));
                        return;
                    }
                    // Broken between the write and this read: the degraded message is
                    // what stands; "already pinned" would cover the one line that
                    // matters.
                    Ok(ReloadOutcome::Degraded) => return,
                    Ok(ReloadOutcome::Loaded) => {}
                }
                self.info(&format!("{name} is already {verb}"));
            }
            Ok(crate::TuiSetPinned::PrefixBusy) => {
                self.info("the prefix is busy (another cargo-lbin holds its lock); try again");
            }
            Err(e) => self.error(&format!("{verb} failed for `{name}`: {e:#}")),
        }
    }

    /// The decision at the `y`: the same escalation test as the build
    /// preflight. Escalation = terminal handoff (passwords belong on the
    /// real terminal); none = in place. Nonblocking lock — a busy prefix
    /// is an answer, not a frozen interface.
    fn remove_confirmed(&mut self, name: String) {
        // The handoff inherits queue()'s guard; the in-place path refuses
        // for the running family itself — the lock is free while cargo
        // compiles, so a removal could win it and be silently undone by the
        // worker's placement.
        if self.anything_running() {
            self.error("an operation is running or queued; finish or cancel it first");
            return;
        }
        let policy = crate::privileged::Policy::for_prefix(&self.prefix);
        let escalate = match crate::placement_needs_privilege(policy, &self.prefix) {
            Ok(escalate) => escalate,
            Err(e) => {
                self.error(&format!("{e:#}"));
                return;
            }
        };
        if escalate {
            self.queue(PendingAction::Remove(name));
            return;
        }
        match crate::tui_remove_one(&self.prefix, &name) {
            Ok(crate::TuiRemove::Removed(bins)) => {
                match self.reload() {
                    Err(e) => {
                        self.error(&format!("removed {name}, but the reload failed: {e:#}"));
                        return;
                    }
                    Ok(ReloadOutcome::Degraded) => return,
                    Ok(ReloadOutcome::Loaded) => {}
                }
                self.info(&format!("removed {name} ({})", bins.join(", ")));
            }
            Ok(crate::TuiRemove::PrefixBusy) => {
                self.info("the prefix is busy (another cargo-lbin holds its lock); try again");
            }
            Err(e) => self.error(&format!("removing `{name}` failed: {e:#}")),
        }
    }

    /// `B`: jump to the other prefix of the known pair — the same
    /// symmetric gate as m/M, no path picker.
    fn jump_to_other_prefix(&mut self) {
        match self.known_pair_dest() {
            Some(dest) => self.jump_to_prefix(dest),
            None => self.info(
                "TUI prefix switching covers the /usr/local <-> ~/.local pair; \
                 run with --prefix for anything else",
            ),
        }
    }

    /// The mechanics of `B`, separate from its gate so each is testable
    /// alone. Everything transient is anchored to `self.prefix`, so the
    /// jump refuses while anything runs — switching under a running check
    /// would lie on every surface. The switch commits on any read leaving
    /// a presentable state — loaded, or degraded (the person may be
    /// jumping there to audit); only a hard failure (the lock) rolls
    /// back. The selection follows the selected crate by name when
    /// visible on the other side; no stronger promise — one that holds
    /// only sometimes is a lie with good days.
    fn jump_to_prefix(&mut self, dest: PathBuf) {
        if self.anything_running() {
            self.error("an operation is running or queued; finish or cancel it first");
            return;
        }
        let keep = self.selected_name();
        // reload() dismisses the search panel itself, so its
        // transactionality is by hand: saved before the attempt, restored
        // after a rollback — a jump that did not happen must not cost the
        // hits. On success both saved panels drop: they were the old
        // prefix's.
        let search = self.search_result.take();
        let back = std::mem::replace(&mut self.prefix, dest);
        // Only hard failures roll back — a broken manifest lands degraded
        // instead: the person may be jumping there precisely to press v.
        let outcome = match self.reload() {
            Ok(outcome) => outcome,
            Err(e) => {
                let failed = std::mem::replace(&mut self.prefix, back);
                // Best-effort: this read succeeded moments ago; the error below
                // still names the real problem.
                let _ = self.reload();
                self.search_result = search;
                self.error(&format!(
                    "cannot read {}: {e:#} — staying here",
                    failed.display()
                ));
                return;
            }
        };
        self.build_report = None;
        self.selected = keep
            .and_then(|name| self.visible().iter().position(|row| row.name == name))
            .unwrap_or(0);
        match outcome {
            ReloadOutcome::Loaded => self.info(&format!("now at {}", self.prefix.display())),
            // The degraded message *is* the arrival announcement — "now at" over
            // it would bury the one thing worth knowing.
            ReloadOutcome::Degraded => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if matches!(key.code, KeyCode::Char('y' | 'Y')) {
            match confirm.action {
                OnConfirm::Remove { name } => self.remove_confirmed(name),
                OnConfirm::Migrate {
                    name,
                    version,
                    dest,
                    snap,
                } => {
                    self.pending_migrate = Some(PendingMigrate {
                        name,
                        version,
                        dest,
                        snap,
                    });
                }
                OnConfirm::MigrateAll { dest, plan } => {
                    let mut queue: std::collections::VecDeque<PendingMigrate> =
                        plan.into_iter().collect();
                    let total = queue.len();
                    let Some(first) = queue.pop_front() else {
                        return;
                    };
                    self.migrate_batch = Some(MigrateBatch {
                        dest,
                        queue,
                        total,
                        moved: 0,
                        warned: Vec::new(),
                        failed: Vec::new(),
                        noticed: Vec::new(),
                        reload_error: None,
                    });
                    self.pending_migrate = Some(first);
                }
            }
        } else {
            self.info("cancelled");
        }
    }

    fn on_key_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                if let Some(Input { purpose, buffer }) = self.input.take() {
                    self.submit_input(purpose, &buffer);
                }
            }
            KeyCode::Backspace => {
                if let Some(input) = self.input.as_mut() {
                    input.buffer.pop();
                }
            }
            KeyCode::Char(c) if !c.is_control() => {
                if let Some(input) = self.input.as_mut() {
                    input.buffer.push(c);
                }
            }
            _ => {}
        }
    }

    fn submit_input(&mut self, purpose: InputPurpose, buffer: &str) {
        match purpose {
            InputPurpose::Install => match parse_install_input(buffer) {
                // One crate builds in place; a batch is a longer conversation and
                // keeps the handoff.
                Ok((crates, locked)) if crates.len() == 1 => {
                    let spec = crates.into_iter().next().expect("len checked");
                    self.info(&format!("building {spec}…"));
                    self.pending_build = Some(PendingBuild {
                        spec,
                        locked,
                        expect: None,
                    });
                }
                Ok((crates, locked)) => self.queue(PendingAction::Install { crates, locked }),
                Err(e) => self.error(&format!("{e:#}")),
            },
            InputPurpose::Search => match parse_search_input(buffer) {
                Ok(query) => self.start_search(query),
                Err(e) => self.error(&format!("{e:#}")),
            },
        }
    }

    fn open_input(&mut self, purpose: InputPurpose) {
        if self.job.is_some() {
            self.error("busy; wait for the current job to finish");
            return;
        }
        self.input = Some(Input {
            purpose,
            buffer: String::new(),
        });
    }

    /// Queues a terminal-taking command behind a notice, so the notice
    /// renders before the screen is handed over.
    fn queue(&mut self, action: PendingAction) {
        if self.job.is_some() {
            self.error("busy; wait for the current job to finish");
            return;
        }
        self.info(&format!("running {}…", action_label(&action)));
        self.pending = Some(action);
    }

    /// `r`: `checkupdate` on a thread; the report is written on the main
    /// thread. `v`: the read-only audit on a worker — a fresh audit
    /// supersedes whatever report was up.
    fn start_verify(&mut self) {
        if self.job.is_some() {
            self.error("busy; wait for the current operation to finish");
            return;
        }
        self.build_report = None;
        // The panel shows one thing at a time, and a verify is about to
        // want it.
        self.downgrade_choice = None;
        let (tx, rx) = mpsc::channel();
        let prefix = self.prefix.clone();
        thread::spawn(move || {
            // The lock-wait notice is a noop on purpose: the spinner already
            // says a job is alive, and a worker's eprintln beneath the alternate
            // screen is what the quiet design forbids. The CLI passes stderr —
            // the same channel split as acquire_with.
            let _ = tx.send(crate::verify_prefix(&prefix, &mut |_| {}));
        });
        self.job = Some(Job::Verify {
            rx,
            started: std::time::Instant::now(),
            cancel: cancel_flag(),
        });
    }

    /// The CLI's severity split in the TUI's shape: errors are a failed
    /// sticky panel, warnings a non-failed one, a clean prefix one footer
    /// line. Same finding texts as the CLI — the surfaces cannot disagree
    /// about what, only where.
    fn finish_verify(&mut self, result: Result<crate::VerifyReport>) {
        let report = match result {
            Ok(report) => report,
            Err(e) => {
                self.error(&format!("verify failed: {e:#}"));
                return;
            }
        };
        if report.errors.is_empty() && report.warnings.is_empty() {
            // Zero errors implies a counted manifest (None only on the early
            // error return).
            let crates = report.crates.unwrap_or(0);
            self.info(&format!("verify: ok — {crates} managed crate(s)"));
            return;
        }
        let failed = !report.errors.is_empty();
        // No sanitize: VerifyReport is the one boundary — a second pass
        // would suggest the first is optional.
        let mut lines: Vec<String> = report.errors.iter().map(|f| f.message.clone()).collect();
        if !report.warnings.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("warnings:".to_owned());
            lines.extend(report.warnings.iter().map(|f| f.message.clone()));
        }
        let title = if failed {
            format!(
                "verify: {} verification error(s), {} warning(s)",
                report.errors.len(),
                report.warnings.len()
            )
        } else {
            format!("verify: {} warning(s)", report.warnings.len())
        };
        self.pin_report(BuildReport {
            title,
            lines,
            failed,
        });
        if failed {
            // The footer does not guess either: an uncounted manifest reports
            // errors without inventing a count.
            let head = match report.crates {
                Some(crates) => format!("verify: {crates} crate(s), "),
                None => "verify: ".to_owned(),
            };
            self.error(&format!(
                "{head}{} error(s) — details in the panel; Esc/Enter dismisses",
                report.errors.len()
            ));
        } else {
            let crates = report.crates.unwrap_or(0);
            self.warn(&format!(
                "verify: {crates} crate(s) ok, {} warning(s) in the panel; \
                 Esc/Enter dismisses",
                report.warnings.len()
            ));
        }
    }

    fn start_check(&mut self) {
        if self.job.is_some() {
            self.error("busy; wait for the current lookup to finish");
            return;
        }
        match self.reload() {
            Err(e) => {
                self.error(&format!("reload failed: {e:#}"));
                return;
            }
            // Degraded is not "nothing installed" — the count is unknown, and
            // zero here would overwrite the reload's honesty one line later.
            Ok(ReloadOutcome::Degraded) => return,
            Ok(ReloadOutcome::Loaded) => {}
        }
        if self.rows.is_empty() {
            self.info("nothing installed; nothing to check");
            return;
        }
        let entries: BTreeMap<String, Entry> = self
            .rows
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    Entry {
                        version: r.version.clone(),
                        bins: r.bins.clone(),
                        locked: r.locked,
                        pinned: r.pinned,
                    },
                )
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        let cancel = cancel_flag();
        let token = cancel.clone();
        thread::spawn(move || {
            // Consulted between index requests: a cancel stops after the
            // round-trip already in flight, never pays for another.
            let _ = tx.send(crate::check_versions(&entries, || {
                token.load(std::sync::atomic::Ordering::Relaxed)
            }));
        });
        self.job = Some(Job::Check { rx, cancel });
        self.message = None;
    }

    /// `D`: ask the index which older versions exist for the selected
    /// crate. Read-only and cancellable, like every other one-shot — the
    /// mutation is the build that a chosen version starts, and nothing
    /// is chosen yet.
    fn start_downgrade(&mut self) {
        if self.anything_running() {
            self.error("busy; wait for the current job to finish");
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        // A pinned crate is offered the list like any other: the pin is a
        // standing instruction and an exact version is how it is
        // restated, which is exactly what a digit supplies — the CLI
        // command has always allowed this.
        let (name, current) = (row.name.clone(), row.version.clone());
        let Ok(parsed) = Version::parse(&current) else {
            self.error(&format!(
                "manifest holds unparsable version for `{name}`; verify says more"
            ));
            return;
        };
        self.search_result = None;
        self.downgrade_choice = None;
        let (tx, rx) = mpsc::channel();
        let lookup = name.clone();
        thread::spawn(move || {
            let candidates = crate::index::releases(&lookup)
                .and_then(|r| r.ok_or_else(|| crate::index::not_found(&lookup)))
                .map(|releases| crate::index::downgrade_candidates(&releases, &parsed));
            let _ = tx.send(candidates);
        });
        self.job = Some(Job::Downgrade {
            name,
            current,
            rx,
            cancel: cancel_flag(),
        });
        self.message = None;
    }

    /// The offer answered: install that exact version, in the panel.
    ///
    /// The version was chosen relative to the one the row showed, and
    /// that premise travels with the request rather than being checked
    /// here — the rows are a snapshot, so the statement is made where
    /// it can be true: under the exclusive lock, in
    /// `tui_downgrade_one`. Pinning is not decided here either:
    /// `install NAME@VERSION` pins an exact version, and this is that
    /// command with the version supplied by a keypress.
    fn pick_downgrade(&mut self, index: usize) {
        // An offer can outlive the lookup that produced it: nothing stops
        // the person from pressing `v` while it sits there, and the run
        // loop starts a queued build before it polls the running job. So
        // the single-flight rule is checked here too, not only where the
        // lookup began — a build written over a live `Job` would leave
        // its worker sending into a receiver nobody holds.
        if self.anything_running() {
            self.error("busy; wait for the current job to finish");
            return;
        }
        let Some(choice) = &self.downgrade_choice else {
            return;
        };
        let Some(version) = choice.versions.get(index) else {
            return;
        };
        let (name, current, version) = (
            choice.name.clone(),
            choice.current.clone(),
            version.to_string(),
        );
        self.downgrade_choice = None;
        self.info(&format!("downgrading {name} {current} -> {version}"));
        // The run loop owns the terminal, and a build needs it for the
        // escalation preflight: the same door the install line uses.
        // `current` rides along as the premise — the worker re-checks it
        // under the exclusive lock, because the rows are a snapshot and
        // the manifest is the authority.
        self.pending_build = Some(PendingBuild {
            spec: format!("{name}@{version}"),
            locked: false,
            expect: Some(current),
        });
    }

    /// The offer, or the reason there is none.
    fn finish_downgrade(&mut self, name: String, current: String, candidates: Vec<Version>) {
        if candidates.is_empty() {
            self.info(&format!(
                "{name} {current} is installed; no older version to go back to"
            ));
            return;
        }
        let older = candidates.len().saturating_sub(DOWNGRADE_CHOICES);
        let versions: Vec<Version> = candidates.into_iter().take(DOWNGRADE_CHOICES).collect();
        let n = versions.len();
        self.downgrade_choice = Some(DowngradeChoice {
            name,
            current,
            versions,
            older,
        });
        self.info(&format!("1-{n} installs, Esc dismisses"));
    }

    fn start_search(&mut self, query: String) {
        // A failed search must not leave the previous query's hits under a
        // footer about a different one, and the panel shows one thing at
        // a time.
        self.search_result = None;
        self.downgrade_choice = None;
        let (tx, rx) = mpsc::channel();
        let q = query.clone();
        thread::spawn(move || {
            let _ = tx.send(api::search(&q, SEARCH_HITS));
        });
        self.job = Some(Job::Search {
            query,
            rx,
            cancel: cancel_flag(),
        });
        self.message = None;
    }

    /// Collects finished background work. A dropped sender means a worker
    /// panicked — ratatui's hook already restored the terminal, so the
    /// loop returns the error rather than scribble over the shell.
    // One collector, every job kind in one match: the Build arm is long
    // because a build speaks many message kinds, and splitting the match
    // would scatter the single-flight story across helpers (the same
    // trade the too_many_arguments allows make at the install sites).
    #[allow(clippy::too_many_lines)]
    fn poll_job(&mut self) -> Result<()> {
        let Some(job) = self.job.take() else {
            return Ok(());
        };
        match job {
            Job::Check { rx, cancel } => match rx.try_recv() {
                Ok(result) => {
                    // The collector is the last cancel boundary: the flag
                    // can flip while the final request is in flight, and
                    // the worker's completed answer — Ok(Some(...)) or an
                    // Err alike — arrives after the person already said
                    // no. Persisting that report, or reporting "failed",
                    // would both contradict the cancel the UI accepted.
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        self.info("update check cancelled");
                    } else {
                        self.finish_check(result);
                    }
                }
                Err(TryRecvError::Empty) => self.job = Some(Job::Check { rx, cancel }),
                Err(TryRecvError::Disconnected) => {
                    bail!("update check worker aborted; the terminal was reset by the panic")
                }
            },
            Job::Verify {
                rx,
                started,
                cancel,
            } => match rx.try_recv() {
                Ok(result) => {
                    // The audit ran to completion either way; a requested
                    // cancel discards the report instead of pinning it.
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        self.info("verify cancelled — the result was discarded");
                    } else {
                        self.finish_verify(result);
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.job = Some(Job::Verify {
                        rx,
                        started,
                        cancel,
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    bail!("verify worker aborted; the terminal was reset by the panic")
                }
            },
            Job::Downgrade {
                name,
                current,
                rx,
                cancel,
            } => match rx.try_recv() {
                Ok(result) => {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        self.info(&format!("version lookup for `{name}` cancelled"));
                    } else {
                        match result {
                            Ok(candidates) => self.finish_downgrade(name, current, candidates),
                            Err(e) => self.error(&format!("{e:#}")),
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.job = Some(Job::Downgrade {
                        name,
                        current,
                        rx,
                        cancel,
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    bail!("version lookup worker aborted; the terminal was reset by the panic")
                }
            },
            Job::Search { query, rx, cancel } => match rx.try_recv() {
                Ok(result) => {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        self.info(&format!("search for `{query}` cancelled"));
                    } else {
                        self.finish_search(query, result);
                    }
                }
                Err(TryRecvError::Empty) => self.job = Some(Job::Search { query, rx, cancel }),
                Err(TryRecvError::Disconnected) => {
                    bail!("search worker aborted; the terminal was reset by the panic")
                }
            },
            Job::Build {
                name,
                rx,
                auth_tx,
                mut units_started,
                mut current,
                mut tail,
                mut status_note,
                mut warnings,
                started,
                mut needs_auth,
                control,
                kind,
                cancel_deadline,
            } => {
                // Drain everything since the last frame: one line per tick would lag
                // a fast build by minutes.
                let mut done: Option<BuildOutcome> = None;
                loop {
                    match rx.try_recv() {
                        Ok(BuildMsg::Cargo(line)) => {
                            match progress::parse_line(&line) {
                                progress::BuildEvent::Compiling { name, version } => {
                                    units_started += 1;
                                    current = Some(format!("{name} {version}"));
                                }
                                // Compilation is over; placement and commit are not compiling-foo,
                                // and the gauge must not claim they are.
                                progress::BuildEvent::Finished => current = None,
                                _ => {}
                            }
                            // cargo speaking again supersedes a notice.
                            status_note = None;
                            tail.push_back(line);
                            if tail.len() > BUILD_TAIL {
                                tail.pop_front();
                            }
                        }
                        Ok(BuildMsg::Notice(line)) => {
                            status_note = Some(line.clone());
                            tail.push_back(line);
                            if tail.len() > BUILD_TAIL {
                                tail.pop_front();
                            }
                        }
                        // Warnings live in `warnings` alone; a copy in the tail would print
                        // twice under a one-line placement error. They also reach the
                        // panel on receipt: the worker emits them before the work they
                        // describe, and a warning held until Done is a post-mortem. On
                        // receipt, not before the build — this loop drains what has
                        // arrived, and a cached build can put Warning and Done in the
                        // same drain.
                        Ok(BuildMsg::Warning(line)) => {
                            warnings.push(line);
                            self.show_live_warnings(&name, &kind, &warnings);
                        }
                        Ok(BuildMsg::NeedAuth(target)) => needs_auth = Some(target),
                        Ok(BuildMsg::Done(outcome)) => {
                            done = Some(outcome);
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            bail!("build worker aborted; the terminal was reset by the panic")
                        }
                    }
                }
                match done {
                    Some(outcome) => {
                        self.finish_build(&name, &kind, outcome, &tail, warnings);
                        // A Ctrl-C during this build asked to leave once the worker was
                        // collected; that is now.
                        if self.quit_after_build {
                            self.should_quit = true;
                        }
                    }
                    None => {
                        self.job = Some(Job::Build {
                            name,
                            rx,
                            auth_tx,
                            units_started,
                            current,
                            tail,
                            status_note,
                            warnings,
                            started,
                            needs_auth,
                            control,
                            kind,
                            cancel_deadline,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Same semantics as `checkupdate`: reaching the index is success
    /// even if the report could not persist — shown from memory, the
    /// persistence failure a warning.
    fn finish_check(&mut self, result: Result<Option<Vec<Checked>>>) {
        let checked = match result {
            // `Ok(None)` is the worker honoring the cancel between index
            // requests: no report to build, nothing to persist — the
            // partial answer is discarded, not stored as if complete.
            Ok(None) => {
                self.info("update check cancelled");
                return;
            }
            Ok(Some(checked)) => checked,
            Err(e) => {
                self.error(&format!("update check failed: {e:#}"));
                return;
            }
        };
        let report = match Report::new(&self.prefix, checked) {
            Ok(report) => report,
            Err(e) => {
                self.error(&format!("update check failed: {e:#}"));
                return;
            }
        };
        let persisted = report.store(&self.cache);
        match self.apply_report(Some(&report)) {
            Err(e) => {
                self.error(&format!("reload failed: {e:#}"));
                return;
            }
            // "checked: 0 update(s)" over a manifest that refused to load would
            // be an invented number.
            Ok(ReloadOutcome::Degraded) => return,
            Ok(ReloadOutcome::Loaded) => {}
        }
        let n = self.updates_available();
        // The count matches the Updates tab; the pinned backlog is reported
        // alongside, never folded in.
        let held = self.pinned_outdated();
        let summary = if held > 0 {
            format!("{n} update(s) available; {held} pinned held back")
        } else {
            format!("{n} update(s) available")
        };
        match persisted {
            Ok(()) => self.info(&format!("checked: {summary}")),
            Err(e) => self.warn(&format!("checked: {summary}; report not saved: {e:#}")),
        }
    }

    /// Installed marks are read from the manifest *now*, not from the
    /// request-time rows: the mark states installation as a fact, and
    /// search has no version check to make it immune.
    fn finish_search(&mut self, query: String, result: Result<Vec<api::Hit>>) {
        match result {
            Ok(hits) if hits.is_empty() => self.info(&format!("no crates match `{query}`")),
            Ok(hits) => {
                match self.reload() {
                    Err(e) => {
                        self.error(&format!("reload failed: {e:#}"));
                        return;
                    }
                    // The hits are real, but their [installed] marks would come from a
                    // manifest that did not load; the degraded message outranks them.
                    Ok(ReloadOutcome::Degraded) => return,
                    Ok(ReloadOutcome::Loaded) => {}
                }
                let installed: BTreeMap<String, String> = self
                    .rows
                    .iter()
                    .filter(|r| hits.iter().any(|h| h.name == r.name))
                    .map(|r| (r.name.clone(), r.version.clone()))
                    .collect();
                let n = hits.len();
                self.search_result = Some(SearchResult {
                    query,
                    hits,
                    installed,
                });
                self.info(&format!("{n} hit(s); 1-{n} to install, Esc to dismiss"));
            }
            Err(e) => self.error(&format!("{e:#}")),
        }
    }

    fn selected_name(&self) -> Option<String> {
        self.selected_row().map(|r| r.name.clone())
    }

    fn select_next(&mut self) {
        let len = self.visible().len();
        if len > 0 && self.selected + 1 < len {
            self.selected += 1;
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// The one door to the sticky panel: pinning resets the scroll — a
    /// leftover offset would open the next report in its middle.
    fn pin_report(&mut self, report: BuildReport) {
        self.report_scroll = 0;
        self.build_report = Some(report);
    }

    fn info(&mut self, text: &str) {
        self.notify(text, MessageKind::Info);
    }

    fn warn(&mut self, text: &str) {
        self.notify(text, MessageKind::Warning);
    }

    fn error(&mut self, text: &str) {
        self.notify(text, MessageKind::Error);
    }

    fn notify(&mut self, text: &str, kind: MessageKind) {
        // Span-bound like everything else; a reload error carries paths too.
        self.message = Some(Message {
            text: crate::text::sanitize(text),
            kind,
        });
    }
}

fn action_label(action: &PendingAction) -> String {
    match action {
        PendingAction::Update(name) => format!("update {name}"),
        PendingAction::Downgrade(name) => format!("downgrade {name}"),
        PendingAction::UpdateAll => "update --all".to_owned(),
        PendingAction::Install { crates, locked } => {
            let mut label = format!("install {}", crates.join(" "));
            if *locked {
                label.push_str(" --locked");
            }
            label
        }
        PendingAction::Remove(name) => format!("remove {name}"),
        PendingAction::SetPinned { name, pinned: true } => format!("pin {name}"),
        PendingAction::SetPinned {
            name,
            pinned: false,
        } => format!("unpin {name}"),
    }
}

/// mm:ss, rolling to h:mm:ss — the widest field grows instead of
/// wrapping.
fn format_elapsed(d: std::time::Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Manifest entries joined with the report, consulted per installed
/// version: a crate changed since the check comes out `Unknown`, not
/// stale.
fn rows_from(
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
                also: also.get(name).cloned().unwrap_or_default(),
                status,
            }
        })
        .collect()
}

/// `i` input: crate specs, `--locked` anywhere — the CLI's shape.
/// Validated here so a typo fails in the footer, not after handoff;
/// passed on as typed.
fn parse_install_input(buffer: &str) -> Result<(Vec<String>, bool)> {
    let mut crates = Vec::new();
    let mut locked = false;
    for token in buffer.split_whitespace() {
        if token == "--locked" {
            locked = true;
        } else {
            // Kept as typed, duplicates included: `parse_all` is the one place
            // that decides what a repeated crate means.
            crates.push(token.to_owned());
        }
    }
    if crates.is_empty() {
        bail!("no crate name given");
    }
    // The same validation `install` will apply, including "one crate
    // once" — refused here, in the footer.
    InstallSpec::parse_all(&crates)?;
    Ok((crates, locked))
}

/// `s` input: free-text search terms, whitespace-normalized. Not a crate
/// name, so not validated as one — "sched ext scheduler" is a fine query.
fn parse_search_input(buffer: &str) -> Result<String> {
    let query = buffer.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        bail!("no search terms given");
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Checked;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn manifest(entries: &[(&str, &str)]) -> Manifest {
        let mut m = Manifest::default();
        for (name, version) in entries {
            m.crates.insert(
                (*name).to_owned(),
                Entry {
                    version: (*version).to_owned(),
                    bins: vec![(*name).to_owned()],
                    locked: false,
                    pinned: false,
                },
            );
        }
        m
    }

    #[test]
    fn rows_carry_three_way_status() {
        let m = manifest(&[("bat", "0.26.0"), ("fd", "10.3.0"), ("ripgrep", "14.1.1")]);
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
                // Checked against an older version: fd was updated since.
                Checked {
                    name: "fd".to_owned(),
                    current: v("10.2.0"),
                    latest: v("10.3.0"),
                },
            ],
        )
        .unwrap();
        let rows = rows_from(&m, Some(&report), &std::collections::BTreeMap::new());
        let status: Vec<(&str, &RowStatus)> =
            rows.iter().map(|r| (r.name.as_str(), &r.status)).collect();
        assert_eq!(status[0], ("bat", &RowStatus::Outdated(v("0.26.1"))));
        assert_eq!(status[1], ("fd", &RowStatus::Unknown));
        assert_eq!(status[2], ("ripgrep", &RowStatus::UpToDate));

        // No report at all: everything unknown, nothing claimed.
        let rows = rows_from(&m, None, &std::collections::BTreeMap::new());
        assert!(rows.iter().all(|r| r.status == RowStatus::Unknown));
    }

    #[test]
    fn install_input_mirrors_cli_shape() {
        assert_eq!(
            parse_install_input("ripgrep --locked bat").unwrap(),
            (vec!["ripgrep".to_owned(), "bat".to_owned()], true)
        );
        assert_eq!(
            parse_install_input("  fd  ").unwrap(),
            (vec!["fd".to_owned()], false)
        );
        assert!(parse_install_input("").is_err());
        assert!(parse_install_input("--locked").is_err());
        assert!(parse_install_input("../evil").is_err());
        // Versioned specs pass through as typed; requirements do not.
        assert_eq!(
            parse_install_input("bat@0.26.0").unwrap(),
            (vec!["bat@0.26.0".to_owned()], false)
        );
        assert!(parse_install_input("bat@^0.26").is_err());
        // One crate, two specs: refused before the handoff — including
        // identical tokens, which the CLI refuses too.
        assert!(parse_install_input("bat bat@0.26.0").is_err());
        assert!(parse_install_input("bat@0.26.0 bat@0.25.0").is_err());
        assert!(parse_install_input("bat bat").is_err());
        assert!(parse_install_input("bat@0.26.0 bat@0.26.0").is_err());
    }

    #[test]
    fn search_input_is_free_text() {
        assert_eq!(parse_search_input(" bat ").unwrap(), "bat");
        assert_eq!(
            parse_search_input("sched  ext\tscheduler").unwrap(),
            "sched ext scheduler"
        );
        assert!(parse_search_input("").is_err());
        assert!(parse_search_input("   ").is_err());
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

    #[test]
    fn the_grace_timer_is_one_shot_and_fires_only_past_the_deadline() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-grace");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();

        let control = std::sync::Arc::new(crate::BuildControl::new());
        // The deadline is armed only after an accepted cancel, so the
        // control is already Cancelling.
        assert!(matches!(
            control.request_cancel(),
            crate::CancelOutcome::Accepted
        ));
        let (_tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::clone(&control),
            kind: BuildKind::Install,
            cancel_deadline: Some(std::time::Instant::now() + Duration::from_secs(60)),
        });

        // Not due: the deadline stays armed and nothing is escalated.
        app.escalate_overdue_cancel();
        assert!(matches!(
            &app.job,
            Some(Job::Build {
                cancel_deadline: Some(_),
                ..
            })
        ));

        // Due: fires once (no live group here, so it lands on the quiet
        // "already stopping" arm) and disarms itself.
        if let Some(Job::Build {
            cancel_deadline, ..
        }) = &mut app.job
        {
            *cancel_deadline = std::time::Instant::now().checked_sub(Duration::from_millis(1));
            assert!(
                cancel_deadline.is_some(),
                "the clock is past its first millisecond"
            );
        }
        app.escalate_overdue_cancel();
        assert!(matches!(
            &app.job,
            Some(Job::Build {
                cancel_deadline: None,
                ..
            })
        ));
        // One-shot: a second pass finds nothing armed and does nothing.
        app.escalate_overdue_cancel();
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_jump_swaps_reloads_and_the_selection_follows_the_name() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-jump");
        let _ = std::fs::remove_dir_all(&root);
        let here = root.join("here");
        let there = root.join("there");
        // foo is pinned here and unpinned there: the asymmetry is what
        // makes the filter case below test the branch it claims to.
        for (prefix, crates) in [
            (&here, vec![("alpha", false), ("foo", true)]),
            (&there, vec![("foo", false), ("zeta", false)]),
        ] {
            std::fs::create_dir_all(prefix.join("bin")).unwrap();
            std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
            let mut manifest = crate::Manifest::default();
            for (name, pinned) in crates {
                manifest.crates.insert(
                    name.to_owned(),
                    crate::Entry {
                        version: "0.1.0".into(),
                        bins: vec![name.to_owned()],
                        locked: false,
                        pinned,
                    },
                );
            }
            manifest.store(prefix).unwrap();
        }
        let mut app = App::new(&here).unwrap();
        let _ = app.reload().unwrap();
        // Select foo on this side…
        let pos = app
            .visible()
            .iter()
            .position(|row| row.name == "foo")
            .unwrap();
        app.selected = pos;
        app.jump_to_prefix(there.clone());
        assert_eq!(app.prefix, there, "the jump committed");
        // …and the selection followed it to the other side.
        assert_eq!(
            app.selected_row().map(|row| row.name.as_str()),
            Some("foo"),
            "the selection follows the crate by name"
        );
        // A name with no counterpart falls back to the top.
        let pos = app
            .visible()
            .iter()
            .position(|row| row.name == "zeta")
            .unwrap();
        app.selected = pos;
        app.jump_to_prefix(here.clone());
        assert_eq!(app.prefix, here);
        assert_eq!(app.selected, 0, "no counterpart: back to the top");
        // Visible, not merely existing: the filter goes on first and the
        // position is found under it; foo is visible-here-hidden-there by
        // the seeding above.
        app.filter = Filter::Pinned;
        let pos = app
            .visible()
            .iter()
            .position(|row| row.name == "foo")
            .expect("foo is pinned here, so the Pinned view shows it");
        app.selected = pos;
        assert_eq!(
            app.selected_name().as_deref(),
            Some("foo"),
            "precondition: the jump will carry a name, not a None — \
             without this the case degrades to the plain fallback"
        );
        app.jump_to_prefix(there.clone());
        assert_eq!(app.prefix, there);
        assert_eq!(
            app.selected, 0,
            "foo exists there but is not visible under Pinned"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The offer is a value for a chosen action: it lands, a digit
    /// answers it, and what leaves the panel is a build — not a browser
    /// with its own navigation.
    #[test]
    fn a_downgrade_offer_turns_one_keypress_into_a_pinned_build() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-offer");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(&root).unwrap();
        app.rows = rows_from(&manifest(&[("foo", "1.2.0")]), None, &BTreeMap::new());

        app.finish_downgrade("foo".into(), "1.2.0".into(), vec![v("1.1.0"), v("1.0.0")]);
        let choice = app.downgrade_choice.as_ref().expect("the offer is up");
        assert_eq!(choice.versions, vec![v("1.1.0"), v("1.0.0")]);
        assert_eq!(choice.older, 0, "nothing withheld, nothing to announce");

        app.pick_downgrade(0);
        let req = app.pending_build.as_ref().expect("a build was requested");
        assert_eq!(req.spec, "foo@1.1.0", "that exact version, nothing else");
        assert_eq!(
            req.expect.as_deref(),
            Some("1.2.0"),
            "and the premise travels with it, to be checked under the lock"
        );
        assert!(
            app.downgrade_choice.is_none(),
            "an answered offer leaves the panel"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nothing older is an answer, not an empty panel; and a long
    /// history is capped at what a digit can name, with the remainder
    /// announced rather than hidden.
    #[test]
    fn the_offer_is_capped_and_an_empty_one_is_a_message() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-cap");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(&root).unwrap();
        app.rows = rows_from(&manifest(&[("foo", "1.2.0")]), None, &BTreeMap::new());

        app.finish_downgrade("foo".into(), "1.2.0".into(), Vec::new());
        assert!(
            app.downgrade_choice.is_none(),
            "no candidates, no panel to dismiss"
        );

        let many: Vec<Version> = (0..12).map(|i| v(&format!("1.0.{i}"))).collect();
        app.finish_downgrade("foo".into(), "1.2.0".into(), many);
        let choice = app.downgrade_choice.as_ref().unwrap();
        assert_eq!(choice.versions.len(), DOWNGRADE_CHOICES, "one digit each");
        assert_eq!(choice.older, 3, "and the rest is said out loud");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Single flight, checked where the build is requested and not only
    /// where the offer began. An offer outlives its lookup, the run loop
    /// starts a queued build before polling the running job, and a
    /// `Job` written over a live one would strand its worker sending
    /// into a receiver nobody holds.
    #[test]
    fn an_offer_answered_during_another_job_starts_nothing() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-busy");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(&root).unwrap();
        app.rows = rows_from(&manifest(&[("foo", "1.2.0")]), None, &BTreeMap::new());
        app.finish_downgrade("foo".into(), "1.2.0".into(), vec![v("1.1.0")]);

        // `v` after the lookup finished: legal, and the offer is still up.
        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job::Verify {
            rx,
            started: std::time::Instant::now(),
            cancel: cancel_flag(),
        });

        app.pick_downgrade(0);
        assert!(
            app.pending_build.is_none(),
            "no build is queued over a running job"
        );
        assert!(
            matches!(app.job, Some(Job::Verify { .. })),
            "and the running job is untouched"
        );

        // With the job done, the same keypress works.
        app.job = None;
        app.pick_downgrade(0);
        assert!(app.pending_build.is_some(), "the offer still answers later");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The record says what the person did. A downgrade's panel, its
    /// messages and its failure title all speak the operation's own
    /// word — calling it an install would describe something nobody
    /// asked for, and putting the whole build into the record was the
    /// reason this moved into the panel at all.
    #[test]
    fn a_downgrade_is_recorded_as_a_downgrade() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-record");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(&root).unwrap();
        assert_eq!(BuildKind::Downgrade.verb(), "downgrade");

        let (tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Downgrade,
            cancel_deadline: None,
        });

        tx.send(BuildMsg::Warning("warning: something".into()))
            .unwrap();
        app.poll_job().unwrap();
        let live = app.build_report.as_ref().expect("warnings reach the panel");
        assert!(live.title.starts_with("downgrade foo"), "{}", live.title);

        tx.send(BuildMsg::Done(BuildOutcome::Failed(anyhow::anyhow!(
            "boom"
        ))))
        .unwrap();
        app.poll_job().unwrap();
        let report = app.build_report.as_ref().expect("a failure is pinned");
        assert_eq!(report.title, "downgrade foo failed", "{}", report.title);
        assert!(report.failed);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pinned crate is downgraded like any other — the CLI command
    /// always allowed it, and the digit supplies exactly the "name a
    /// version" the pin asks for. The pin itself is not touched here:
    /// `install NAME@VERSION` re-pins, which is the door this takes.
    #[test]
    fn a_pinned_crate_is_offered_the_list_like_any_other() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-downgrade-pinned");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(&root).unwrap();
        let mut m = manifest(&[("foo", "1.2.0")]);
        m.crates.get_mut("foo").unwrap().pinned = true;
        app.rows = rows_from(&m, None, &BTreeMap::new());

        app.finish_downgrade("foo".into(), "1.2.0".into(), vec![v("1.1.0")]);
        assert!(
            app.downgrade_choice.is_some(),
            "a pin is restated by naming a version, not a reason to refuse"
        );
        app.pick_downgrade(0);
        let req = app.pending_build.as_ref().expect("the build is requested");
        assert_eq!(req.spec, "foo@1.1.0");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_jump_refuses_while_anything_runs_and_lands_degraded_on_a_broken_manifest() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-jump-guard");
        let _ = std::fs::remove_dir_all(&root);
        let here = root.join("here");
        let broken = root.join("broken");
        std::fs::create_dir_all(here.join("share/cargo-lbin")).unwrap();
        std::fs::create_dir_all(here.join("bin")).unwrap();
        crate::Manifest::default().store(&here).unwrap();
        std::fs::create_dir_all(broken.join("share/cargo-lbin")).unwrap();
        std::fs::write(broken.join("share/cargo-lbin/manifest.json"), "not json").unwrap();

        let mut app = App::new(&here).unwrap();
        let _ = app.reload().unwrap();

        // Guarded: with a job alive the prefix stays put.
        let (_tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });
        app.jump_to_prefix(broken.clone());
        assert_eq!(app.prefix, here, "a live job holds the prefix in place");
        app.job = None;

        // A broken manifest no longer bounces the jump: the landing is
        // degraded — the person may be jumping there to press v. Rollback
        // remains for hard failures (the lock).
        app.search_result = Some(SearchResult {
            query: "foo".into(),
            hits: Vec::new(),
            installed: std::collections::BTreeMap::new(),
        });
        app.jump_to_prefix(broken.clone());
        assert_eq!(
            app.prefix, broken,
            "the jump commits — degraded, not refused"
        );
        assert!(
            app.manifest_error.is_some(),
            "the landing remembers why the list is empty"
        );
        assert!(app.visible().is_empty());
        assert!(
            app.search_result.is_none(),
            "a committed jump drops the old prefix's panel"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_confirmed_remove_runs_in_place_when_no_privilege_is_needed() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-remove");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        let mut manifest = Manifest::default();
        for name in ["foo", "bar"] {
            std::fs::write(prefix.join("bin").join(name), "#!/bin/sh\n").unwrap();
            manifest.crates.insert(
                name.to_owned(),
                Entry {
                    version: "0.1.0".into(),
                    bins: vec![name.to_owned()],
                    locked: false,
                    pinned: false,
                },
            );
        }
        manifest.store(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();
        let _ = app.reload().unwrap();

        // A user-writable prefix: the decision lands in place — no
        // terminal handoff is queued, the file and the row are gone,
        // and the interface says so itself.
        // Retried, because "busy" is a legitimate answer and this binary
        // can produce it against itself: the prefix lock this test takes
        // while reloading is inheritable across a sibling test's
        // fork-to-exec window, and the removal's exclusive attempt is
        // non-blocking by design — it reports busy rather than waiting.
        // Production says "try again" to a person; the test does.
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                app.remove_confirmed("foo".into());
                !prefix.join("bin/foo").exists()
            }),
            "the binary is gone"
        );
        assert!(
            app.pending.is_none(),
            "no handoff for a passwordless prefix"
        );
        assert!(
            app.visible().iter().all(|row| row.name != "foo"),
            "the row is gone from the reloaded list"
        );
        let said = app.message.take().expect("the removal reports itself");
        assert!(
            said.text.contains("removed foo"),
            "the interface owns the words: {}",
            said.text
        );

        // Mid-job the confirm is reachable, so the in-place path refuses for
        // itself: a removal winning the free lock would be silently undone
        // by the worker's placement.
        let (_tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "bar".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });
        app.remove_confirmed("bar".into());
        assert!(
            prefix.join("bin/bar").exists(),
            "a running job holds every in-place mutation off the prefix"
        );
        app.job = None;
        app.message = None;

        // What earns the union over a bare job check: a batch between
        // members has an empty job slot and a live queue.
        app.migrate_batch = Some(MigrateBatch {
            dest: prefix.join("elsewhere"),
            queue: std::collections::VecDeque::new(),
            total: 1,
            moved: 0,
            warned: Vec::new(),
            failed: Vec::new(),
            noticed: Vec::new(),
            reload_error: None,
        });
        app.remove_confirmed("bar".into());
        assert!(
            prefix.join("bin/bar").exists(),
            "a batch with an empty job slot still holds the prefix"
        );
        app.migrate_batch = None;
        app.message = None;

        // A held lock is an answer, not a wait: the removal refuses,
        // nothing changes, and the message says busy.
        let held = StateLock::acquire(&prefix, &Mode::Exclusive).unwrap();
        app.remove_confirmed("bar".into());
        drop(held);
        assert!(
            prefix.join("bin/bar").exists(),
            "a busy prefix removes nothing"
        );
        let said = app.message.take().expect("the refusal reports itself");
        assert!(said.text.contains("busy"), "named as busy: {}", said.text);
        assert!(
            app.visible().iter().any(|row| row.name == "bar"),
            "the row survives the refusal"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_pin_flips_in_place_when_no_privilege_is_needed() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-pin");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        let mut manifest = Manifest::default();
        manifest.crates.insert(
            "foo".into(),
            Entry {
                version: "0.1.0".into(),
                bins: vec!["foo".into()],
                locked: false,
                pinned: false,
            },
        );
        manifest.store(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();
        let _ = app.reload().unwrap();
        app.selected = 0;

        // In place: no handoff queued, the bit lands on disk, the row
        // and the message agree.
        // Retried for the same reason the removal is: the in-place flip
        // takes the prefix lock without waiting, and this binary can
        // hold that lock against itself for a fork-to-exec window.
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                app.pin_selected();
                Manifest::load(&prefix).unwrap().crates["foo"].pinned
            }),
            "the pin is committed"
        );
        assert!(
            app.pending.is_none(),
            "no handoff for a passwordless prefix"
        );
        assert!(
            app.selected_row().is_some_and(|row| row.pinned),
            "the reloaded row agrees"
        );
        let said = app.message.take().expect("the pin reports itself");
        assert!(
            said.text.contains("pinned foo at 0.1.0"),
            "the interface owns the words: {}",
            said.text
        );

        // And back: the same key is its own inverse (retried alike).
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                app.pin_selected();
                !Manifest::load(&prefix).unwrap().crates["foo"].pinned
            }),
            "the unpin is committed"
        );
        let said = app.message.take().expect("the unpin reports itself");
        assert!(said.text.contains("unpinned foo"), "{}", said.text);

        // A stale row: the manifest moved underneath, so the flip finds
        // itself already answered.
        let mut moved = Manifest::load(&prefix).unwrap();
        moved.crates.get_mut("foo").unwrap().pinned = true;
        moved.store(&prefix).unwrap();
        // Retried on the *message*, not on the disk: the expected
        // outcome is a no-op, so "nothing changed" cannot tell success
        // from a transient busy. The word the interface chose can.
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                app.pin_selected();
                app.message
                    .as_ref()
                    .is_some_and(|m| m.text.contains("already pinned"))
            }),
            "named as already: {:?}",
            app.message.as_ref().map(|m| m.text.clone())
        );
        assert!(
            Manifest::load(&prefix).unwrap().crates["foo"].pinned,
            "already answered: nothing flips"
        );
        let _ = app.message.take();
        assert!(
            app.selected_row().is_some_and(|row| row.pinned),
            "the reloaded row shows the state that stands"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn build_running_answers_the_footer_truthfully() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-footer");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();
        assert!(!app.build_running(), "no job, no cancel hint");
        let (_tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });
        assert!(
            app.build_running(),
            "a running build earns the cancel hints"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_batch_tallies_advances_and_a_cancel_ends_it() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-batch");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();
        let dest = prefix.join("other");

        let pending = |name: &str| PendingMigrate {
            name: name.into(),
            version: "0.1.0".into(),
            dest: dest.clone(),
            snap: crate::MigrationSnapshot::from_parts(
                name,
                "0.1.0",
                vec![name.into()],
                false,
                false,
            )
            .unwrap(),
        };
        let target = MigrateTarget {
            version: "0.1.0".into(),
            dest: dest.clone(),
        };

        // Success advances the queue — and a successful member's warnings
        // are not laundered by the tally. The outcome's version (0.2.0)
        // deliberately differs from the plan's (0.1.0): the footer must
        // announce what the destination committed, not echo the plan.
        app.migrate_batch = Some(MigrateBatch {
            dest: dest.clone(),
            queue: [pending("bar")].into_iter().collect(),
            total: 2,
            moved: 0,
            warned: Vec::new(),
            failed: Vec::new(),
            noticed: Vec::new(),
            reload_error: None,
        });
        let no_tail = VecDeque::new();
        app.finish_batch_step(
            "foo",
            &target,
            BuildOutcome::Migrated(Version::new(0, 2, 0)),
            &no_tail,
            vec!["foo shadows something".to_owned()],
        );
        assert!(app.pending_migrate.is_some(), "the queue advanced");
        let batch = app.migrate_batch.as_ref().unwrap();
        assert_eq!(batch.moved, 1);
        assert_eq!(batch.noticed.len(), 1, "the shadow warning survived");
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("foo 0.2.0")),
            "the footer speaks the destination's version, not the plan's: {:?}",
            app.message.as_ref().map(|m| m.text.clone())
        );

        // ...a failure on the last member finalizes with a failed panel
        // carrying the successful member's warnings too...
        app.pending_migrate = None;
        app.finish_batch_step(
            "bar",
            &target,
            BuildOutcome::Failed(anyhow::anyhow!("already installed, say")),
            &no_tail,
            Vec::new(),
        );
        assert!(app.migrate_batch.is_none(), "the batch wrapped up");
        let report = app.build_report.take().expect("a shortfall gets a panel");
        assert!(report.failed);
        assert!(report.title.contains("1 of 2"));
        assert!(
            report.lines.iter().any(|l| l.contains("shadows something")),
            "the successful member's warning reached the summary panel"
        );

        // ...and a cancel ends the batch with the queue dropped.
        app.migrate_batch = Some(MigrateBatch {
            dest: dest.clone(),
            queue: [pending("baz"), pending("qux")].into_iter().collect(),
            total: 3,
            moved: 1,
            warned: Vec::new(),
            failed: Vec::new(),
            noticed: Vec::new(),
            reload_error: None,
        });
        app.finish_batch_step(
            "bar",
            &target,
            BuildOutcome::Cancelled,
            &no_tail,
            Vec::new(),
        );
        assert!(app.migrate_batch.is_none(), "a cancel ends the whole batch");
        assert!(
            app.pending_migrate.is_none(),
            "nothing was silently continued"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_broken_manifest_degrades_the_session_instead_of_ending_it() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-degraded");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::write(
            crate::Manifest::path(&prefix),
            r#"{"crates":{"a":{"version":"nope","bins":["x"]}}}"#,
        )
        .unwrap();
        // The refusal is a state to present — dying here would take the v
        // key with it.
        let mut app = App::new(&prefix).expect("degraded, not dead");
        assert!(app.manifest_error.is_some());
        assert!(app.rows.is_empty());

        // Mutating keys are refused with the reason, not forwarded to
        // workers that would bounce noisily.
        app.on_key(KeyEvent::from(KeyCode::Char('u')));
        assert!(app.confirm.is_none() && app.input.is_none());
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("repair")),
            "the refusal names the way out: {:?}",
            app.message.as_ref().map(|m| &m.text)
        );

        // v stays reachable — the whole point of degrading.
        app.on_key(KeyEvent::from(KeyCode::Char('v')));
        assert!(
            app.busy().is_some(),
            "the audit runs from the degraded state"
        );

        // ...and so is its cancel door: the gate must not swallow c and
        // answer a cancel with repair instructions.
        app.on_key(KeyEvent::from(KeyCode::Char('c')));
        let Some(Job::Verify { cancel, .. }) = &app.job else {
            panic!("verify still owns the slot");
        };
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "c reached the running verify through the degraded gate"
        );
        app.job = None; // the verify worker's slot, released for the test

        // ? opens the degraded page: the gate lets the key through and the
        // page must not advertise the doors the gate locked.
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(app.show_help, "help is one of the keys that still work");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.show_help);

        // B stays reachable too: the jump is how one *leaves* a broken
        // prefix, and a gate that lets you in but not out is a trap.
        app.on_key(KeyEvent::from(KeyCode::Char('B')));
        assert!(
            !app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("repair")),
            "B passed the gate: {:?}",
            app.message.as_ref().map(|m| &m.text)
        );

        // Hand-repair, then r retries the load and the session recovers.
        std::fs::write(crate::Manifest::path(&prefix), r#"{"crates":{}}"#).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('r')));
        assert!(app.manifest_error.is_none(), "{:?}", app.manifest_error);
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("loads again")),
            "recovery is announced: {:?}",
            app.message.as_ref().map(|m| &m.text)
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_mid_session_breakage_never_reports_an_invented_zero() {
        // The manifest breaks mid-session; every reload() caller must
        // present the degraded fact, not its happy-path sentence.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-midbreak");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        crate::Manifest::default().store(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();
        assert!(app.manifest_error.is_none());

        std::fs::write(crate::Manifest::path(&prefix), "not json").unwrap();
        // r is checkupdate while healthy; its reload lands degraded and must
        // not say "nothing installed": the count is unknown, not zero.
        app.on_key(KeyEvent::from(KeyCode::Char('r')));
        assert!(app.manifest_error.is_some());
        let text = app
            .message
            .as_ref()
            .map(|m| m.text.clone())
            .unwrap_or_default();
        assert!(
            !text.contains("nothing installed"),
            "the reload's honesty survived its caller: {text}"
        );
        assert!(text.contains("cannot be loaded"), "{text}");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_jump_into_degraded_announces_the_breakage_not_the_arrival() {
        let root = std::env::temp_dir().join("cargo-lbin-test-tui-jump-announce");
        let _ = std::fs::remove_dir_all(&root);
        let here = root.join("here");
        let broken = root.join("broken");
        std::fs::create_dir_all(here.join("share/cargo-lbin")).unwrap();
        std::fs::create_dir_all(here.join("bin")).unwrap();
        crate::Manifest::default().store(&here).unwrap();
        std::fs::create_dir_all(broken.join("share/cargo-lbin")).unwrap();
        std::fs::write(broken.join("share/cargo-lbin/manifest.json"), "not json").unwrap();
        let mut app = App::new(&here).unwrap();
        app.jump_to_prefix(broken.clone());
        assert_eq!(app.prefix, broken);
        let text = app
            .message
            .as_ref()
            .map(|m| m.text.clone())
            .unwrap_or_default();
        assert!(
            !text.contains("now at"),
            "\"now at\" would bury the one thing worth knowing: {text}"
        );
        assert!(text.contains("cannot be loaded"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_check_result_arriving_after_the_cancel_is_discarded() {
        // The race the collector boundary closes: the flag flips while
        // the last request is in flight, the worker's completed answer
        // arrives anyway — Ok(Some) and Err alike. Persisting that
        // report, or saying "failed", would contradict the cancel the UI
        // already accepted.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-check-race");
        let _ = std::fs::create_dir_all(prefix.join("share/cargo-lbin"));
        let _ = std::fs::create_dir_all(prefix.join("bin"));
        let mut app = App::new(&prefix).unwrap();
        app.cache = prefix.join("cache-isolated");

        // Ok(Some) after the flag: discarded, nothing persisted.
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Check {
            rx,
            cancel: cancel_flag(),
        });
        app.on_key(KeyEvent::from(KeyCode::Char('c')));
        tx.send(Ok(Some(Vec::new()))).unwrap();
        app.poll_job().unwrap();
        assert!(app.job.is_none(), "the slot is freed");
        let text = app.message.as_ref().unwrap().text.clone();
        assert!(text.contains("cancelled"), "{text}");
        assert!(
            !app.cache.exists(),
            "a cancelled check persists nothing — the report store was never touched"
        );

        // Err after the flag: still "cancelled", never "failed".
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Check {
            rx,
            cancel: cancel_flag(),
        });
        app.on_key(KeyEvent::from(KeyCode::Char('c')));
        tx.send(Err(anyhow::anyhow!("network died mid-flight")))
            .unwrap();
        app.poll_job().unwrap();
        let text = app.message.as_ref().unwrap().text.clone();
        assert!(
            text.contains("cancelled") && !text.contains("failed"),
            "an error after the cancel is the cancel's outcome, not a failure: {text}"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_oneshot_cancel_is_a_flag_and_a_discarded_result() {
        // The whole model in one walk: running -> c sets the flag and the
        // label says so -> the arrived result is discarded, the slot
        // freed, one cancelled note shown. No grace, no escalation.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-oneshot-cancel");
        let _ = std::fs::create_dir_all(prefix.join("share/cargo-lbin"));
        let _ = std::fs::create_dir_all(prefix.join("bin"));
        let mut app = App::new(&prefix).unwrap();
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Search {
            query: "tokio".into(),
            rx,
            cancel: cancel_flag(),
        });
        assert!(app.job.as_ref().unwrap().label().starts_with("searching"));
        app.on_key(KeyEvent::from(KeyCode::Char('c')));
        let Some(Job::Search { cancel, .. }) = &app.job else {
            panic!("the slot stays held until the worker returns");
        };
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert!(
            app.job
                .as_ref()
                .unwrap()
                .label()
                .contains("cancel requested"),
            "the label names the state, not a power the door lacks"
        );
        // The worker returns a full result; the cancel discards it.
        tx.send(Ok(Vec::new())).unwrap();
        app.poll_job().unwrap();
        assert!(app.job.is_none(), "the slot is freed");
        assert!(app.search_result.is_none(), "the result was discarded");
        let text = app.message.as_ref().unwrap().text.clone();
        assert!(text.contains("cancelled"), "{text}");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn the_report_scroll_stops_at_the_rendered_bound() {
        // The old debt: a held Down ran the offset far past the tail and
        // Up owed symmetric presses; the renderer's written-back bound
        // now clamps at the keypress, and End/Home jump to it and back.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-clamp");
        let _ = std::fs::create_dir_all(prefix.join("share/cargo-lbin"));
        let _ = std::fs::create_dir_all(prefix.join("bin"));
        let mut app = App::new(&prefix).unwrap();
        app.pin_report(BuildReport {
            title: "x".into(),
            lines: vec!["l".into(); 40],
            failed: true,
        });
        app.report_scroll_max.set(9); // what a draw of this report wrote back
        for _ in 0..50 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        assert_eq!(app.report_scroll, 9, "Down cannot outrun the bound");
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.report_scroll, 8, "Up answers on the first press");
        app.on_key(KeyEvent::from(KeyCode::Home));
        assert_eq!(app.report_scroll, 0);
        app.on_key(KeyEvent::from(KeyCode::End));
        assert_eq!(app.report_scroll, 9);
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn a_fresh_report_opens_at_its_headline() {
        // Pinning the next report must zero the scroll, or it would open in
        // the middle.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-scroll");
        let _ = std::fs::create_dir_all(prefix.join("share/cargo-lbin"));
        let _ = std::fs::create_dir_all(prefix.join("bin"));
        let mut app = App::new(&prefix).unwrap();
        app.report_scroll = 7;
        app.finish_verify(Ok(crate::VerifyReport {
            crates: Some(1),
            errors: vec![crate::Finding {
                kind: "binary-missing",
                message: "`foo`: managed binary is missing".into(),
                krate: None,
                bin: None,
                path: None,
                hint: None,
            }],
            warnings: Vec::new(),
        }));
        let report = app.build_report.as_ref().expect("findings pin a panel");
        assert!(report.failed, "an invariant violation is a failed panel");
        assert_eq!(app.report_scroll, 0, "a fresh report starts at the top");
        let _ = std::fs::remove_dir_all(&prefix);
    }

    /// A warning is shown on receipt, not banked until `Done`: with the
    /// build still running, the panel already carries it. That is the
    /// surfacing contract in full — the UI never delays the worker, so
    /// a build fast enough to finish within one poll is a race the
    /// warning loses without being wrong, and this test pins the
    /// behaviour where the mechanism actually answers: a warning
    /// received while the job is alive is readable at once.
    #[test]
    fn a_warning_reaches_the_panel_while_the_build_still_runs() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-live-warning");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();

        let (tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });

        tx.send(BuildMsg::Warning(
            "warning: `foo` is already managed under /usr/local @1.2.3".into(),
        ))
        .unwrap();
        app.poll_job().unwrap();
        assert!(app.build_running(), "the warning does not end the build");
        let report = app
            .build_report
            .as_ref()
            .expect("a warning is readable while the build runs");
        assert!(report.lines.iter().any(|l| l.contains("already managed")));
        assert!(!report.failed, "a warning is not a failure");

        // A second warning grows the same panel; the scroll is not
        // yanked back to the top under a reader.
        app.report_scroll = 1;
        tx.send(BuildMsg::Warning("warning: `foo` is shadowed".into()))
            .unwrap();
        app.poll_job().unwrap();
        let report = app.build_report.as_ref().expect("still readable");
        assert_eq!(report.lines.len(), 2, "both warnings, in order");
        assert_eq!(
            app.report_scroll, 1,
            "a growing panel keeps the reader's place"
        );

        // And it is a warning, not a gate: the build finishes on its own
        // and the final panel is the same record.
        tx.send(BuildMsg::Done(BuildOutcome::Success)).unwrap();
        app.poll_job().unwrap();
        assert!(app.job.is_none(), "the build ran to completion, ungated");
        let report = app.build_report.as_ref().expect("the record survives");
        assert_eq!(report.lines.len(), 2, "{report:?}", report = report.lines);
        let _ = std::fs::remove_dir_all(&prefix);
    }

    /// A cancelled build's live warning panel does not outlive the job
    /// it belonged to: the cancel gets its message, and nothing stale
    /// stays pinned over the list.
    #[test]
    fn a_cancelled_build_leaves_no_stale_warning_panel() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-live-warning-cancel");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();

        let (tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });
        tx.send(BuildMsg::Warning(
            "warning: `foo` is already managed".into(),
        ))
        .unwrap();
        app.poll_job().unwrap();
        assert!(app.build_report.is_some());
        tx.send(BuildMsg::Done(BuildOutcome::Cancelled)).unwrap();
        app.poll_job().unwrap();
        assert!(
            app.build_report.is_none(),
            "a cancelled build pins no panel, live or final"
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn captured_kinds_survive_to_the_person() {
        // The exact regression: a warning once captured, tailed and dropped
        // by a successful finish; a notice captured and never shown.
        let prefix = std::env::temp_dir().join("cargo-lbin-test-tui-kinds");
        let _ = std::fs::remove_dir_all(&prefix);
        std::fs::create_dir_all(&prefix).unwrap();
        let mut app = App::new(&prefix).unwrap();

        let (tx, rx) = mpsc::channel();
        let (auth_tx, _auth_rx) = mpsc::channel();
        app.job = Some(Job::Build {
            name: "foo".into(),
            rx,
            auth_tx,
            units_started: 0,
            current: None,
            tail: VecDeque::new(),
            status_note: None,
            warnings: Vec::new(),
            started: std::time::Instant::now(),
            needs_auth: None,
            control: std::sync::Arc::new(crate::BuildControl::new()),
            kind: BuildKind::Install,
            cancel_deadline: None,
        });

        // A notice becomes the live status, wins over the unit counter…
        tx.send(BuildMsg::Notice("waiting for the state lock…".into()))
            .unwrap();
        app.poll_job().unwrap();
        let gauge = app.build_progress().expect("job is running");
        assert!(
            gauge.contains("waiting for the state lock"),
            "a notice is the live truth of the moment: {gauge}"
        );
        // …and cargo speaking again supersedes it.
        tx.send(BuildMsg::Cargo("   Compiling serde v1.0.0".into()))
            .unwrap();
        app.poll_job().unwrap();
        let gauge = app.build_progress().expect("job is running");
        assert!(
            gauge.contains("1 unit") && !gauge.contains("1 units") && !gauge.contains("waiting"),
            "cargo's stream supersedes a stale notice: {gauge}"
        );

        // A warning survives a successful finish. (Raw on purpose: this
        // injects past the boundary, which is exercised below.)
        tx.send(BuildMsg::Warning(
            "`foo` is shadowed by /usr/bin/foo".into(),
        ))
        .unwrap();
        tx.send(BuildMsg::Done(BuildOutcome::Success)).unwrap();
        app.poll_job().unwrap();
        assert!(app.job.is_none(), "the job is finished");
        let report = app
            .build_report
            .as_ref()
            .expect("warnings pin a report past a success");
        assert!(!report.failed);
        assert!(
            report.lines.iter().any(|l| l.contains("shadowed")),
            "the captured warning reaches the person: {:?}",
            report.lines
        );
        let _ = std::fs::remove_dir_all(&prefix);
    }

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
                BuildMsg::NeedAuth(_) | BuildMsg::Done(_) => {
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

    #[test]
    fn rows_carry_the_cross_prefix_suffix() {
        // Pre-formatted by the shared formatter: pins plumbing and no-drift.
        let mut also = std::collections::BTreeMap::new();
        also.insert(
            "one".to_owned(),
            vec![crate::prefixes::AlsoIn {
                prefix: std::path::PathBuf::from("/usr/local"),
                version: "0.9.0".to_owned(),
            }],
        );
        let m = manifest(&[("one", "1.0.0"), ("two", "2.0.0")]);
        let rows = rows_from(&m, None, &also);
        let one = rows.iter().find(|r| r.name == "one").unwrap();
        // Facts on the row; the shared formatter is applied at render
        // time, so the CLI listing and the list suffix still cannot
        // drift.
        assert_eq!(one.also.len(), 1);
        assert_eq!(one.also[0].prefix, std::path::PathBuf::from("/usr/local"));
        assert_eq!(one.also[0].version, "0.9.0");
        assert_eq!(
            crate::prefixes::describe(&one.also),
            " [also in /usr/local @0.9.0]"
        );
        let two = rows.iter().find(|r| r.name == "two").unwrap();
        assert!(two.also.is_empty(), "installed nowhere else, no facts");
    }

    #[test]
    fn elapsed_formats_like_a_clock() {
        use std::time::Duration;
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(97)), "01:37");
        assert_eq!(format_elapsed(Duration::from_secs(59 * 60 + 59)), "59:59");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 65)), "1:01:05");
    }
}
