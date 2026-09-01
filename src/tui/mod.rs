//! Interactive front end over the same commands the CLI runs.
//!
//! The TUI adds no logic of its own. It reads the manifest and the last
//! `checkupdate` report from disk, and every action is one of the existing
//! commands: `r` is `checkupdate`, `u`/`U` are `update NAME`/`update --all`,
//! `i` is `install`, `x` is `remove`, `s` is `info`. Nothing happens
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
//! command would stack a hook per operation. `checkupdate` and `info`
//! only talk to the index; they run on a one-shot thread while the list
//! stays navigable, and their answers are applied on the main thread.

mod ui;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use semver::Version;

use crate::index;
use crate::lock::{Mode, StateLock};
use crate::manifest::{Entry, Manifest};
use crate::report::{Checked, Report, Status};
use crate::validate::validate_name;

/// How often the input poll wakes up to look for finished background work.
const TICK: Duration = Duration::from_millis(100);

/// One installed crate as the list shows it.
pub struct Row {
    pub name: String,
    pub version: String,
    pub bins: Vec<String>,
    pub locked: bool,
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
}

impl Filter {
    pub const ALL: [Filter; 2] = [Filter::All, Filter::Updates];

    pub fn index(self) -> usize {
        match self {
            Filter::All => 0,
            Filter::Updates => 1,
        }
    }

    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Updates,
            Filter::Updates => Filter::All,
        }
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
    Info,
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
    Install { crates: Vec<String>, locked: bool },
    Remove(String),
}

/// Background work in flight. At most one at a time: the footer shows one
/// busy label and the user should know what it stands for.
enum Job {
    Check(Receiver<Result<Vec<Checked>>>),
    Info {
        name: String,
        rx: Receiver<Result<Vec<index::Release>>>,
    },
}

impl Job {
    fn label(&self) -> String {
        match self {
            Job::Check(_) => "checking crates.io for updates…".to_owned(),
            Job::Info { name, .. } => format!("looking up `{name}` on crates.io…"),
        }
    }
}

/// A finished lookup, shown in the details panel until dismissed.
pub struct InfoResult {
    pub name: String,
    pub text: String,
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
    pub info_result: Option<InfoResult>,
    pub show_help: bool,
    pending: Option<PendingAction>,
    job: Option<Job>,
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
            info_result: None,
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
    /// check, and when an info result is about to be presented — the
    /// TUI may have been open for an hour while another cargo-lbin
    /// changed the prefix, and each of those is a moment the user is
    /// about to be shown or act on the prefix's state. The state lock is
    /// held only for the manifest read, never while the TUI idles.
    fn reload(&mut self) -> Result<()> {
        // An info result states "installed: yes/no" as a fact about the
        // prefix. Anything that re-reads the prefix — a command's return,
        // `r`, a newer lookup — is exactly the moment that fact may have
        // stopped being true, so it goes. `finish_info` reloads first
        // and sets the new result after, so this never eats a fresh one.
        self.info_result = None;
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
        self.rows = rows_from(&manifest, report);
        self.report_age = report.map(Report::age);
        self.clamp_selection();
        Ok(())
    }

    /// Rows under the current filter, paired with their index in `rows`.
    pub fn visible(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| match self.filter {
                Filter::All => true,
                Filter::Updates => matches!(r.status, RowStatus::Outdated(_)),
            })
            .collect()
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.visible().get(self.selected).copied()
    }

    pub fn updates_available(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.status, RowStatus::Outdated(_)))
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
            terminal.draw(|frame| ui::draw(frame, self))?;

            if let Some(action) = self.pending.take() {
                self.run_in_terminal(terminal, &action)?;
                continue;
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

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.show_help {
            self.show_help = false;
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
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.info_result.take().is_none() {
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
            KeyCode::Tab | KeyCode::BackTab => {
                self.filter = self.filter.next();
                self.clamp_selection();
            }
            KeyCode::Char('r') => self.start_check(),
            KeyCode::Char('s') => self.open_input(InputPurpose::Info),
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
                Ok((crates, locked)) => self.queue(PendingAction::Install { crates, locked }),
                Err(e) => self.error(&format!("{e:#}")),
            },
            InputPurpose::Info => match parse_info_input(buffer) {
                Ok(name) => self.start_info(name),
                Err(e) => self.error(&format!("{e:#}")),
            },
        }
    }

    fn open_input(&mut self, purpose: InputPurpose) {
        if self.job.is_some() && purpose == InputPurpose::Info {
            self.error("busy; wait for the current lookup to finish");
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

    fn start_info(&mut self, name: String) {
        // The old result is not a placeholder for the new one: a failed
        // lookup must not leave the previous crate's answer on screen
        // under a footer that talks about a different name.
        self.info_result = None;
        let (tx, rx) = mpsc::channel();
        let query = name.clone();
        thread::spawn(move || {
            let _ = tx.send(index::releases(&query));
        });
        self.job = Some(Job::Info { name, rx });
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
            Job::Info { name, rx } => match rx.try_recv() {
                Ok(result) => self.finish_info(name, result),
                Err(TryRecvError::Empty) => self.job = Some(Job::Info { name, rx }),
                Err(TryRecvError::Disconnected) => {
                    bail!("info worker aborted; the terminal was reset by the panic")
                }
            },
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
        match persisted {
            Ok(()) => self.info(&format!("checked: {n} update(s) available")),
            Err(e) => self.warn(&format!(
                "checked: {n} update(s) available; report not saved: {e:#}"
            )),
        }
    }

    /// The "installed" line is read from the manifest *now*, not from the
    /// rows as they were when the request went out: another cargo-lbin
    /// may have installed or removed the crate while the index was
    /// answering, and `describe_info` states installation as a fact.
    /// `checkupdate` is immune by construction (`status_for` validates
    /// the version); info has no such check, so it reloads instead.
    fn finish_info(&mut self, name: String, result: Result<Vec<index::Release>>) {
        match result {
            Ok(releases) => {
                if let Err(e) = self.reload() {
                    self.error(&format!("reload failed: {e:#}"));
                    return;
                }
                let installed = self.rows.iter().find(|r| r.name == name).map(|r| Entry {
                    version: r.version.clone(),
                    bins: r.bins.clone(),
                    locked: r.locked,
                });
                let text = crate::describe_info(&name, &releases, installed.as_ref());
                self.info_result = Some(InfoResult { name, text });
                self.info("info shown; Esc to dismiss");
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
        self.message = Some(Message {
            text: text.to_owned(),
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
    }
}

/// Manifest entries joined with what the report knows about each. The
/// report is consulted per installed version, so a crate updated or
/// installed after the check comes out `Unknown`, not stale.
fn rows_from(manifest: &Manifest, report: Option<&Report>) -> Vec<Row> {
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
                status,
            }
        })
        .collect()
}

/// `i` input: crate names separated by whitespace, optionally with
/// `--locked` anywhere — the same shape as the CLI, so nothing new to
/// learn. Names are validated here so a typo fails in the footer, not
/// after the screen has been handed over.
fn parse_install_input(buffer: &str) -> Result<(Vec<String>, bool)> {
    let mut crates = Vec::new();
    let mut locked = false;
    for token in buffer.split_whitespace() {
        if token == "--locked" {
            locked = true;
        } else {
            validate_name(token)?;
            if !crates.iter().any(|c| c == token) {
                crates.push(token.to_owned());
            }
        }
    }
    if crates.is_empty() {
        bail!("no crate name given");
    }
    Ok((crates, locked))
}

/// `s` input: exactly one crate name.
fn parse_info_input(buffer: &str) -> Result<String> {
    let mut tokens = buffer.split_whitespace();
    let name = tokens.next().context("no crate name given")?;
    if tokens.next().is_some() {
        bail!("info takes one crate name here; use the CLI for several");
    }
    validate_name(name)?;
    Ok(name.to_owned())
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
        let rows = rows_from(&m, Some(&report));
        let status: Vec<(&str, &RowStatus)> =
            rows.iter().map(|r| (r.name.as_str(), &r.status)).collect();
        assert_eq!(status[0], ("bat", &RowStatus::Outdated(v("0.26.1"))));
        assert_eq!(status[1], ("fd", &RowStatus::Unknown));
        assert_eq!(status[2], ("ripgrep", &RowStatus::UpToDate));

        // No report at all: everything unknown, nothing claimed.
        let rows = rows_from(&m, None);
        assert!(rows.iter().all(|r| r.status == RowStatus::Unknown));
    }

    #[test]
    fn install_input_mirrors_cli_shape() {
        assert_eq!(
            parse_install_input("ripgrep --locked bat ripgrep").unwrap(),
            (vec!["ripgrep".to_owned(), "bat".to_owned()], true)
        );
        assert_eq!(
            parse_install_input("  fd  ").unwrap(),
            (vec!["fd".to_owned()], false)
        );
        assert!(parse_install_input("").is_err());
        assert!(parse_install_input("--locked").is_err());
        assert!(parse_install_input("../evil").is_err());
    }

    #[test]
    fn info_input_takes_one_name() {
        assert_eq!(parse_info_input(" bat ").unwrap(), "bat");
        assert!(parse_info_input("").is_err());
        assert!(parse_info_input("bat fd").is_err());
        assert!(parse_info_input("bad name!").is_err());
    }

    #[test]
    fn filter_cycles_and_indexes() {
        assert_eq!(Filter::All.next(), Filter::Updates);
        assert_eq!(Filter::Updates.next(), Filter::All);
        for (i, f) in Filter::ALL.iter().enumerate() {
            assert_eq!(f.index(), i);
        }
    }
}
