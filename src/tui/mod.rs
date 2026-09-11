//! Interactive front end over the same commands the CLI runs.
//!
//! The TUI adds no logic of its own. It reads the manifest and the last
//! `checkupdate` report from disk, and every action is one of the existing
//! commands: `r` is `checkupdate`, `u`/`U` are `update NAME`/`update --all`,
//! `i` is `install`, `x` is `remove`, `s` is `search`. Nothing happens
//! unless a key asks for it — no polling, no refresh or network access on
//! start. (Once asked, `i`, `u` and `U` reach the network too, through
//! cargo; the guarantee is about what the TUI does unprompted.)
//!
//! Two ways of running a command. Commands that build and place binaries
//! (`update`, `install`, `remove`) need the real terminal: cargo prints its
//! own progress, rustc its own diagnostics, and sudo may prompt for a
//! password. The TUI steps aside for them — leaves the alternate screen,
//! runs the command exactly as the CLI would, waits for Enter, and comes
//! back (the lazygit-spawns-an-editor pattern). The `Terminal` is created
//! once and kept across handoffs: `ratatui::try_init()` installs a panic hook
//! on every call, wrapping the previous one, so re-initializing per
//! command would stack a hook per operation. `checkupdate` and `search`
//! only talk to crates.io; they run on a one-shot thread while the list
//! stays navigable, and their answers are applied on the main thread.

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

/// How long an accepted cancel is given before the run loop escalates
/// to SIGKILL on its own. Two seconds: enough for any well-behaved
/// build tree to fold after SIGTERM, short enough that a wedged one
/// does not hold the person hostage.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// One installed crate as the list shows it.
#[derive(Clone)]
pub struct Row {
    pub name: String,
    pub version: String,
    pub bins: Vec<String>,
    pub locked: bool,
    pub pinned: bool,
    /// Pre-formatted ` [also in ...]` suffix from the prefixes module —
    /// the same formatter the CLI listing uses, so the two surfaces
    /// cannot drift; empty for a crate installed nowhere else.
    pub also: String,
    pub status: RowStatus,
}

/// What the last `checkupdate` says about a row — three states, and the
/// third is silence, never a guess (see `report::Status`).
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
/// will act on: a pinned crate is held back by it, so a pinned crate
/// with a newer version does not belong in a view whose count promises
/// actionable updates. Its backlog is not hidden — it lives in the
/// Pinned tab, whose count is always on screen.
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
    /// The operation succeeded but something around it did not — shown
    /// in yellow so it is neither dismissed as routine nor read as a
    /// failure.
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
    /// The one funnel: the prompt is Span-bound like every other line,
    /// and both builders interpolate strings the TUI does not control —
    /// binary names from the manifest, prefixes inherited from $HOME.
    fn new(prompt: &str, action: OnConfirm) -> Self {
        Self {
            prompt: crate::text::sanitize(prompt),
            action,
        }
    }
}

/// What a confirmed `y` triggers. A migration starts an in-place job
/// like an install does; a removal decides its shape only at the `y` —
/// in place, or the terminal handoff — because privilege is a property
/// of the world, checked fresh (see `remove_confirmed`).
enum OnConfirm {
    Migrate {
        name: String,
        /// The row's version string, for the result line — display data
        /// for the same plan the snapshot freezes.
        version: String,
        dest: PathBuf,
        /// The plan, frozen at the keypress from the very row the person
        /// is looking at. The worker receives *this* snapshot — never a
        /// fresh one taken after the `y`, which could bless a version
        /// the person never confirmed; a source that moved on since is
        /// the checkpoint's job to reject.
        snap: crate::MigrationSnapshot,
    },
    /// `x`: the shape (in place or terminal handoff) is decided fresh
    /// at the `y`, not at the keypress — privilege is a property of the
    /// world, and the world may move while the prompt is open.
    Remove { name: String },
    /// `migrate --all`: the whole plan frozen at the keypress, one
    /// snapshot per row, complete or not at all.
    MigrateAll {
        dest: PathBuf,
        plan: Vec<PendingMigrate>,
    },
}

/// Commands that take over the terminal; queued by key handlers and run
/// by the event loop after the frame announcing them has been drawn.
#[derive(Clone)]
enum PendingAction {
    Update(String),
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
    /// `downgrade`: the version prompt appears in the terminal, like
    /// `update`'s confirmation.
    Downgrade(String),
}

/// The render boundary in one function: every pipeline line becomes a
/// `BuildMsg` here — sanitized, because past this point it is Span-bound,
/// and a shadow warning or a lock notice carries paths, and a path may
/// hold ESC as legally as `a`. The worker forwards through this and
/// nothing else; a future producer cannot route around it.
fn build_msg(kind: crate::LineKind, line: &str) -> BuildMsg {
    let line = crate::text::sanitize(line);
    match kind {
        crate::LineKind::Cargo => BuildMsg::Cargo(line),
        crate::LineKind::Notice => BuildMsg::Notice(line),
        crate::LineKind::Warning => BuildMsg::Warning(line),
    }
}

/// Messages a build worker streams to the UI thread — the protocol
/// mirrors the pipeline's classification, because capture is not
/// presentation and the UI decides differently per kind.
enum BuildMsg {
    /// cargo's own output: parsed for the gauge, kept for the tail.
    Cargo(String),
    /// The pipeline narrating itself; promoted to the live status so
    /// "waiting for the state lock" is what the person sees, not a
    /// gauge frozen at zero units.
    Notice(String),
    /// Kept past success: a shadowed binary does not stop being
    /// shadowed because the install succeeded.
    Warning(String),
    /// A privileged step wants sudo revalidated on a real terminal —
    /// for the named prefix: one worker can escalate for two prefixes
    /// in one job (a migration's destination placement, then its source
    /// retirement), and the prompt must name the one actually asking.
    /// The worker blocks on the auth channel until the run loop — the
    /// only place that owns the terminal — answers.
    NeedAuth(PathBuf),
    /// The pipeline finished, one way or the other — classified by the
    /// worker, where the error itself is at hand: the UI must not guess
    /// "cancelled" from a phase flag that a late `c` can set an instant
    /// after cargo died of its own causes.
    Done(BuildOutcome),
}

/// How a build ended. `Cancelled` is a real outcome of the pipeline
/// (the `BuildCancelled` marker travelling up as an error), not a UI
/// interpretation: a cancelled build writes no failure log and leaves
/// no stage behind. `CompletedWithWarning` is a job whose build
/// succeeded but whose follow-through did not finish as asked — the
/// name is deliberately generic: the payload owns the specifics, and
/// the protocol does not learn any one operation's vocabulary.
enum BuildOutcome {
    Success,
    Cancelled,
    CompletedWithWarning(String),
    Failed(anyhow::Error),
}

/// What the job is building toward. The UI composes its result lines
/// from this data — the worker reports outcomes, never prose.
enum BuildKind {
    Install,
    /// Boxed: `Job::Build` is already the enum's largest variant, and
    /// the target rides in every build job regardless of kind — an
    /// inline payload here is dead weight on every install.
    Migrate(Box<MigrateTarget>),
}

/// A confirmed migration on its way to `start_migrate`, carried through
/// the run loop because the destination preflight may need the terminal.
struct PendingMigrate {
    name: String,
    version: String,
    dest: PathBuf,
    snap: crate::MigrationSnapshot,
}

/// A confirmed `M`: the CLI's `migrate --all`, as a queue of the very
/// same single migrations `m` runs — each crate its own unit of work,
/// its own preflight (a sudo timestamp can expire mid-batch), its own
/// pass through the Build panel and the cancel door. The batch owns the
/// tally and the final summary; per-crate panels are suppressed in
/// favor of one report at the end, which is the CLI's "reported, and
/// the batch moves on" in the TUI's shape.
struct MigrateBatch {
    dest: PathBuf,
    queue: std::collections::VecDeque<PendingMigrate>,
    total: usize,
    moved: usize,
    /// `(name, lines)` — destination committed, source not retired: the
    /// full Incomplete reason plus any build warnings of that member.
    warned: Vec<(String, Vec<String>)>,
    /// `(name, lines)` — the same diagnostics a single job's failure
    /// panel gets (error chain, tail excerpt when the chain is terse),
    /// the authoritative already-installed refusal among them exactly
    /// as the CLI counts it: a shortfall, not a skip.
    failed: Vec<(String, Vec<String>)>,
    /// `(name, warnings)` — members that migrated *fully* but whose
    /// build spoke warnings: a batch summary must not launder a shadow
    /// warning into a clean "migrated N of N".
    noticed: Vec<(String, Vec<String>)>,
    /// The last mid-batch reload failure, resurfaced at the summary —
    /// recorded, not discarded.
    reload_error: Option<String>,
}

/// Did `start_migrate` actually start a worker? A refusal carries its
/// reason: the caller — a batch especially — must put it somewhere the
/// final summary will not overwrite.
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
    /// caller decides where it must survive (a footer line for an
    /// install; the batch's summary panel for a migration, whose later
    /// summary would overwrite the footer).
    Reported(String),
}

/// Where a migration is headed; the UI composes its result lines from
/// this.
struct MigrateTarget {
    version: String,
    dest: PathBuf,
}

/// A build's sticky report, held in the details panel until dismissed.
/// A failure carries the error tail and the log path (see
/// `stage::build_captured`); a success carries warnings — a shadowed
/// binary does not stop being shadowed because the install succeeded —
/// and either would be wasted by a message that scrolls away with the
/// next keypress.
pub struct BuildReport {
    pub title: String,
    pub lines: Vec<String>,
    pub failed: bool,
}

/// How many output lines the gauge keeps around for the failure panel.
/// The full text is in the log file; this is only what a placement or
/// commit error — which carries no tail of its own — gets to show.
const BUILD_TAIL: usize = 40;

/// Background work in flight. At most one at a time: the footer shows one
/// busy label and the user should know what it stands for.
enum Job {
    Check(Receiver<Result<Vec<Checked>>>),
    Search {
        query: String,
        rx: Receiver<Result<Vec<api::Hit>>>,
    },
    /// A captured single-crate install: `tui_install_one` on a worker
    /// thread, its lines streaming in over `rx`.
    Build {
        name: String,
        rx: Receiver<BuildMsg>,
        /// The run loop's answer to `BuildMsg::NeedAuth`.
        auth_tx: Sender<bool>,
        /// Units started, per the progress parser. Started, not
        /// finished: cargo announces a unit when it begins.
        units_started: usize,
        /// The unit last announced by cargo.
        current: Option<String>,
        /// Rolling tail for the failure panel.
        tail: VecDeque<String>,
        /// The last pipeline notice, shown as the live status until
        /// cargo speaks again.
        status_note: Option<String>,
        /// Warnings; shown past a success, dropped on failure — a
        /// rollback removes the binaries they described.
        warnings: Vec<String>,
        /// When the worker was spawned. Lock waiting counts on purpose:
        /// the clock answers "how long has this operation been running",
        /// not "how long has cargo been compiling".
        started: std::time::Instant,
        /// Set by `poll_job` when the worker asked for revalidation —
        /// carrying the prefix the escalation is for; answered by the
        /// run loop, which owns the terminal.
        needs_auth: Option<PathBuf>,
        /// The cancel state machine shared with the worker: `c` and
        /// Ctrl-C talk to the build through this and nothing else.
        control: std::sync::Arc<crate::BuildControl>,
        /// What is being built toward; the result lines are composed
        /// from this.
        kind: BuildKind,
        /// When the grace period of an accepted cancel runs out; armed
        /// by the first Accepted, one-shot. The run loop escalates to
        /// SIGKILL when it passes, because the worker cannot be trusted
        /// to reach its own sweep — a group member holding the inherited
        /// stderr can wedge it *inside* `read_until`, past the poll: a
        /// partial line with no terminating newline is enough to turn
        /// "readable" into a blocking read on a pipe nobody will close.
        cancel_deadline: Option<std::time::Instant>,
    },
}

impl Job {
    fn label(&self) -> String {
        match self {
            Job::Check(_) => "checking crates.io for updates…".to_owned(),
            Job::Search { query, .. } => format!("searching crates.io for `{query}`…"),
            Job::Build { name, .. } => format!("building {name}…"),
        }
    }
}

/// How many hits the details panel can show; passed to `api::search`,
/// which guarantees no more come back.
const SEARCH_HITS: usize = 6;

/// A finished search, shown in the details panel until dismissed. Digit
/// keys pick a hit by its 1-based position and fill the install input.
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<api::Hit>,
    /// Installed version per hit name, read from the manifest when the
    /// result was presented (see `finish_search`).
    pub installed: BTreeMap<String, String>,
}

pub struct App {
    prefix: PathBuf,
    cache: PathBuf,
    /// Ctrl-C during a build: quit, but only once the worker has
    /// reported back — leaving earlier would orphan a cargo that is
    /// still being torn down.
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
    /// A confirmed migration waiting for the run loop, which owns the
    /// terminal the preflight may need for a password.
    pending_migrate: Option<PendingMigrate>,
    /// A running `M` batch; `finish_build` feeds it and advances the
    /// queue, `c` on the current crate ends it.
    migrate_batch: Option<MigrateBatch>,
    pub search_result: Option<SearchResult>,
    /// A build's sticky report pinned to the details panel until dismissed.
    pub build_report: Option<BuildReport>,
    pub show_help: bool,
    pending: Option<PendingAction>,
    /// A captured install waiting for the run loop, which owns the
    /// terminal and must preauthorize sudo before spawning the worker.
    pending_build: Option<(String, bool)>,
    job: Option<Job>,
    /// Frame counter; drives the gauge spinner.
    ticks: usize,
    should_quit: bool,
}

/// Entry point for `cargo lbin tui`.
///
/// Owns the terminal for the whole session. Teardown mirrors the handoff
/// in `run_in_terminal` — show the cursor, then leave raw mode and the
/// alternate screen — and is attempted whether or not the loop returned
/// an error, so a failure inside the TUI does not also leave the shell
/// with a hidden cursor. The loop's result is reported first: it is the
/// one the user asked about.
pub fn run(prefix: &Path) -> Result<()> {
    // Everything that can fail before raw mode does so here, as a plain
    // error message rather than a garbled screen.
    let mut app = App::new(prefix)?;
    // `try_init` over `init`: a terminal that refuses raw mode or the
    // alternate screen is a normal error for cargo-lbin to report, not a
    // panic. It initializes in stages (hook, raw mode, alternate screen,
    // terminal), so on failure a best-effort restore undoes whichever
    // stages did succeed before the error is passed on.
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
            build_report: None,
            pending_build: None,
            ticks: 0,
            show_help: false,
            pending: None,
            job: None,
            should_quit: false,
            quit_after_build: false,
        };
        app.reload()?;
        Ok(app)
    }

    /// Re-read manifest and report from disk and rebuild the rows. Called
    /// at start, after every terminal-taking command, before an update
    /// check, and when a search result is about to be presented — the
    /// TUI may have been open for an hour while another cargo-lbin
    /// changed the prefix, and each of those is a moment the user is
    /// about to be shown or act on the prefix's state. The state lock is
    /// held only for the manifest read, never while the TUI idles.
    fn reload(&mut self) -> Result<()> {
        // A search result marks hits as installed — a fact about the
        // prefix. Anything that re-reads the prefix — a command's return,
        // `r`, a newer search — is exactly the moment that fact may have
        // stopped being true, so it goes. `finish_search` reloads first
        // and sets the new result after, so this never eats a fresh one.
        self.search_result = None;
        let report = match Report::load(&self.cache, &self.prefix) {
            Ok(report) => report,
            Err(e) => {
                self.warn(&format!("update report unreadable: {e:#}"));
                None
            }
        };
        self.apply_report(report.as_ref())
    }

    /// Rebuild the rows from a fresh manifest read and the given report —
    /// which may be one that could not be written to disk; what the
    /// index answered is still shown.
    fn apply_report(&mut self, report: Option<&Report>) -> Result<()> {
        let manifest = {
            let _lock = StateLock::acquire(&self.prefix, &Mode::Shared)?;
            Manifest::load(&self.prefix)?
        };
        // Once per reload, never per frame — and lockless by design; the
        // prefixes module explains why an annotation must never wait on
        // a foreign lock.
        let also = crate::prefixes::also_installed(&self.prefix);
        self.rows = rows_from(&manifest, report, &also);
        self.report_age = report.map(Report::age);
        self.clamp_selection();
        Ok(())
    }

    /// Rows under the current filter, paired with their index in `rows`.
    pub fn visible(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| admits(self.filter, r))
            .collect()
    }

    /// Is a build job (install or migrate) running right now? The
    /// footer asks: while one runs, `c` is the key that matters and
    /// most of the list keys bounce off "another operation is already
    /// running" — the hint bar swaps to the build's controls, the same
    /// way it already does for a confirmation or an input. Controls,
    /// not promises: the job outlives the placement door, where `c` is
    /// `TooLate` and Ctrl-C only arms quit-after, so the bar names the
    /// keys and leaves each press's truthful outcome to the runtime
    /// message.
    pub fn build_running(&self) -> bool {
        matches!(self.job, Some(Job::Build { .. }))
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.visible().get(self.selected).copied()
    }

    pub fn updates_available(&self) -> usize {
        // Keep the cached Updates view aligned with update --all
        // semantics: pinned crates are held back and counted separately.
        // Alignment of meaning, not of outcome — this number comes from
        // the recorded report, while `U` computes a fresh plan.
        self.rows
            .iter()
            .filter(|r| matches!(r.status, RowStatus::Outdated(_)) && !r.pinned)
            .count()
    }

    pub fn pinned_count(&self) -> usize {
        self.rows.iter().filter(|r| r.pinned).count()
    }

    /// Pinned crates the last check found a newer version for — the
    /// backlog the pin is deliberately sitting on.
    pub fn pinned_outdated(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.pinned && matches!(r.status, RowStatus::Outdated(_)))
            .count()
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    /// Rows the last report says nothing about. Counted separately so the
    /// footer never lets "0 updates" imply "all current".
    pub fn not_checked(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::Unknown)
            .count()
    }

    pub fn busy(&self) -> Option<String> {
        self.job.as_ref().map(Job::label)
    }

    /// The live gauge line for a running build, or `None`. The spinner
    /// keeps the line visibly alive between units — one large crate can
    /// compile for minutes without a new `Compiling` line.
    pub fn build_progress(&self) -> Option<String> {
        const FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
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
        // A pipeline notice is the live truth of the moment — "waiting
        // for the state lock…" beats a gauge frozen at zero units, which
        // is exactly the impression the notice exists to prevent. The
        // clock runs through it: waiting is part of the operation.
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
            if let Some((spec, locked)) = self.pending_build.take() {
                self.start_build(terminal, &spec, locked)?;
                continue;
            }
            if let Some(req) = self.pending_migrate.take() {
                let member = req.name.clone();
                if let StartOutcome::Refused(reason) = self.start_migrate(terminal, req)? {
                    match self.migrate_batch.as_mut() {
                        // A batch cannot wait on a worker that never
                        // existed: the refused member is recorded as a
                        // failure — with its reason, in the panel, where
                        // the summary will not overwrite it — and the
                        // remainder is counted as not attempted.
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

    /// Steps out of the TUI, runs the command as the CLI would — its
    /// output, its prompts, its sudo — and steps back in. The wait for
    /// Enter is what lets the user read the output; the redraw would
    /// otherwise erase it at once.
    ///
    /// The cursor is shown explicitly: `draw` hides it whenever a frame
    /// sets no cursor position, and `restore` does not bring it back, so
    /// without this sudo would prompt for a password at an invisible
    /// cursor. `try_restore` rather than `restore`: if raw mode could not
    /// be left, handing the terminal to cargo and sudo anyway would be
    /// worse than aborting with the reason. Re-entry re-enables raw mode
    /// and the alternate screen on the same `Terminal` — see the module
    /// doc for why not a fresh `init()`.
    fn run_in_terminal(
        &mut self,
        terminal: &mut DefaultTerminal,
        action: &PendingAction,
    ) -> Result<()> {
        terminal.show_cursor()?;
        ratatui::try_restore().context("leaving the TUI")?;
        println!();
        let outcome = match action {
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
            PendingAction::Downgrade(name) => crate::cmd_downgrade(&self.prefix, name),
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

        // The command may have changed everything; the report is stale for
        // whatever it touched, and `reload` shows that as "not checked".
        self.reload()?;
        match outcome {
            Ok(()) => self.info(&format!("{} finished", action_label(action))),
            Err(e) => self.error(&format!("{} failed: {e:#}", action_label(action))),
        }
        Ok(())
    }

    /// What the escalation preflight found for one prefix; both captured
    /// workflows (install, migrate) run it before spawning a worker, and
    /// each maps the outcomes to what it can offer.
    fn preflight_escalation(terminal: &mut DefaultTerminal, prefix: &Path) -> Result<Preflight> {
        let policy = crate::privileged::Policy::for_prefix(prefix);
        // The pipeline's own union (bin + state) plus the lock file —
        // the worker's first privileged touch. Mixed ownership needs the
        // state term up front; lock preparation is consulted only where
        // escalation is possible at all — and it must be consulted: for
        // a migration into /usr/local the destination's state lock may
        // be the first one ever prepared there, ahead of any NeedAuth
        // machinery, and a cold sudo would fail `sudo -n` instead of
        // asking.
        let escalate = match crate::operation_needs_privilege(policy, prefix) {
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
        // Right after a successful validation the timestamp should be
        // warm; a sudo that does not cache is detected now, not by the
        // worker's first `sudo -n`.
        match crate::privileged::credentials_fresh() {
            Ok(true) => Ok(Preflight::Ready),
            Ok(false) => Ok(Preflight::NoCache),
            Err(e) => Ok(Preflight::Reported(format!("{e:#}"))),
        }
    }

    /// A captured single-crate install. The run loop calls this because
    /// only it owns the terminal: when placement will need sudo and the
    /// credential timestamp is stale, the initial prompt happens here, up
    /// front, on a real terminal — never inside the alternate screen.
    fn start_build(
        &mut self,
        terminal: &mut DefaultTerminal,
        raw_spec: &str,
        locked: bool,
    ) -> Result<()> {
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
        // A new attempt supersedes the previous report — a stale
        // "install foo failed" over a fresh run of foo would report on
        // the wrong world. Cleared here, once the attempt is definitely
        // starting, so the terminal-fallback path supersedes it too.
        self.build_report = None;
        match Self::preflight_escalation(terminal, &self.prefix.clone())? {
            Preflight::Ready => {}
            // Captured placement runs `sudo -n`, so a sudo that does not
            // cache credentials (timestamp_timeout=0, per-TTY quirks)
            // would be asked a question it cannot voice. Install has an
            // old way to fall back to: hand the terminal over instead of
            // starting a build that must end in an error.
            Preflight::NoCache => {
                self.info("sudo does not cache credentials here; handing the terminal over");
                self.pending = Some(PendingAction::Install {
                    crates: vec![raw_spec.to_owned()],
                    locked,
                });
                return Ok(());
            }
            Preflight::Reported(message) => {
                self.error(&message);
                return Ok(());
            }
        }
        let (tx, rx) = mpsc::channel();
        let (auth_tx, auth_rx) = mpsc::channel();
        let control = std::sync::Arc::new(crate::BuildControl::new());
        let worker_control = std::sync::Arc::clone(&control);
        let prefix = self.prefix.clone();
        let name = spec.name.clone();
        let worker_tx = tx.clone();
        std::thread::spawn(move || {
            let line_tx = worker_tx.clone();
            let control = worker_control;
            let result = crate::tui_install_one(
                &prefix,
                &spec,
                locked,
                &mut |k: crate::LineKind, l: &str| {
                    let _ = line_tx.send(build_msg(k, l));
                },
                &mut |escalating: &Path| {
                    // A cancelled build must not ask anyone for a
                    // password: refuse here instead of raising NeedAuth
                    // for an install that will never place. The run
                    // loop guards the other side of the same race.
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
                                // A denial that answers a cancel *is*
                                // the cancel; a real refusal keeps its
                                // own name.
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
            kind: BuildKind::Install,
            cancel_deadline: None,
        });
        Ok(())
    }

    /// Start an in-place migration of `name` to `dest`: the same
    /// `Job::Build`, gauge, cancel door, dead-man switch and sudo
    /// roundtrip as an install — the TUI is a frontend to `migrate`,
    /// not a second migrate. The worker reports an outcome; the words
    /// are composed in `finish_build` from the job's own data.
    /// Whether a worker actually started, and if not, why — the reason
    /// travels back because a running batch must both end (its queue
    /// cannot wait on a worker that never existed) and *record* the
    /// refusal where the summary will not overwrite it.
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
        // The same invariant as an install: a new in-place attempt
        // supersedes whatever report the last one left up — a stale
        // "install foo failed" must not sit over a fresh migration of
        // bar, and a cancelled outcome returns early without reaching
        // any later cleanup.
        self.build_report = None;
        // The destination's preflight, before the worker exists: its
        // state lock may be the first ever prepared under /usr/local,
        // ahead of any NeedAuth roundtrip — a cold sudo must be asked
        // here, on the suspended terminal, not fail `sudo -n` in the
        // dark. The source deliberately keeps its late, in-flight
        // revalidation instead: the prompt-at-the-end timing is
        // documented, and warming its timestamp before a long build
        // would buy nothing.
        match Self::preflight_escalation(terminal, &dest)? {
            Preflight::Ready => {}
            // No terminal handoff exists for migrate, on purpose; the
            // CLI is the interactive shape.
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
                    // The same auth roundtrip as an install; a cancelled
                    // job must not ask anyone for a password, and a
                    // denial that answers a cancel is the cancel.
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
            // Classified once, by type and by data — the UI never
            // guesses.
            let outcome = match result {
                Ok(crate::MigrateOutcome::Moved { .. }) => BuildOutcome::Success,
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

    /// The worker hit the placement checkpoint with a stale credential
    /// timestamp — the build outlived it. Revalidate on the real
    /// terminal and let the worker proceed (or fail, and say so).
    /// Arm the one-shot grace deadline of an accepted cancel; a repeat
    /// keeps the original deadline (the person pressing `c` twice fast
    /// escalates through `request_cancel` itself, not through here).
    fn arm_cancel_grace(&mut self) {
        if let Some(Job::Build {
            cancel_deadline, ..
        }) = &mut self.job
        {
            cancel_deadline.get_or_insert(std::time::Instant::now() + CANCEL_GRACE);
        }
    }

    /// The cancel's dead-man switch, run every tick. Once the grace of
    /// an accepted cancel expires, SIGKILL goes out from this thread —
    /// the worker cannot be trusted to reach its own sweep (see
    /// `cancel_deadline`), and signalling from the UI is precisely what
    /// stays possible no matter where the worker is stuck. One-shot;
    /// Killed is worth a line, "already stopping" is quiet success.
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
        // A cancel that landed after the worker raised NeedAuth: answer
        // no without suspending the screen — nobody types a password
        // for a build that will never place. The worker checks before
        // asking; this guards the other side of the same race.
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
        // The prefix comes from the request, never from the app: a
        // migration escalates for its *destination* mid-job while
        // `self.prefix` is the source, and a prompt naming the wrong
        // side would lie in exactly one direction of the pair.
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
                // Validated and immediately stale again: this sudo does
                // not cache, and the noninteractive placement ahead
                // cannot ask. Fail loudly rather than let `sudo -n`
                // discover it a moment later with a terser message.
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
            // `needs_auth` was taken with the target above; only the
            // answer remains.
            let _ = auth_tx.send(ok);
        }
        Ok(())
    }

    /// The other prefix of the known pair, when the current prefix is a
    /// member of it — the gate `m` and `M` share. Symmetric on purpose:
    /// `known_others` of the current prefix must name exactly one
    /// candidate *and* that candidate's own view must name us back — a
    /// custom prefix on a HOME-less system would otherwise pass the
    /// first half.
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

    /// Advisory precheck for `m`: is the crate already installed at the
    /// destination? Fresh, silent, nonblocking — the same shape as the
    /// pin precheck below, for the same reason: nobody should confirm a
    /// migration (or type sudo's password for its preflight) that the
    /// authoritative refusal will bounce a moment later. A busy lock or
    /// an unreadable manifest answers "not occupied": advisory means
    /// the flow proceeds and the backend stays the judge.
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

    /// Advisory state check before anyone is asked for a password:
    /// typing sudo's prompt only to hear "that crate is pinned" a
    /// hundred milliseconds later would be a bad joke. Advisory, silent,
    /// nonblocking: a busy lock yields "not now", never a frozen UI —
    /// busy or unreadable skips the courtesy check, and the
    /// authoritative pass runs in the worker, under the real lock.
    /// Returns true when the install must stop here (refused or errored,
    /// message already shown).
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

    /// Leaves the TUI, runs `f` on the real terminal, and re-enters.
    /// The outer Result is the terminal handover itself — if that fails,
    /// the TUI cannot continue; `f`'s own result is the inner value.
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

    /// One batch member finished; tally it, advance the queue or wrap
    /// up. A cancel ends the whole batch: cancelling one crate and
    /// silently continuing with the rest would be guessing intent.
    fn finish_batch_step(
        &mut self,
        name: &str,
        target: &MigrateTarget,
        outcome: BuildOutcome,
        tail: &VecDeque<String>,
        warnings: Vec<String>,
    ) {
        // Per-crate reload keeps the list truthful mid-batch; a failure
        // is recorded on the batch and resurfaces at the summary.
        let reload_error = self.reload().err().map(|e| format!("{e:#}"));
        let cancelled = matches!(outcome, BuildOutcome::Cancelled);
        let (done, total) = {
            let Some(batch) = self.migrate_batch.as_mut() else {
                return;
            };
            if let Some(e) = reload_error {
                batch.reload_error = Some(e);
            }
            // The same diagnostics contract as a single job, member by
            // member: a fully successful member's build warnings must
            // not be laundered by the tally, a terse failure still gets
            // the tail excerpt, and an Incomplete reason travels with
            // the warnings its build spoke.
            match outcome {
                BuildOutcome::Success => {
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
        self.info(&format!(
            "[{done}/{total}] {name} {} processed",
            target.version
        ));
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
    /// report panel — warnings with their full Incomplete reasons,
    /// failures with their errors, the already-installed refusal among
    /// them exactly as the CLI counts it.
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
        // Everything the tally counted, plus the queue, must add up to
        // the plan: a member the caller recorded as refused sits in
        // `failed`, so "not attempted" is exactly what is still queued.
        let mut summary = format!("migrated {moved} of {total} to {}", dest.display());
        if let Some(how) = ended_early {
            let unprocessed = queue.len();
            // write!, not push_str(&format!(..)): no second allocation,
            // and the sink is infallible.
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
        self.build_report = Some(BuildReport {
            title: format!("migrate --all: {moved} of {total} migrated"),
            lines,
            failed: !failed.is_empty(),
        });
        self.warn(&format!(
            "{summary} — details in the panel; Esc/Enter dismisses"
        ));
    }

    /// The diagnostics a failure shows, single job and batch member
    /// alike: the error chain, plus the build's last lines when the
    /// chain is terse — a placement or commit error carries no tail of
    /// its own, and the compiler's actual message often lives there.
    fn failure_lines(e: &anyhow::Error, tail: &VecDeque<String>) -> Vec<String> {
        let text = format!("{e:#}");
        let mut lines: Vec<String> = text.lines().map(crate::text::sanitize).collect();
        if lines.len() <= 1 {
            lines.extend(tail.iter().rev().take(8).rev().cloned());
        }
        lines
    }

    /// A finished captured install: reload, then speak in the pipeline's
    /// own words when it left any — the "installed …" note is more
    /// informative than a generic "finished".
    fn finish_build(
        &mut self,
        name: &str,
        kind: &BuildKind,
        outcome: BuildOutcome,
        tail: &VecDeque<String>,
        warnings: Vec<String>,
    ) {
        let verb = match kind {
            BuildKind::Install => "install",
            // Tuple variant, tuple pattern: `Migrate { .. }` would also
            // parse (a rest pattern in braces is legal on tuple
            // variants), but a pattern should not lie about the shape.
            BuildKind::Migrate(_) => "migrate",
        };
        // A batch owns its members' presentation: tallies instead of
        // per-crate panels, one summary at the end — the CLI's
        // "reported, and the batch moves on", in the TUI's shape.
        if self.migrate_batch.is_some()
            && let BuildKind::Migrate(target) = kind
        {
            self.finish_batch_step(name, target, outcome, tail, warnings);
            return;
        }
        // A cancelled build ended exactly as asked: no failure panel,
        // no log path — the pipeline wrote no log and removed the stage
        // — one line saying the person's own decision was carried out.
        // Nothing was placed and nothing committed (the placement door
        // refused), so there is nothing to reload. The classification
        // is the worker's, by type, not this thread's guess from a
        // phase flag: a cancel that lost every race arrives here as the
        // Success or Failed it truly was on disk.
        if matches!(outcome, BuildOutcome::Cancelled) {
            self.info(&format!("{verb} {name} cancelled"));
            return;
        }
        // The pipeline's error outranks a reload error: the tail and the
        // log path are the diagnosis, and a failed screen refresh must
        // not eat them. On success the roles flip — the reload *is* the
        // remaining work, so its failure is the headline.
        let reload = self.reload();
        match outcome {
            BuildOutcome::Cancelled => unreachable!("returned above"),
            BuildOutcome::Success => {
                // Composed from the job's data, not fished out of the
                // pipeline's prose: the worker reports outcomes, the UI
                // owns the words. The install note keeps its historical
                // shape (the pipeline's own summary line is the best
                // one-liner it has); the migrate note says what is true
                // in every success flavor — including a source someone
                // else already retired — without overclaiming.
                let note = match kind {
                    BuildKind::Install => tail
                        .iter()
                        .rev()
                        .find(|l| l.starts_with("installed "))
                        .cloned()
                        .unwrap_or_else(|| format!("install {name} finished")),
                    BuildKind::Migrate(target) => format!(
                        "migrated {name} {} to {}",
                        target.version,
                        target.dest.display()
                    ),
                };
                // A failed reload does not eat the outcome: the install
                // happened and a shadow warning stays true, so the
                // report is pinned first and the reload complains after.
                if warnings.is_empty() {
                    match reload {
                        Ok(()) => self.info(&note),
                        Err(e) => self.error(&format!("{note} — but reload failed: {e:#}")),
                    }
                } else {
                    // Captured is not shown: a warning that reached the
                    // channel but never a human would make the whole
                    // classification pointless. The panel keeps them
                    // until acknowledged; the message says why it is up.
                    let mut lines = warnings;
                    if let Err(e) = &reload {
                        lines.push(String::new());
                        lines.push(crate::text::sanitize(&format!(
                            "(and the list reload failed: {e:#})"
                        )));
                    }
                    self.build_report = Some(BuildReport {
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
                // The build half succeeded and the state on disk has
                // changed — the reload above already reflects it. The
                // reason is a payload, shown whole in the panel (which
                // wraps): it is multi-sentence by design — what stands
                // where, what not to re-run — and a truncated footer
                // line must not be its only copy. Not a failure panel:
                // nothing here is broken, something is unfinished.
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
                self.build_report = Some(BuildReport {
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
                // An anyhow chain carries paths too; same boundary rule
                // — shared with the batch, so both surfaces diagnose
                // identically.
                let mut lines = Self::failure_lines(&e, tail);
                // Deliberately no warnings here: they were spoken about
                // binaries the rollback has since removed — "foo is
                // shadowed" is not true of an install that did not
                // happen. On success they are the whole point; see above.
                if let Err(re) = reload {
                    lines.push(crate::text::sanitize(&format!(
                        "(and the list reload failed: {re:#})"
                    )));
                }
                self.build_report = Some(BuildReport {
                    // "install", not "build": the failure may be the
                    // placement or the manifest commit after a clean
                    // cargo run, and the title must not narrow it.
                    title: format!("install {name} failed"),
                    lines,
                    failed: true,
                });
                self.error(&format!(
                    "install {name} failed — details in the panel; Esc/Enter dismisses"
                ));
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Quit — but never orphan cargo. With a build running the
            // exit is a cancel first: SIGTERM to the group, leave when
            // the worker reports back (a second Ctrl-C escalates to
            // SIGKILL through the same state machine). Placement cannot
            // be cancelled, so there the exit simply waits it out —
            // placement is seconds, and killing `sudo install` between
            // two binaries is not an option.
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
        if self.build_report.is_some()
            && self.input.is_none()
            && self.confirm.is_none()
            && matches!(key.code, KeyCode::Esc | KeyCode::Enter)
        {
            self.build_report = None;
            self.message = None;
            return;
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
        match key.code {
            KeyCode::Char('q') => {
                if matches!(self.job, Some(Job::Build { .. })) {
                    self.error("a build is running; c cancels it, Ctrl-C cancels and quits");
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Esc => {
                if self.search_result.take().is_some() {
                    // Dismissing a result is fine mid-build; only the
                    // exit is held back, symmetrically with `q`.
                } else if matches!(self.job, Some(Job::Build { .. })) {
                    self.error("a build is running; c cancels it, Ctrl-C cancels and quits");
                } else {
                    self.should_quit = true;
                }
            }
            // Cancel the running build and stay: first press SIGTERMs
            // cargo's group, a second SIGKILLs it. Without a build the
            // key means nothing — silence, not an error, because there
            // is nothing the person could have meant instead.
            KeyCode::Char('c') => {
                if let Some(Job::Build { name, control, .. }) = &self.job {
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
            }
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
            KeyCode::Char('s') => self.open_input(InputPurpose::Search),
            // With a search result up, a digit picks a hit and opens the
            // install line with that name — editable, so `--locked` can
            // still be added before Enter.
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
            // No gate at all — not on the cached report, not on the rows
            // in memory. `update --all` reads the manifest and asks the
            // index itself; a stale "0 updates" here must not stop a
            // command that would find two, and a manifest another
            // cargo-lbin changed since the last reload must not stop one
            // that would find a crate this list has never seen. If there
            // is nothing to do, the command says so.
            KeyCode::Char('U') => self.queue(PendingAction::UpdateAll),
            // Toggle from what the row shows; the command itself re-reads
            // the manifest under the lock, so a pin changed by another
            // process since the last reload is reported, not overwritten
            // blindly ("already pinned").
            KeyCode::Char('p') => {
                if let Some(row) = self.selected_row() {
                    self.queue(PendingAction::SetPinned {
                        name: row.name.clone(),
                        pinned: !row.pinned,
                    });
                }
            }
            // The choice of version is made in the terminal, by the
            // command itself — one prompt, the real list, no TUI copy.
            KeyCode::Char('D') => {
                if let Some(name) = self.selected_name() {
                    self.queue(PendingAction::Downgrade(name));
                }
            }
            KeyCode::Char('x') => self.remove_selected(),
            KeyCode::Char('B') => self.jump_to_other_prefix(),
            // Migrate the selected crate to the other prefix of the
            // known pair — and only there: with a custom --prefix "the
            // other side" stops being a function, and the TUI does not
            // grow a path picker for it; the CLI's explicit --to is the
            // tool. The gate is symmetric on purpose: `known_others` of
            // the current prefix must name exactly one candidate *and*
            // that candidate's own view must name us back — a custom
            // prefix with HOME unset would otherwise pass the first
            // half.
            // Both migrate keys live in their own methods: the dispatch
            // table stays a table.
            KeyCode::Char('m') => self.migrate_selected(),
            KeyCode::Char('M') => self.migrate_everything(),
            _ => {}
        }
    }

    /// `m`: migrate the selected crate to the other prefix of the known
    /// pair; the plan is frozen from the row on screen.
    fn migrate_selected(&mut self) {
        // Cloned out of the borrow: the fresh advisory precheck
        // below needs `&mut self` (it reports through the
        // footer), and the row is a reference into `self`.
        if let Some(row) = self.selected_row().cloned() {
            match self.known_pair_dest() {
                Some(dest) => {
                    // The agreed UX for "already on the other
                    // side" is a plain error line, not a sticky
                    // failure panel — decided on a *fresh*
                    // advisory read of the destination, never on
                    // the row's cached `[also in …]`: that
                    // annotation is a lockless snapshot of the
                    // last reload, so it can refuse a migration
                    // whose destination was emptied minutes ago
                    // (and stay wrong until the next reload).
                    // Advisory in the other direction too: a busy
                    // lock or a fresh install racing this read
                    // falls through to migrate_one's
                    // authoritative refusal, which then surfaces
                    // as Failed — the race window, accepted for
                    // now over a typed refusal variant.
                    if self.destination_occupied(&dest, &row.name) {
                        return;
                    }
                    // Frozen here, from the row on screen: the
                    // plan the person confirms is byte for byte
                    // the plan the worker revalidates.
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
                    let prompt = format!(
                        "migrate {} {}: {} -> {}? the exact version is rebuilt \
                         there, then retired here [y/N]",
                        row.name,
                        row.version,
                        self.prefix.display(),
                        dest.display()
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

    /// The whole prefix to the other side: `migrate --all` as a
    /// queue of the exact single migrations `m` runs. The plan is
    /// frozen here, one snapshot per row, complete or not at all
    /// — a plan that silently dropped an unparseable row would
    /// migrate a different set than the person confirmed.
    fn migrate_everything(&mut self) {
        let Some(dest) = self.known_pair_dest() else {
            self.info(
                "TUI migrate covers the /usr/local <-> ~/.local pair; \
                 migrate elsewhere via the CLI: cargo lbin migrate --all --to PREFIX",
            );
            return;
        };
        // `--all` means all crates *now*, not all as of the last
        // reload: a crate installed by another process since
        // would otherwise be silently absent from the plan, and
        // no checkpoint can reject a member the plan never had.
        // (Small `m` is the opposite case on purpose: there the
        // person confirms exactly the row they are looking at.)
        // Races *after* this moment are the per-crate
        // revalidation's job, as ever.
        if let Err(e) = self.reload() {
            self.error(&format!("cannot plan the batch: {e:#}"));
            return;
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
            "migrate all {} crate(s): {} -> {}? exact versions are rebuilt \
             there, then retired here; c cancels the batch [y/N]",
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

    /// The decision at the `y`: the same escalation test the build
    /// preflight consults (destination writability plus, where sudo is
    /// possible at all, lock preparation). Escalation means today's
    /// terminal handoff — a password prompt belongs on the real
    /// terminal, and sudo asks naturally there. No escalation means in
    /// place: removal is instant, and a prefix that never asks a
    /// password earns no screen flip. The lock is nonblocking by the
    /// same UI-thread argument as the in-place worker's checkpoints —
    /// a busy prefix is an answer, not a frozen interface.
    fn remove_confirmed(&mut self, name: String) {
        let policy = crate::privileged::Policy::for_prefix(&self.prefix);
        let escalate = match crate::operation_needs_privilege(policy, &self.prefix) {
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
                if let Err(e) = self.reload() {
                    self.error(&format!("removed {name}, but the reload failed: {e:#}"));
                    return;
                }
                self.info(&format!("removed {name} ({})", bins.join(", ")));
            }
            Ok(crate::TuiRemove::PrefixBusy) => {
                self.info("the prefix is busy (another cargo-lbin holds its lock); try again");
            }
            Err(e) => self.error(&format!("removing `{name}` failed: {e:#}")),
        }
    }

    /// `B`: jump to the other prefix of the known pair — m/M made the
    /// pair navigable for crates, B makes it navigable for the person.
    /// The same symmetric gate, the same one-line answer for anything
    /// else; no path picker grows here either.
    fn jump_to_other_prefix(&mut self) {
        match self.known_pair_dest() {
            Some(dest) => self.jump_to_prefix(dest),
            None => self.info(
                "TUI prefix switching covers the /usr/local <-> ~/.local pair; \
                 run with --prefix for anything else",
            ),
        }
    }

    /// The mechanics of `B`, separate from its gate so the policy and
    /// the machinery are each testable alone. Everything transient in
    /// App — a job above all, but also the queued starts — is anchored
    /// to `self.prefix`, so the jump refuses while any of it is alive:
    /// switching under a running check would apply the old prefix's
    /// results to the new prefix's screen, and every surface would lie.
    /// The switch commits only on a successful read of the other side —
    /// a title claiming one prefix over rows read from another would
    /// lie on every line — and the selection follows the *currently*
    /// selected crate by name when it is visible on the other side
    /// under the current filter; otherwise it falls back to the top. No
    /// stronger promise: a migration's retirement reloads the list and
    /// moves the selection before B is ever pressed, so "lands on the
    /// crate just migrated" would hold only sometimes, and a hint that
    /// holds only sometimes is a lie with good days.
    fn jump_to_prefix(&mut self, dest: PathBuf) {
        if self.job.is_some()
            || self.migrate_batch.is_some()
            || self.pending_migrate.is_some()
            || self.pending_build.is_some()
            || self.pending.is_some()
        {
            self.error("an operation is running or queued; finish or cancel it first");
            return;
        }
        let keep = self.selected_name();
        // reload() dismisses the search panel itself, so the panel's
        // transactionality is by hand: taken before the attempt,
        // restored after a rollback's own reload — a jump that did not
        // happen must not cost the person their hits. On success the
        // saved panel simply drops, together with the build report:
        // both were the old prefix's (the hits carry its [installed]
        // marks, the report describes its operations).
        let search = self.search_result.take();
        let back = std::mem::replace(&mut self.prefix, dest);
        if let Err(e) = self.reload() {
            let failed = std::mem::replace(&mut self.prefix, back);
            // Best-effort: this read succeeded moments ago; if the world
            // broke since, the error below still names the real problem.
            let _ = self.reload();
            self.search_result = search;
            self.error(&format!(
                "cannot read {}: {e:#} — staying here",
                failed.display()
            ));
            return;
        }
        self.build_report = None;
        self.selected = keep
            .and_then(|name| self.visible().iter().position(|row| row.name == name))
            .unwrap_or(0);
        self.info(&format!("now at {}", self.prefix.display()));
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
                // One crate builds in place, behind the gauge; a batch is
                // a longer conversation and keeps the terminal handoff.
                Ok((crates, locked)) if crates.len() == 1 => {
                    let spec = crates.into_iter().next().expect("len checked");
                    self.info(&format!("building {spec}…"));
                    self.pending_build = Some((spec, locked));
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

    /// `r`: the same query `checkupdate` runs, on a thread; the report is
    /// written on the main thread once the answer is in.
    fn start_check(&mut self) {
        if self.job.is_some() {
            self.error("busy; wait for the current lookup to finish");
            return;
        }
        if let Err(e) = self.reload() {
            self.error(&format!("reload failed: {e:#}"));
            return;
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
        thread::spawn(move || {
            let _ = tx.send(crate::check_versions(&entries));
        });
        self.job = Some(Job::Check(rx));
        self.message = None;
    }

    fn start_search(&mut self, query: String) {
        // The old result is not a placeholder for the new one: a failed
        // search must not leave the previous query's hits on screen under
        // a footer that talks about a different one.
        self.search_result = None;
        let (tx, rx) = mpsc::channel();
        let q = query.clone();
        thread::spawn(move || {
            let _ = tx.send(api::search(&q, SEARCH_HITS));
        });
        self.job = Some(Job::Search { query, rx });
        self.message = None;
    }

    /// Collects finished background work.
    ///
    /// A dropped sender means the worker panicked, and that ends the
    /// session rather than the job: `ratatui::try_init` installs a global
    /// panic hook that restores the terminal on *any* thread's panic, so
    /// by the time the main loop sees `Disconnected`, raw mode is off and
    /// the alternate screen has been left. Drawing another frame into
    /// that would scribble over the shell. The loop returns the error,
    /// the outer teardown runs once more (harmless), and the user gets
    /// a plain error line — the state ratatui already put them in.
    fn poll_job(&mut self) -> Result<()> {
        let Some(job) = self.job.take() else {
            return Ok(());
        };
        match job {
            Job::Check(rx) => match rx.try_recv() {
                Ok(result) => self.finish_check(result),
                Err(TryRecvError::Empty) => self.job = Some(Job::Check(rx)),
                Err(TryRecvError::Disconnected) => {
                    bail!("update check worker aborted; the terminal was reset by the panic")
                }
            },
            Job::Search { query, rx } => match rx.try_recv() {
                Ok(result) => self.finish_search(query, result),
                Err(TryRecvError::Empty) => self.job = Some(Job::Search { query, rx }),
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
                // Drain everything queued since the last frame: a fast
                // build emits many lines per tick, and rendering one line
                // per 100ms would show a gauge lagging minutes behind.
                let mut done: Option<BuildOutcome> = None;
                loop {
                    match rx.try_recv() {
                        Ok(BuildMsg::Cargo(line)) => {
                            match progress::parse_line(&line) {
                                progress::BuildEvent::Compiling { name, version } => {
                                    units_started += 1;
                                    current = Some(format!("{name} {version}"));
                                }
                                // Compilation is over; collision checks,
                                // placement and the manifest commit are
                                // not "compiling foo", and the gauge must
                                // not claim they are.
                                progress::BuildEvent::Finished => current = None,
                                _ => {}
                            }
                            // cargo speaking again supersedes a notice:
                            // "waiting for the state lock" is over once
                            // Compiling lines flow.
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
                        // Warnings live in `warnings` alone: the panel
                        // appends them itself, and a copy in the tail
                        // would print them twice under a one-line
                        // placement error.
                        Ok(BuildMsg::Warning(line)) => warnings.push(line),
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
                        // A Ctrl-C during this build asked to leave once
                        // the worker was collected; that is now.
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

    /// Same semantics as `checkupdate`: a check that reached the index is
    /// a success even if the report could not be written — the answer is
    /// shown from memory and the persistence failure is a warning, not a
    /// failed check.
    fn finish_check(&mut self, result: Result<Vec<Checked>>) {
        let report = match result.and_then(|checked| Report::new(&self.prefix, checked)) {
            Ok(report) => report,
            Err(e) => {
                self.error(&format!("update check failed: {e:#}"));
                return;
            }
        };
        let persisted = report.store(&self.cache);
        if let Err(e) = self.apply_report(Some(&report)) {
            self.error(&format!("reload failed: {e:#}"));
            return;
        }
        let n = self.updates_available();
        // The count matches the Updates tab — what `U` would do. A pinned
        // backlog is reported alongside rather than folded in, so the
        // number never promises an update that `update --all` will skip.
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

    /// The installed marks are read from the manifest *now*, not from
    /// the rows as they were when the request went out: another
    /// cargo-lbin may have installed or removed a crate while crates.io
    /// was answering, and the mark states installation as a fact.
    /// `checkupdate` is immune by construction (`status_for` validates
    /// the version); search has no such check, so it reloads instead.
    fn finish_search(&mut self, query: String, result: Result<Vec<api::Hit>>) {
        match result {
            Ok(hits) if hits.is_empty() => self.info(&format!("no crates match `{query}`")),
            Ok(hits) => {
                if let Err(e) = self.reload() {
                    self.error(&format!("reload failed: {e:#}"));
                    return;
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
        // The footer is Span-bound like everything else; one funnel,
        // one rule — a reload error carries paths too.
        self.message = Some(Message {
            text: crate::text::sanitize(text),
            kind,
        });
    }
}

fn action_label(action: &PendingAction) -> String {
    match action {
        PendingAction::Update(name) => format!("update {name}"),
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
        PendingAction::Downgrade(name) => format!("downgrade {name}"),
    }
}

/// mm:ss, rolling to h:mm:ss past an hour — a Rust build can outlive
/// both formats' assumptions, but never silently: the widest field
/// grows instead of wrapping.
fn format_elapsed(d: std::time::Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Manifest entries joined with what the report knows about each. The
/// report is consulted per installed version, so a crate updated or
/// installed after the check comes out `Unknown`, not stale.
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
                also: crate::prefixes::describe_for(also, name),
                status,
            }
        })
        .collect()
}

/// `i` input: crate specs (`NAME` or `NAME@VERSION`) separated by
/// whitespace, optionally with `--locked` anywhere — the same shape as
/// the CLI, so nothing new to learn. Specs are validated here so a typo
/// fails in the footer, not after the screen has been handed over; the
/// strings are passed on as typed and parsed again by `install`.
fn parse_install_input(buffer: &str) -> Result<(Vec<String>, bool)> {
    let mut crates = Vec::new();
    let mut locked = false;
    for token in buffer.split_whitespace() {
        if token == "--locked" {
            locked = true;
        } else {
            // Kept as typed, duplicates included: `parse_all` is the one
            // place that decides what a repeated crate means, and it
            // refuses it. Silently collapsing `bat bat` here would let
            // the TUI accept what the CLI rejects.
            crates.push(token.to_owned());
        }
    }
    if crates.is_empty() {
        bail!("no crate name given");
    }
    // The same validation `install` will apply, including "one crate
    // once": `foo foo@1.2.3`, and `foo foo`, are two specs for one crate
    // and are refused here, in the footer, rather than in the terminal
    // after handoff.
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
            also: String::new(),
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
        // control is already Cancelling when the timer looks at it.
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
        app.reload().unwrap();
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
        // Visible, not merely existing: under the Pinned filter an
        // unpinned counterpart does not catch the selection — the
        // documented word is "visible", and this is why. Order matters
        // twice for the test to exercise the branch it claims to: the
        // filter goes on *first* and the position is found under it
        // (an index carried over from the All view would point past the
        // Pinned view and selected_name would answer None before the
        // jump even looks), and foo must be visible-here-hidden-there,
        // which the pinned-here/unpinned-there seeding above provides.
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

    #[test]
    fn a_jump_refuses_while_anything_runs_and_rolls_back_on_a_bad_read() {
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
        app.reload().unwrap();

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

        // Committed only on a successful read: an unreadable other side
        // reports and rolls back — the search panel included, even
        // though the rollback's own reload dismisses it in passing.
        app.search_result = Some(SearchResult {
            query: "foo".into(),
            hits: Vec::new(),
            installed: std::collections::BTreeMap::new(),
        });
        app.jump_to_prefix(broken);
        assert_eq!(app.prefix, here, "a failed read never commits the jump");
        assert!(
            app.visible().is_empty(),
            "the rows still describe `here` (whose manifest is empty), \
             not the unreadable other side"
        );
        assert!(
            app.search_result.is_some(),
            "a jump that did not happen does not cost the person their hits"
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
        app.reload().unwrap();

        // A user-writable prefix: the decision lands in place — no
        // terminal handoff is queued, the file and the row are gone,
        // and the interface says so itself.
        app.remove_confirmed("foo".into());
        assert!(
            app.pending.is_none(),
            "no handoff for a passwordless prefix"
        );
        assert!(!prefix.join("bin/foo").exists(), "the binary is gone");
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

        // Success advances the queue into pending_migrate — and a fully
        // successful member's build warnings are not laundered by the
        // tally.
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
            BuildOutcome::Success,
            &no_tail,
            vec!["foo shadows something".to_owned()],
        );
        assert!(app.pending_migrate.is_some(), "the queue advanced");
        let batch = app.migrate_batch.as_ref().unwrap();
        assert_eq!(batch.moved, 1);
        assert_eq!(batch.noticed.len(), 1, "the shadow warning survived");

        // …a failure on the last member finalizes with a failed panel
        // that carries the successful member's warning section too…
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

        // …and a cancel ends the batch with the queue dropped, never
        // silently continued.
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
    fn captured_kinds_survive_to_the_person() {
        // The exact regression: a warning was once captured, tailed and
        // then dropped by a successful finish; a notice was captured
        // and never shown while the gauge sat at zero units.
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

        // A warning survives a successful finish, pinned to the report.
        // (Raw here: this test injects past the worker's sanitizing
        // boundary on purpose — the boundary itself is exercised below.)
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
        // A path may hold ESC as legally as `a`. build_msg IS the
        // boundary — the worker forwards through it and nothing else —
        // so this test guards the exact function whose removal would
        // re-open the hole.
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
        // The suffix comes pre-formatted from the shared formatter, so
        // this pins both the plumbing and the no-drift property.
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
        assert_eq!(one.also, " [also in /usr/local @0.9.0]");
        let two = rows.iter().find(|r| r.name == "two").unwrap();
        assert_eq!(two.also, "", "installed nowhere else, no suffix");
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
