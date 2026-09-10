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

/// One installed crate as the list shows it.
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
    action: PendingAction,
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
    /// Placement wants sudo revalidated on a real terminal. The worker
    /// blocks on the auth channel until the run loop — the only place
    /// that owns the terminal — answers.
    NeedAuth,
    /// The pipeline finished, one way or the other.
    Done(Result<()>),
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
        /// Set by `poll_job` when the worker asked for revalidation;
        /// answered by the run loop, which owns the terminal.
        needs_auth: bool,
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
            search_result: None,
            build_report: None,
            pending_build: None,
            ticks: 0,
            show_help: false,
            pending: None,
            job: None,
            should_quit: false,
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
            ..
        }) = &self.job
        else {
            return None;
        };
        let frame = FRAMES[self.ticks % FRAMES.len()];
        // A pipeline notice is the live truth of the moment — "waiting
        // for the state lock…" beats a gauge frozen at zero units, which
        // is exactly the impression the notice exists to prevent.
        if let Some(note) = status_note {
            return Some(format!("{frame} {name}: {note}"));
        }
        let unit_word = if *units_started == 1 { "unit" } else { "units" };
        let mut line = format!("{frame} building {name} · {units_started} {unit_word}");
        if let Some(current) = current {
            let _ = write!(line, " · compiling {current}");
        }
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
            terminal.draw(|frame| ui::draw(frame, self))?;

            if let Some(action) = self.pending.take() {
                self.run_in_terminal(terminal, &action)?;
                continue;
            }
            if let Some((spec, locked)) = self.pending_build.take() {
                self.start_build(terminal, &spec, locked)?;
                continue;
            }
            if matches!(
                &self.job,
                Some(Job::Build {
                    needs_auth: true,
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
        let policy = crate::privileged::Policy::for_prefix(&self.prefix);
        // The pipeline's own union (bin + state) plus the lock file —
        // the worker's first privileged touch. Mixed ownership needs the
        // state term up front; lock preparation is consulted only where
        // escalation is possible at all.
        let escalate = match crate::install_needs_privilege(policy, &self.prefix) {
            Ok(escalate) => escalate,
            Err(e) => {
                self.error(&format!("{e:#}"));
                return Ok(());
            }
        } || (matches!(policy.sudo, crate::privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(&self.prefix));
        if escalate {
            let fresh = match crate::privileged::credentials_fresh() {
                Ok(fresh) => fresh,
                Err(e) => {
                    self.error(&format!("{e:#}"));
                    return Ok(());
                }
            };
            let prefix = self.prefix.clone();
            if !fresh
                && let Err(e) =
                    Self::suspended(terminal, || crate::privileged::preauthorize(&prefix, true))?
            {
                self.error(&format!("{e:#}"));
                return Ok(());
            }
            // Captured placement runs `sudo -n`, so a sudo that does not
            // cache credentials (timestamp_timeout=0, per-TTY quirks)
            // would be asked a question it cannot voice. Detect that now
            // — right after a successful validation the timestamp should
            // be warm — and hand the terminal over the old way instead
            // of starting a build that must end in an error.
            match crate::privileged::credentials_fresh() {
                Ok(true) => {}
                Ok(false) => {
                    self.info("sudo does not cache credentials here; handing the terminal over");
                    self.pending = Some(PendingAction::Install {
                        crates: vec![raw_spec.to_owned()],
                        locked,
                    });
                    return Ok(());
                }
                Err(e) => {
                    self.error(&format!("{e:#}"));
                    return Ok(());
                }
            }
        }
        let (tx, rx) = mpsc::channel();
        let (auth_tx, auth_rx) = mpsc::channel();
        let prefix = self.prefix.clone();
        let name = spec.name.clone();
        let worker_tx = tx.clone();
        std::thread::spawn(move || {
            let line_tx = worker_tx.clone();
            let result = crate::tui_install_one(
                &prefix,
                &spec,
                locked,
                &mut |k: crate::LineKind, l: &str| {
                    let _ = line_tx.send(build_msg(k, l));
                },
                &mut || match crate::privileged::credentials_fresh() {
                    // The common case: the up-front validation is still
                    // fresh and placement proceeds without a word.
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        let _ = worker_tx.send(BuildMsg::NeedAuth);
                        match auth_rx.recv() {
                            Ok(true) => Ok(()),
                            Ok(false) => anyhow::bail!("sudo authentication failed"),
                            Err(_) => anyhow::bail!("the interface went away mid-authorization"),
                        }
                    }
                    Err(e) => Err(e),
                },
            );
            let _ = tx.send(BuildMsg::Done(result));
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
            needs_auth: false,
        });
        Ok(())
    }

    /// The worker hit the placement checkpoint with a stale credential
    /// timestamp — the build outlived it. Revalidate on the real
    /// terminal and let the worker proceed (or fail, and say so).
    fn answer_auth(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let prefix = self.prefix.clone();
        let outcome = Self::suspended(terminal, || crate::privileged::preauthorize(&prefix, true))?;
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
        if let Some(Job::Build {
            auth_tx,
            needs_auth,
            ..
        }) = &mut self.job
        {
            *needs_auth = false;
            let _ = auth_tx.send(ok);
        }
        Ok(())
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

    /// A finished captured install: reload, then speak in the pipeline's
    /// own words when it left any — the "installed …" note is more
    /// informative than a generic "finished".
    fn finish_build(
        &mut self,
        name: &str,
        result: Result<()>,
        tail: &VecDeque<String>,
        warnings: Vec<String>,
    ) {
        // The pipeline's error outranks a reload error: the tail and the
        // log path are the diagnosis, and a failed screen refresh must
        // not eat them. On success the roles flip — the reload *is* the
        // remaining work, so its failure is the headline.
        let reload = self.reload();
        match result {
            Ok(()) => {
                let note = tail
                    .iter()
                    .rev()
                    .find(|l| l.starts_with("installed "))
                    .cloned()
                    .unwrap_or_else(|| format!("install {name} finished"));
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
                        title: format!("install {name}: warnings"),
                        lines,
                        failed: false,
                    });
                    self.warn(&format!(
                        "{note} — with warnings in the panel; Esc/Enter dismisses"
                    ));
                }
            }
            Err(e) => {
                // An anyhow chain carries paths too; same boundary rule.
                let text = format!("{e:#}");
                let mut lines: Vec<String> = text.lines().map(crate::text::sanitize).collect();
                if lines.len() <= 1 {
                    // A placement or commit error carries no tail of its
                    // own; give the panel the build's last lines instead.
                    lines.extend(tail.iter().rev().take(8).rev().cloned());
                }
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
            self.should_quit = true;
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
                    self.error("a build is running; Ctrl-C abandons it");
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Esc => {
                if self.search_result.take().is_some() {
                    // Dismissing a result is fine mid-build; only the
                    // exit is held back, symmetrically with `q`.
                } else if matches!(self.job, Some(Job::Build { .. })) {
                    self.error("a build is running; Ctrl-C abandons it");
                } else {
                    self.should_quit = true;
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
            KeyCode::Char('x') => {
                if let Some(row) = self.selected_row() {
                    let prompt = format!("remove {} ({})? [y/N]", row.name, row.bins.join(", "));
                    self.confirm = Some(Confirm {
                        prompt,
                        action: PendingAction::Remove(row.name.clone()),
                    });
                }
            }
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if matches!(key.code, KeyCode::Char('y' | 'Y')) {
            self.queue(confirm.action);
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
                mut needs_auth,
            } => {
                // Drain everything queued since the last frame: a fast
                // build emits many lines per tick, and rendering one line
                // per 100ms would show a gauge lagging minutes behind.
                let mut done: Option<Result<()>> = None;
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
                        Ok(BuildMsg::NeedAuth) => needs_auth = true,
                        Ok(BuildMsg::Done(result)) => {
                            done = Some(result);
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            bail!("build worker aborted; the terminal was reset by the panic")
                        }
                    }
                }
                match done {
                    Some(result) => self.finish_build(&name, result, &tail, warnings),
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
                            needs_auth,
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
            needs_auth: false,
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
        tx.send(BuildMsg::Done(Ok(()))).unwrap();
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
                BuildMsg::NeedAuth | BuildMsg::Done(_) => {
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
        assert!(two.also.is_empty(), "installed nowhere else, no suffix");
    }
}
