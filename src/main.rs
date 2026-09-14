mod api;
mod index;
mod json;
mod lock;
mod manifest;
mod prefixes;
mod privileged;
#[cfg(feature = "tui")]
mod progress;
mod report;
mod shadow;
mod stage;
mod text;
#[cfg(feature = "tui")]
mod tui;
mod validate;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use lock::{Mode, StateLock};
use manifest::{Entry, Manifest};
use report::{Checked, Report, Status};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use validate::{InstallSpec, validate_name};

/// Exit codes for `checkupdate`, following the pacman-contrib
/// `checkupdates` convention: 0 = updates available, 2 = none, 1 = error.
const EXIT_UPDATES: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_NO_UPDATES: u8 = 2;

#[derive(Parser)]
#[command(
    name = "cargo-lbin",
    version,
    about = "Install crates.io binaries into <prefix>/bin (default /usr/local/bin)",
    long_about = "Builds crates as the invoking user in a stage directory, then installs \
the resulting binaries into <prefix>/bin, escalating via sudo only for file \
placement. State lives in <prefix>/share/cargo-lbin/manifest.json. Sources are \
crates.io exclusively."
)]
struct Cli {
    // Precedence: --prefix, then $CARGO_LBIN_PREFIX, then /usr/local (clap's
    // env support orders and documents it).
    // `help =`, not a doc comment: `<prefix>/bin` parses as an unclosed HTML
    // tag under rustdoc.
    #[arg(
        long,
        global = true,
        env = "CARGO_LBIN_PREFIX",
        help = "Installation prefix; binaries land in <prefix>/bin",
        default_value = "/usr/local"
    )]
    prefix: PathBuf,

    /// Install for this user only: an alias for --prefix ~/.local
    ///
    /// Binaries land in ~/.local/bin (on most distributions already on
    /// PATH), state in ~/.local/share/cargo-lbin; sudo is never used.
    // Overrides $CARGO_LBIN_PREFIX too: ad hoc beats ambient; only an
    // explicit --prefix conflicts. Clap counts env as "present", so the
    // conflict is enforced below via value_source.
    #[arg(long, global = true)]
    user: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

/// The prefix `--user` stands for: `$HOME/.local` — bin/ blessed by
/// file-hierarchy(7) and on PATH almost everywhere; state deliberately
/// follows the prefix, not `XDG_DATA_HOME`.
fn user_prefix() -> Result<PathBuf> {
    #[allow(deprecated)] // un-deprecated in std, attribute kept for older toolchain docs
    std::env::home_dir()
        .context("--user needs a home directory, and none could be determined")
        .map(|home| home.join(".local"))
}

#[derive(Subcommand)]
enum Cmd {
    /// Build crates from crates.io and install their binaries
    ///
    /// `NAME@VERSION` installs exactly that version and pins it.
    Install {
        #[arg(required = true, value_name = "NAME[@VERSION]")]
        crates: Vec<String>,
        /// Build with the crate's committed Cargo.lock (reproducible; skips
        /// newer dependency releases until the crate itself releases)
        #[arg(long)]
        locked: bool,
    },
    /// Remove previously installed binaries
    Remove {
        #[arg(required = true)]
        crates: Vec<String>,
    },
    /// Check the manifest's claims against the disk
    ///
    /// Read-only, always — verify does not even prepare the lock; it
    /// takes a shared lock only where one already exists. Every entry
    /// must parse, every declared binary must exist as an executable
    /// regular file, and no two entries may claim one binary name —
    /// anything else is an invariant violation and the command exits
    /// non-zero. Observations about the surroundings (the crate also
    /// installed under another known prefix, another executable with a
    /// managed binary's name on PATH, stage directories left by a
    /// crashed build) are warnings and leave the exit status at zero
    /// (a claim that could not be checked at all — permissions, I/O —
    /// is an error, not a warning: an unverifiable claim on one's own
    /// prefix is itself not healthy):
    /// they measure the environment, not the managed state. A finding
    /// names the repair command where lbin has an unambiguous one, and
    /// otherwise describes the state and leaves the decision to you;
    /// verify itself never writes, prompts, or escalates.
    Verify {
        /// Machine-readable output (schema documented in README)
        #[arg(long)]
        json: bool,
    },
    /// Remove build debris from the cache; every removal is opt-in
    ///
    /// `--stages` removes the stage directories `verify` reports as
    /// ownerless — the set is `verify`'s own, so the two cannot drift.
    /// Ownerless is a liveness heuristic, not proof: the owning
    /// cargo-lbin is gone, but a build it spawned may survive it and
    /// still hold the directory, which is why removal is explicit and
    /// never a default. `--logs-older-than DAYS` removes failure logs
    /// past that age — the person names the retention, lbin does not
    /// invent one. The cache is the user's own; no lock is taken and
    /// sudo is never used. `--dry-run` lists what would go and removes
    /// nothing.
    Clean {
        /// List what would be removed without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Remove stage directories whose owning cargo-lbin is gone
        #[arg(long)]
        stages: bool,
        /// Remove build logs older than this many days
        #[arg(long, value_name = "DAYS")]
        logs_older_than: Option<u64>,
    },
    /// Pin crates to their installed version
    ///
    /// A pin declares the version, not just a hold against the next
    /// update: `update --all` leaves the crate out, `update NAME` and
    /// `install NAME` are refused until unpinned, and `migrate` rebuilds
    /// exactly the pinned version at the destination (an unpinned crate
    /// migrates to the latest).
    Pin {
        #[arg(required = true)]
        crates: Vec<String>,
    },
    /// Release a pin
    Unpin {
        #[arg(required = true)]
        crates: Vec<String>,
    },
    /// List pinned crates and whether newer versions exist (read-only,
    /// no sudo).
    ///
    /// Reads the last recorded `checkupdate` by default; `--check` asks
    /// crates.io about the pinned crates instead, without touching the
    /// recorded report. Exit codes as `checkupdate`: 0 a pinned crate is
    /// known to have a newer version, 2 none is known to, 1 error
    Pinned {
        /// Query crates.io now instead of reading the recorded check
        #[arg(long)]
        check: bool,
        /// Machine-readable output (schema documented in README)
        #[arg(long)]
        json: bool,
    },
    /// List installed crates and their binaries
    List {
        /// Machine-readable output (schema documented in README)
        #[arg(long)]
        json: bool,
    },
    /// Interactive front end over the same commands (starts from disk;
    /// nothing runs unprompted)
    #[cfg(feature = "tui")]
    Tui,
    /// Show one or more crates exactly by name: latest versions and
    /// whether they are installed under the prefix
    Info {
        #[arg(required = true)]
        crates: Vec<String>,
    },
    /// Find crates on crates.io by keyword; marks the ones installed
    /// under the prefix
    Search {
        /// Search terms (joined with spaces)
        #[arg(required = true)]
        query: Vec<String>,
        /// Maximum number of results (1-100)
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=100))]
        limit: u8,
    },
    /// Check crates.io for newer versions (read-only, no sudo).
    /// Exit codes: 0 updates available, 2 none, 1 error
    Checkupdate {
        /// Machine-readable output (schema documented in README)
        #[arg(long)]
        json: bool,
    },
    /// Pick an older version of an installed crate from crates.io,
    /// install it and pin it
    Downgrade {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Print a shell completion script for cargo-lbin's commands and
    /// flags to stdout
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Write man pages (roff) for cargo-lbin and every subcommand into DIR
    ///
    /// For packagers: one page per command, named `cargo-lbin.1` and
    /// `cargo-lbin-<subcommand>.1`, generated from the same clap
    /// definitions --help prints — the two can never drift.
    Man {
        /// Directory to write the pages into (created if missing)
        dir: PathBuf,
    },
    /// Rebuild an installed crate under another prefix, then retire it
    /// here
    ///
    /// The crate is rebuilt at the destination — never copied, so
    /// provenance is re-established by the same pipeline as `install` —
    /// carrying `--locked` and the pin. The version follows the pin: a
    /// pinned crate is rebuilt at exactly its pinned version, an
    /// unpinned one gets the latest available, because without a pin the
    /// version was never part of the intent. The entry here is
    /// retired only after the destination has fully committed; a failure
    /// in between leaves the crate installed in both prefixes — never in
    /// neither — with the command's output naming both paths (the
    /// listing's `[also in …]` shows it too, but only for the known pair
    /// `/usr/local` and `~/.local`). A crate already installed at the
    /// destination is refused — no `--force` by design.
    Migrate {
        /// Crates to migrate (use --all for every installed crate)
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        crates: Vec<String>,
        /// Migrate every crate installed under the prefix
        #[arg(long)]
        all: bool,
        /// Destination prefix
        #[arg(long, value_name = "PREFIX")]
        to: PathBuf,
        /// Skip the confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Update installed crates to their newest crates.io versions
    // A list or `--all`, never a bare `update`: rebuilding system binaries
    // must be told, not guessed.
    Update {
        /// Crates to update (use --all for every installed crate)
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        crates: Vec<String>,
        /// Update every installed crate that has a newer version
        #[arg(long)]
        all: bool,
        /// Skip the confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
}

fn main() -> ExitCode {
    // Accept `cargo-lbin ...` and `cargo lbin ...`: strip cargo's "lbin"
    // token so clap sees one argv.
    let args = std::env::args_os()
        .enumerate()
        .filter_map(|(i, a)| (!(i == 1 && a == *"lbin")).then_some(a));
    let matches = <Cli as clap::CommandFactory>::command().get_matches_from(args);
    let mut cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    if cli.user {
        // Two explicit answers deserve an error; the ambient env yields to the
        // ad hoc flag — that is what an alias is for.
        if matches.value_source("prefix") == Some(clap::parser::ValueSource::CommandLine) {
            eprintln!("error: the argument '--user' cannot be used with an explicit '--prefix'");
            return ExitCode::from(EXIT_ERROR);
        }
        cli.prefix = match user_prefix() {
            Ok(prefix) => prefix,
            Err(e) => {
                eprintln!("error: {e:#}");
                return ExitCode::from(EXIT_ERROR);
            }
        };
    }
    // Root would run cargo — build scripts and proc macros — with root
    // privileges, undoing the design's one security property; fail loudly.
    // Parse first so `sudo cargo-lbin --help` works. The override is for
    // root-only environments (containers, CI).
    // SAFETY: geteuid cannot fail and has no preconditions.
    if unsafe { libc::geteuid() } == 0
        && std::env::var_os("CARGO_LBIN_ALLOW_ROOT").is_none_or(|v| v != "1")
    {
        eprintln!("error: cargo-lbin must not be run as root");
        eprintln!("run it as your normal user; sudo is requested only when required for placement");
        eprintln!("(set CARGO_LBIN_ALLOW_ROOT=1 only in environments where root is the only user)");
        return ExitCode::from(EXIT_ERROR);
    }
    let result = match cli.cmd {
        Cmd::Install { ref crates, locked } => cmd_install(&cli.prefix, crates, locked),
        Cmd::Remove { ref crates } => cmd_remove(&cli.prefix, crates),
        Cmd::Verify { json } => cmd_verify(&cli.prefix, json),
        Cmd::Pin { ref crates } => cmd_set_pinned(&cli.prefix, crates, true),
        Cmd::Unpin { ref crates } => cmd_set_pinned(&cli.prefix, crates, false),
        Cmd::Pinned { check, json } => return cmd_pinned(&cli.prefix, check, json),
        Cmd::List { json } => cmd_list(&cli.prefix, json),
        #[cfg(feature = "tui")]
        Cmd::Tui => tui::run(&cli.prefix),
        Cmd::Info { ref crates } => cmd_info(&cli.prefix, crates),
        Cmd::Search { ref query, limit } => cmd_search(&cli.prefix, query, limit),
        Cmd::Checkupdate { json } => return cmd_checkupdate(&cli.prefix, json),
        Cmd::Clean {
            dry_run,
            stages,
            logs_older_than,
        } => cmd_clean(dry_run, stages, logs_older_than),
        Cmd::Man { ref dir } => cmd_man(dir),
        Cmd::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
        Cmd::Downgrade { ref name } => cmd_downgrade(&cli.prefix, name),
        Cmd::Update {
            ref crates,
            all,
            yes,
        } => cmd_update(&cli.prefix, crates, all, yes),
        Cmd::Migrate {
            ref crates,
            all,
            ref to,
            yes,
        } => cmd_migrate(&cli.prefix, to, crates, all, yes),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("cargo-lbin"));
    }
    let home = std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".cache/cargo-lbin"))
}

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

    fn phase(&self) -> BuildPhase {
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
    fn warning(&mut self, s: &str) {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => eprintln!("{s}"),
            #[cfg(feature = "tui")]
            Frontend::Captured { on_line, .. } => on_line(LineKind::Warning, s),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// Whether the pipeline's own preauthorize should run: a captured
    /// frontend already validated and re-checks at the checkpoint, so a
    /// prompt from inside would be exactly the hidden one to prevent.
    fn wants_preauthorize(&self) -> bool {
        matches!(self, Frontend::Terminal | Frontend::Checkpointed { .. })
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

    /// The placement checkpoint: no-op on the terminal, the captured
    /// frontend's sudo re-validation — a build can outlive the credential
    /// timestamp. Called only when placement will escalate.
    // `prefix` is consumed only by the tui arm; the parameter is the
    // contract either way.
    #[cfg_attr(not(feature = "tui"), allow(unused_variables))]
    fn before_placement(&mut self, prefix: &Path) -> Result<()> {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => Ok(()),
            #[cfg(feature = "tui")]
            Frontend::Captured {
                before_placement, ..
            } => before_placement(prefix),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }
}

/// Build one crate, verify destination ownership, place, clean up
/// obsolete binaries and commit the manifest — all before the next
/// crate, so a mid-batch failure never leaves installed files
/// unrecorded. Returns the version the manifest committed — for a
/// `None` request, whatever cargo's resolution picked, which no caller
/// could know beforehand. A fresh per-crate stage, wiped before and
/// removed only after the commit: a shared stage let stale binaries
/// fail builds and let a different `--locked` be skipped as "already
/// installed".
// Eight arguments like `place_and_commit`, same reason: one install's
// parameters; a struct would be built only to be destructured here.
#[allow(clippy::too_many_arguments)]
fn install_and_commit(
    prefix: &Path,
    cache: &Path,
    manifest: &mut Manifest,
    name: &str,
    version: Option<&Version>,
    locked: bool,
    pin: PinPolicy,
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
    // UX-only early form of the policy check: fail (and prompt) before a
    // multi-minute build, not after; enforcement proper lives at every
    // privileged call site.
    let initial_escalate = install_needs_privilege(policy, prefix)?;
    if frontend.wants_preauthorize() {
        privileged::preauthorize(prefix, initial_escalate)?;
    }
    // Per-PID stage: the state lock serializes per *prefix*, so two runs
    // on different prefixes may build the same crate — one wiping the
    // other's stage must be structurally impossible.
    let stage_dir = cache
        .join("stage")
        .join(std::process::id().to_string())
        .join(name);
    if stage_dir.exists() {
        fs::remove_dir_all(&stage_dir)
            .with_context(|| format!("clearing stale stage {}", stage_dir.display()))?;
    }
    let built = frontend.build(name, version, locked, &stage_dir, cache)?;
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
    for w in shadow_warnings(prefix, &new_bins) {
        frontend.warning(&w);
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
        if install_needs_privilege(policy, prefix)? {
            frontend.before_placement(prefix)?;
        }
        frontend.placement_begins()
    })();
    #[cfg(feature = "tui")]
    if let Err(e) = &checkpoints
        && e.downcast_ref::<BuildCancelled>().is_some()
    {
        let _ = fs::remove_dir_all(&stage_dir);
    }
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
    // Stage removal is deliberately last: a stage surviving a failure is
    // forensic evidence of exactly the build that caused it.
    let _ = fs::remove_dir_all(&stage_dir);
    if let Some(pid_dir) = stage_dir.parent() {
        // Best effort, non-recursive: succeeds only once our PID directory
        // is empty, i.e. after the last crate of this run.
        let _ = fs::remove_dir(pid_dir);
    }
    Ok(installed)
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
        },
    )?;
    // Announced only after the commit — an "installed" before `store`
    // could be followed by its own undoing — and keyed off the committed
    // state, which is what `unpin` would change.
    let pin_note = if pinned {
        format!(" [pinned; `cargo lbin unpin {name}` to allow updates]")
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

/// One warning per binary that a PATH entry outside the prefix already
/// provides, naming file, owner (if the package manager says) and PATH
/// order. Warning, not refusal (see `shadow`); only for names new to
/// this crate — shadowing that arises later is external drift, and
/// re-warning on every update would be the price of catching it.
fn shadow_warnings(prefix: &Path, bins: &[String]) -> Vec<String> {
    // Install frontends print these raw, so the severity word travels in
    // the string; `verify` takes the bare notes and frames its own — no
    // "warning: warning:".
    shadow_notes(prefix, bins)
        .into_iter()
        .map(|n| format!("warning: {n}"))
        .collect()
}

/// The verify-side sibling of `shadow_notes`: the same scan, but each
/// shadow keeps its subjects as data — `bin` and the shadowing `path` —
/// so a `--json` consumer reads fields, not `message`.
fn shadow_findings(prefix: &Path, bins: &[String]) -> Vec<Finding> {
    if bins.is_empty() {
        return Vec::new();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let prefix_bin = prefix.join("bin");
    shadow::find_shadows(&path_var, &prefix_bin, bins, &cwd, shadow::is_executable)
        .iter()
        .map(|s| {
            let owner = shadow::owner_of(&s.existing);
            Finding {
                bin: Some(s.bin.clone()),
                path: Some(s.existing.clone()),
                ..Finding::plain(
                    "path-shadow",
                    shadow::describe(s, &prefix_bin, owner.as_deref()),
                )
            }
        })
        .collect()
}

/// `shadow_warnings` without the severity word: the raw
/// `shadow::describe` lines, for callers that add their own framing.
fn shadow_notes(prefix: &Path, bins: &[String]) -> Vec<String> {
    if bins.is_empty() {
        return Vec::new();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    // The cwd only anchors relative PATH entries; unreadable means those
    // cannot be judged, and a possibly-wrong warning is worse than none.
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let prefix_bin = prefix.join("bin");
    shadow::find_shadows(&path_var, &prefix_bin, bins, &cwd, shadow::is_executable)
        .iter()
        .map(|s| {
            let owner = shadow::owner_of(&s.existing);
            shadow::describe(s, &prefix_bin, owner.as_deref())
        })
        .collect()
}

/// Will an install into `prefix` need privileged writes? The union of
/// bin and the state directory — either alone can be the one needing
/// sudo. One answer for the pipeline checkpoint and (composed) the TUI
/// preflight, so the two cannot drift. A missing state dir probes as
/// writable where the prefix allows creating it.
fn install_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    Ok(policy.probe_destination(&prefix.join("bin"))?
        || policy.probe_destination(&prefix.join("share/cargo-lbin"))?)
}

/// One verify finding as data: `message` is the human finding text
/// (hint embedded; the text renderers add their own framing, e.g. the
/// severity word) and every other field is the datum a `--json`
/// consumer would otherwise have to parse back out of it — `kind` a
/// stable machine name, `crate`/`bin`/`path` the subjects where the
/// finding has them (else null), `hint` the bare pasteable repair
/// command where one is unambiguous (a reinstall for a broken binary;
/// stale-stages carries none — its removal is a heuristic's verdict).
/// The text surfaces read only `message` and stay byte-identical.
#[derive(Debug, serde::Serialize)]
pub(crate) struct Finding {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    #[serde(rename = "crate")]
    pub(crate) krate: Option<String>,
    pub(crate) bin: Option<String>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) hint: Option<String>,
}

impl Finding {
    fn plain(kind: &'static str, message: String) -> Self {
        Self {
            kind,
            message,
            krate: None,
            bin: None,
            path: None,
            hint: None,
        }
    }
    fn for_crate(kind: &'static str, krate: &str, message: String) -> Self {
        Self {
            krate: Some(krate.to_owned()),
            ..Self::plain(kind, message)
        }
    }
    fn for_bin(kind: &'static str, krate: &str, bin: &str, message: String) -> Self {
        Self {
            bin: Some(bin.to_owned()),
            ..Self::for_crate(kind, krate, message)
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Everything `verify` found, split as the exit status needs it:
/// errors are broken invariants *and* claims that could not be checked
/// (hence "verification error(s)" in the summaries); warnings are
/// surroundings worth a look. Data, not placement — the split is the
/// contract both renderers speak.
pub(crate) struct VerifyReport {
    /// `Some(n)` when the manifest was counted — `Some(0)` on a fresh
    /// prefix is knowledge — and `None` when it could not be: a missing
    /// brace may hide forty entries, and "0 crates" there would be the
    /// audit guessing in its own verdict line.
    pub(crate) crates: Option<usize>,
    pub(crate) errors: Vec<Finding>,
    pub(crate) warnings: Vec<Finding>,
}

/// The whole audit, read-only in the strictest sense: verify does not
/// even prepare the lock — shared only where one exists, lockless
/// otherwise (atomic manifest placement keeps that safe from torn
/// files). Never writes, prompts or escalates; a finding names the
/// repair only where lbin has an unambiguous one.
///
/// Two phases by lock scope: hard invariants under the shared lock;
/// environmental observations after it drops — nobody's install should
/// wait on `rpm -qf`.
pub(crate) fn verify_prefix(
    prefix: &Path,
    lock_notice: &mut dyn FnMut(&str),
) -> Result<VerifyReport> {
    let (crates, errors, names, all_bins) = {
        let _lock = StateLock::acquire_shared_existing(prefix, lock_notice)?;
        // `load_unvalidated`: the validated loader refuses exactly the states
        // verify exists to name. A manifest that does not deserialize is the
        // audit's one finding, not a failure of the audit.
        let manifest = match Manifest::load_unvalidated(prefix) {
            Ok(manifest) => manifest,
            // Two worlds: an I/O failure says nothing about the content (no
            // "restore" advice); only bytes serde refused earn repair-or-restore.
            Err(e) => {
                let kind = if e.downcast_ref::<std::io::Error>().is_some() {
                    "manifest-unreadable"
                } else {
                    "manifest-unparseable"
                };
                let message = if kind == "manifest-unreadable" {
                    format!(
                        "the manifest cannot be inspected: {e:#} — no further \
                         manifest-dependent checks can be performed"
                    )
                } else {
                    format!(
                        "the manifest cannot be parsed: {e:#} — repair {} by \
                         hand, or restore it from a backup",
                        Manifest::path(prefix).display()
                    )
                };
                let finding = Finding {
                    path: Some(Manifest::path(prefix)),
                    ..Finding::plain(kind, message)
                };
                return Ok(VerifyReport {
                    crates: None,
                    errors: vec![sanitize_finding(finding)],
                    warnings: Vec::new(),
                });
            }
        };
        let (errors, checkable_bins) = verify_entries(prefix, &manifest);
        let names: Vec<String> = manifest.crates.keys().cloned().collect();
        (Some(manifest.crates.len()), errors, names, checkable_bins)
    };
    let mut warnings = Vec::new();
    // A crate in another known prefix is legal by construction — an
    // observation, never a violation; verify does not guess the history.
    let also = prefixes::also_installed(prefix);
    for name in &names {
        for a in also.get(name).map_or(&[][..], Vec::as_slice) {
            warnings.push(Finding {
                path: Some(a.prefix.clone()),
                ..Finding::for_crate(
                    "also-installed",
                    name,
                    format!(
                        "`{name}` is also installed under {} @{} — legal; `cargo lbin remove` \
                         the unwanted side if both were not meant",
                        a.prefix.display(),
                        a.version
                    ),
                )
            });
        }
    }
    // The install-time name scan, bare of the severity word; neutral on
    // purpose — `describe` says which side PATH resolves first, and a
    // same-named executable is worth seeing whichever side wins.
    warnings.extend(shadow_findings(prefix, &all_bins));
    // Cache debris is kept deliberately (forensics), and verify is the
    // one place that lists it. One aggregated finding — forty directories
    // must not bury the one that matters; an unreadable cache contributes
    // silence, not failure.
    if let Ok(cache) = cache_dir()
        && let Ok(stale) = scan_stale_stages(&cache)
        && !stale.is_empty()
    {
        // No hint: the finding is a liveness heuristic, not an
        // unambiguous repair — an orphaned build may still hold the
        // directory, so the message names the tool and keeps the
        // caution instead of promising a command is safe to paste.
        warnings.push(Finding {
            path: Some(cache.clone()),
            ..Finding::plain(
                "stale-stages",
                format!(
                    "{} stage director{} under {} whose owning cargo-lbin process is \
                     gone — possible leftover build debris; inspect, then \
                     `cargo lbin clean --stages` when safe (a PID can be reused, \
                     and an orphaned build may still hold the directory)",
                    stale.len(),
                    if stale.len() == 1 { "y" } else { "ies" },
                    cache.display()
                ),
            )
        });
    }
    // Sanitized once, at the report boundary, for both renderers — and
    // last, so the stale-stage line is covered: everything here travelled
    // through `load_unvalidated` and is untrusted terminal text.
    Ok(VerifyReport {
        crates,
        errors: errors.into_iter().map(sanitize_finding).collect(),
        warnings: warnings.into_iter().map(sanitize_finding).collect(),
    })
}

/// The sanitization boundary, over exactly the field rendered to a
/// human: `message` travelled through `load_unvalidated` and is
/// untrusted terminal text. The data fields stay raw — a smuggled
/// control character in a crate name IS the broken state a `--json`
/// consumer is diagnosing, JSON's serializer escapes it safely, and a
/// laundered copy would hide the very bytes that matter (`hint` is
/// built from validated names and the prefix, or is a constant).
fn sanitize_finding(f: Finding) -> Finding {
    Finding {
        message: text::sanitize(&f.message),
        ..f
    }
}

/// POSIX single-quote shell quoting for the one command lbin invites a
/// human to paste: bare when boring, quoted otherwise, `'\''` for the
/// embedded quote. A hint the docs call pasteable must not be the one
/// thing in the output unsafe to paste.
fn shell_quote(s: &str) -> String {
    let boring = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | ',' | '=' | '@' | '%')
        });
    if boring {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The pasteable spelling of the audited prefix, or `None` when no
/// honest one exists: non-UTF-8 `display()` is lossy, and a control char
/// would be laundered by the sanitize boundary into a command naming a
/// different path — safe to paste, wrong to run. `--prefix=<quoted>`
/// so a dash-leading prefix cannot lex as an option.
fn pasteable_prefix(prefix: &Path) -> Option<String> {
    let s = prefix.to_str()?;
    if s.chars().any(char::is_control) {
        return None;
    }
    Some(format!("--prefix={}", shell_quote(s)))
}

/// The reinstall a disk finding may name — `None` when the prefix has
/// no safe spelling. When present it is true: the audited prefix
/// always, the pinned version (a bare `install` bounces off the pin),
/// `--locked` when the entry carries it. Only the prefix needs
/// quoting: name and version already passed validation — shell-inert
/// alphabets.
fn reinstall_hint(prefix: &Path, name: &str, entry: &Entry) -> Option<String> {
    let prefix_arg = pasteable_prefix(prefix)?;
    let mut hint = format!("cargo lbin install {name}");
    if entry.pinned {
        hint.push('@');
        hint.push_str(&entry.version);
    }
    if entry.locked {
        hint.push_str(" --locked");
    }
    hint.push(' ');
    hint.push_str(&prefix_arg);
    Some(hint)
}

/// One managed binary's disk verdict: the remedy text and the bare
/// `hint` are built together (loadable manifest only — one breach
/// anywhere bounces `install`, so no hint is offered over one), then
/// the claim is checked by `symlink_metadata` — the honest primitive:
/// a symlink must be its own finding, not followed into a wrong one.
fn disk_finding(
    prefix: &Path,
    bin_dir: &Path,
    loadable: bool,
    name: &str,
    entry: &Entry,
    bin: &str,
) -> Option<Finding> {
    use std::os::unix::fs::PermissionsExt;
    let hint = if loadable {
        reinstall_hint(prefix, name, entry)
    } else {
        None
    };
    let remedy = if loadable {
        match hint.clone() {
            Some(hint) => format!(" — reinstall: {hint}"),
            // Loadable manifest, unspellable prefix: the repair is still a
            // reinstall, there is just no command worth pasting.
            None => " — reinstall it (this prefix's name cannot be \
                      spelled as a safe shell command, so none is \
                      offered)"
                .to_owned(),
        }
    } else {
        " — reinstall once the manifest findings above are repaired".to_owned()
    };
    let path = bin_dir.join(bin);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Finding {
            path: Some(path.clone()),
            hint: hint.clone(),
            ..Finding::for_bin(
                "binary-missing",
                name,
                bin,
                format!(
                    "`{name}`: managed binary {} is missing{remedy}",
                    path.display()
                ),
            )
        }),
        // EACCES, EIO, a symlink loop: "missing" would be a lie — the honest
        // finding is that the claim could not be checked.
        Err(e) => Some(Finding {
            path: Some(path.clone()),
            ..Finding::for_bin(
                "binary-uninspectable",
                name,
                bin,
                format!(
                    "`{name}`: managed binary {} cannot be inspected: {e} — the \
                     manifest's claim could not be checked",
                    path.display()
                ),
            )
        }),
        Ok(md) if md.file_type().is_symlink() => Some(Finding {
            path: Some(path.clone()),
            hint: hint.clone(),
            ..Finding::for_bin(
                "binary-is-a-symlink",
                name,
                bin,
                format!(
                    "`{name}`: managed binary {} is a symlink, not the regular \
                     file lbin placed — remove it, then{}",
                    path.display(),
                    remedy.trim_start_matches(" —")
                ),
            )
        }),
        Ok(md) if !md.is_file() => Some(Finding {
            path: Some(path.clone()),
            hint: hint.clone(),
            ..Finding::for_bin(
                "binary-not-a-regular-file",
                name,
                bin,
                format!(
                    "`{name}`: managed binary {} is not a regular file — remove \
                     whatever took its place, then{}",
                    path.display(),
                    remedy.trim_start_matches(" —")
                ),
            )
        }),
        Ok(md) if md.permissions().mode() & 0o111 == 0 => Some(Finding {
            path: Some(path.clone()),
            hint: hint.clone(),
            ..Finding::for_bin(
                "binary-not-executable",
                name,
                bin,
                format!(
                    "`{name}`: managed binary {} is not executable{remedy}",
                    path.display()
                ),
            )
        }),
        Ok(_) => None,
    }
}

/// The hard invariants — `Manifest::validate`'s exact set, one finding
/// per breach where validate first-bails, plus the disk checks. Kept
/// in lockstep on purpose: a check the loader gains must appear here,
/// or verify will bless state `load` refuses. Only a name
/// `validate_bin_name` accepts is ever joined under `bin/` or handed
/// onward — `../../x` must not make a read-only auditor stat outside
/// the prefix.
///
/// Two passes, because remedies depend on the whole: hints exist only
/// when the validate mirror found nothing — one broken entry poisons
/// `Manifest::load` for every command. Validate-class findings name no
/// command ever (it would bounce; a loader-bypassing repair would be a
/// back door) and point at the manifest file itself.
///
/// Returns `(errors, checkable_bins)` — the bins safe for the PATH
/// scan.
///
/// `symlink_metadata` on purpose: lbin places regular files, so a
/// symlink is structural drift even with a healthy target, and a
/// dangling one is a symlink finding, not a lying "missing". Scope
/// stays structural — `pacman -Qk`, not `-Qkk`: no hashes, by the same
/// decision that makes migrate rebuild rather than copy.
fn verify_entries(prefix: &Path, manifest: &Manifest) -> (Vec<Finding>, Vec<String>) {
    let mut errors = Vec::new();
    let mut checkable: Vec<String> = Vec::new();
    let mut claims: BTreeMap<&String, Vec<&String>> = BTreeMap::new();
    let bin_dir = prefix.join("bin");
    let by_hand = format!(
        "lbin's own commands refuse a manifest in this state; repair {} by hand, \
         or restore it from a backup",
        Manifest::path(prefix).display()
    );
    // Pass 1 — the validate mirror, over everything.
    for (name, entry) in &manifest.crates {
        if validate_name(name).is_err() {
            errors.push(Finding::for_crate(
                "invalid-crate-name",
                name,
                format!("`{name}` is not a valid crate name — {by_hand}"),
            ));
        }
        if Version::parse(&entry.version).is_err() {
            errors.push(Finding::for_crate(
                "unparseable-version",
                name,
                format!(
                    "`{name}`: manifest version `{}` is unparseable — {by_hand}",
                    entry.version
                ),
            ));
        }
        if entry.bins.is_empty() {
            errors.push(Finding::for_crate(
                "no-binaries",
                name,
                format!("`{name}`: declares no binaries — a state lbin never writes; {by_hand}"),
            ));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for bin in &entry.bins {
            if validate::validate_bin_name(bin).is_err() {
                // Deliberately not joined under bin/, not claimed, not
                // scanned: the name is the breach, and following it to
                // the disk could lead outside the prefix.
                errors.push(Finding::for_bin(
                    "invalid-bin-name",
                    name,
                    bin,
                    format!("`{name}`: bin entry `{bin}` is not one plain filename — {by_hand}"),
                ));
                continue;
            }
            if !seen.insert(bin.as_str()) {
                errors.push(Finding::for_bin(
                    "duplicate-bin-in-entry",
                    name,
                    bin,
                    format!("`{name}`: binary `{bin}` is listed twice — {by_hand}"),
                ));
                continue;
            }
            claims.entry(bin).or_default().push(name);
            checkable.push(bin.clone());
        }
    }
    // A doubly-claimed name is a state lbin cannot produce, so no command
    // is named: `remove` refuses the manifest, and forced it would delete
    // the file the survivor still claims — hand-edit and verify again.
    for (bin, names) in &claims {
        if names.len() > 1 {
            let owners = names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(" and ");
            errors.push(Finding {
                bin: Some((*bin).clone()),
                ..Finding::plain(
                    "duplicate-bin-claim",
                    format!(
                        "binary `{bin}` is claimed by {owners} — a state lbin never \
                         writes; {by_hand}"
                    ),
                )
            });
        }
    }
    // Pass 2 — the disk audit over names pass 1 let through; hints only
    // when pass 1 found nothing (one breach anywhere bounces `install`).
    let loadable = errors.is_empty();
    for (name, entry) in &manifest.crates {
        // The same gate as pass 1, dedup included: a twice-listed name
        // was flagged once there and gets one disk verdict here.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for bin in &entry.bins {
            if validate::validate_bin_name(bin).is_err() || !seen.insert(bin.as_str()) {
                continue;
            }
            if let Some(finding) = disk_finding(prefix, &bin_dir, loadable, name, entry, bin) {
                errors.push(finding);
            }
        }
    }
    (errors, checkable)
}

/// Stage directories not owned by a live PID — dead-PID and non-PID
/// names alike; stages are named by the owning cargo-lbin's PID and
/// kept on failure. Owner liveness is what the scan measures, and only
/// that: a heuristic, not proof a build is dead — an orphaned cargo may
/// outlive the cargo-lbin that spawned it and still hold the directory.
/// The one definition of ownerless, shared by `verify` and `clean`;
/// error *policy* is the caller's: read errors come back unflattened,
/// verify silences them (read-only, a possibly-wrong warning is worse
/// than none), clean propagates them (a mutating command must not
/// report success over a cache it could not read). `NotFound` is an
/// empty cache for both.
fn scan_stale_stages(cache: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut stale = Vec::new();
    // Two namespaces, two formats — and the namespace is part of the
    // format version. stage/ is 0.12's: a run there is a bare PID and
    // nothing else. stage-v2/ is the leased runs' home: a run there is
    // <pid>-<nonce> and nothing else. A name wearing the other
    // namespace's format is not a run at all, just debris owed no
    // protection story: a bare PID in stage-v2 must not borrow /proc's
    // vote, and a leased name in stage/ — where nothing legally writes
    // one — must not earn a sparing it never signed up for.
    for (namespace, leased) in [("stage", false), (stage::RUN_NAMESPACE, true)] {
        scan_stage_dir(&cache.join(namespace), leased, &mut stale)?;
    }
    stale.sort();
    Ok(stale)
}

/// One namespace's scan, appending to the shared verdict list.
/// `leased` names the one format that is legal here; everything else
/// is judged as debris.
fn scan_stage_dir(dir: &Path, leased: bool, stale: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let run = entry.file_name().to_str().and_then(stage::parse_run_dir);
        let stale_entry = match (leased, run) {
            // The heuristic, for the layout that has nothing better —
            // with both of its known lies left standing.
            (false, Some(stage::StageRun::LegacyPid(pid))) => {
                !Path::new(&format!("/proc/{pid}")).exists()
            }
            // Never stale here yet: liveness for this layout is the
            // lease, and until the probe exists the only safe answer
            // is to spare the run. A wrong "stale" is a deletable lie;
            // a wrong "leave it" costs disk until the next pass.
            (true, Some(stage::StageRun::LeasedRun { .. })) => false,
            // Junk names, and run names wearing the other namespace's
            // format: not a run at all, debris.
            _ => true,
        };
        if stale_entry {
            stale.push(entry.path());
        }
    }
    Ok(())
}

/// The mutating half of the pair `verify` opens: `verify` names the
/// debris read-only, `clean` removes it — through the very same
/// `scan_stale_stages`, so the two can never disagree on what debris is.
/// Old failure logs join in: they are written on every failed build
/// and nothing else ever prunes them. The cache is the user's own —
/// no prefix lock, no sudo; a PID alive on *any* prefix's build is
/// spared by the liveness test itself.
fn cmd_clean(dry_run: bool, stages: bool, logs_older_than_days: Option<u64>) -> Result<()> {
    clean_cache(&cache_dir()?, dry_run, stages, logs_older_than_days)
}

fn clean_cache(
    cache: &Path,
    dry_run: bool,
    stages: bool,
    logs_older_than_days: Option<u64>,
) -> Result<()> {
    // Every removal is opt-in: a mutating command does nothing it was
    // not explicitly asked to do.
    if !stages && logs_older_than_days.is_none() {
        bail!("nothing requested: name --stages and/or --logs-older-than DAYS");
    }
    // The removal set for stages is scan_stale_stages' answer — verify's
    // function, so diagnosis and cleanup cannot drift. But that answer
    // is a liveness heuristic over the *owning* cargo-lbin, not proof
    // of a dead build: an orphaned cargo may survive its parent and
    // still hold the directory — which is why --stages is opt-in and
    // this loop stays behind it. Unlike verify (read-only, silence over
    // a possibly-wrong warning), a mutating command must not report
    // success over a cache it could not read: NotFound is an empty
    // cache, every other read error is an error.
    let stale = if stages {
        scan_stale_stages(cache)
            .with_context(|| format!("reading the stage namespaces under {}", cache.display()))?
    } else {
        Vec::new()
    };
    // Both range checks are errors, never a silently different cutoff:
    // checked_mul so a u64 from the CLI cannot wrap, and checked_sub so
    // a value that multiplies fine but predates the epoch cannot turn
    // "remove logs older than N" into "scan nothing and report success".
    let cutoff = match logs_older_than_days {
        Some(days) => {
            let seconds = days
                .checked_mul(86_400)
                .context("--logs-older-than is too large")?;
            Some(
                std::time::SystemTime::now()
                    .checked_sub(std::time::Duration::from_secs(seconds))
                    .context("--logs-older-than is too large")?,
            )
        }
        None => None,
    };
    let mut old_logs = Vec::new();
    if let Some(cutoff) = cutoff {
        let logs_dir = cache.join("logs");
        let entries = match fs::read_dir(&logs_dir) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", logs_dir.display()));
            }
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry = entry.with_context(|| format!("reading {}", logs_dir.display()))?;
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "log") {
                    continue;
                }
                // An unreadable log is an error, not a skipped removal:
                // the person asked for a retention, and "done" must mean
                // the whole set was considered.
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .with_context(|| format!("inspecting {}", path.display()))?;
                if modified < cutoff {
                    old_logs.push(path);
                }
            }
        }
    }
    old_logs.sort();
    if stale.is_empty() && old_logs.is_empty() {
        println!("nothing to clean");
        return Ok(());
    }
    let verb = if dry_run { "would remove" } else { "removing" };
    let mut failures = 0usize;
    for dir in &stale {
        println!("{verb} ownerless stage {}", dir.display());
        if !dry_run && let Err(e) = fs::remove_dir_all(dir) {
            eprintln!("error: removing {}: {e}", dir.display());
            failures += 1;
        }
    }
    let days = logs_older_than_days.unwrap_or_default();
    for log in &old_logs {
        println!("{verb} log older than {days} day(s): {}", log.display());
        if !dry_run && let Err(e) = fs::remove_file(log) {
            eprintln!("error: removing {}: {e}", log.display());
            failures += 1;
        }
    }
    // Failures preempt the summary: "removed 3 stage(s)" counts
    // candidates, and printing it above "1 removal(s) failed" would be
    // the summary contradicting the verdict. The per-item lines already
    // say what was attempted.
    if failures > 0 {
        bail!("{failures} removal(s) failed");
    }
    let done = if dry_run { "would remove" } else { "removed" };
    let mut parts = Vec::new();
    if stages {
        parts.push(format!("{} ownerless stage(s)", stale.len()));
    }
    if logs_older_than_days.is_some() {
        parts.push(format!("{} old log(s)", old_logs.len()));
    }
    println!("{done} {}", parts.join(", "));
    Ok(())
}

/// The CLI's words over `verify_prefix`'s data: findings to stderr,
/// verdict to stdout, exit status measuring consistency alone —
/// warnings by themselves exit zero.
fn cmd_verify(prefix: &Path, json: bool) -> Result<()> {
    // Lock-wait notice to stderr: a silent multi-minute wait is
    // indistinguishable from a hang.
    let report = verify_prefix(prefix, &mut |m: &str| eprintln!("{m}"))?;
    if json {
        // The document is stdout's only content; findings do not repeat
        // on stderr — the JSON contract, same as list and checkupdate.
        // The exit status stays the text mode's: the summary lives in
        // the bail below, on stderr, where a script's log wants it.
        crate::json::print_verify(prefix, &report)?;
    } else {
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        for w in &report.warnings {
            eprintln!("warning: {w}");
        }
    }
    if report.errors.is_empty() {
        // Zero errors implies a counted manifest (`None` only on the early
        // error return); the fallback keeps that a comment, not a panic.
        let crates = report.crates.unwrap_or(0);
        if !json {
            if report.warnings.is_empty() {
                println!("ok: {crates} managed crate(s)");
            } else {
                println!(
                    "ok: {crates} managed crate(s), {} warning(s)",
                    report.warnings.len()
                );
            }
        }
        return Ok(());
    }
    match report.crates {
        Some(crates) => bail!(
            "{crates} managed crate(s): {} verification error(s), {} warning(s)",
            report.errors.len(),
            report.warnings.len()
        ),
        // The count is unknown, and the verdict line does not guess: a
        // manifest missing its last brace may hold forty entries.
        None => bail!(
            "managed crate count unavailable: {} verification error(s), {} warning(s)",
            report.errors.len(),
            report.warnings.len()
        ),
    }
}

/// The state half of the escalation union: the manifest plus, where
/// escalation is possible, the lock file. Its own question because it
/// is its own write set — a pin flip must not force a handoff over a
/// read-only bin it never touches.
#[cfg(feature = "tui")]
fn state_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    Ok(policy.probe_destination(&prefix.join("share/cargo-lbin"))?
        || (matches!(policy.sudo, privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(prefix)))
}

/// The escalation union for operations placing/removing under bin. The
/// build preflight, in-place removal and captured retirement must
/// never disagree about whether a prefix asks a password; private
/// copies of this `||` would drift.
#[cfg(feature = "tui")]
fn placement_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    // Composed from the canonical probes, not re-spelled: changes reach
    // the TUI preflight through this line.
    Ok(install_needs_privilege(policy, prefix)?
        || (matches!(policy.sudo, privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(prefix)))
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
    if spec.version.is_none() {
        refuse_pinned(&manifest, std::slice::from_ref(&spec.name))?;
    }
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        &spec.name,
        spec.version.as_ref(),
        locked,
        PinPolicy::Infer,
        &mut Frontend::Captured {
            on_line,
            before_placement,
            control,
            checkpoint: None,
        },
    )?;
    Ok(())
}

fn cmd_install(prefix: &Path, crates: &[String], locked: bool) -> Result<()> {
    // Parsed and de-duplicated first: the pin check runs once against the
    // manifest as it is now (see `parse_all`).
    let specs = InstallSpec::parse_all(crates)?;
    let cache = cache_dir()?;
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // A bare reinstall builds the newest version — exactly what a pin
    // forbids; refuse before the first build. Naming a version is a
    // re-pin and is allowed.
    let unversioned: Vec<String> = specs
        .iter()
        .filter(|s| s.version.is_none())
        .map(|s| s.name.clone())
        .collect();
    refuse_pinned(&manifest, &unversioned)?;
    for spec in &specs {
        install_and_commit(
            prefix,
            &cache,
            &mut manifest,
            &spec.name,
            spec.version.as_ref(),
            locked,
            PinPolicy::Infer,
            &mut Frontend::Terminal,
        )?;
    }
    Ok(())
}

/// Error if any of `crates` is pinned: a pin is the more deliberate
/// and durable statement, so it wins; the message says how to change
/// that.
fn refuse_pinned(manifest: &Manifest, crates: &[String]) -> Result<()> {
    let pinned: Vec<&str> = crates
        .iter()
        .filter(|n| manifest.crates.get(n.as_str()).is_some_and(|e| e.pinned))
        .map(String::as_str)
        .collect();
    if !pinned.is_empty() {
        let names = pinned.join(" ");
        bail!(
            "pinned: {} (run `cargo lbin unpin {names}` first)",
            pinned.join(", ")
        );
    }
    Ok(())
}

/// `pin`/`unpin`: one manifest write for the selection. Already in the
/// requested state is reported, not an error; "pinned X" is said only
/// after the store did it.
fn cmd_set_pinned(prefix: &Path, crates: &[String], pinned: bool) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    let targets = select_targets(&manifest, crates)?;
    let verb = if pinned { "pinned" } else { "unpinned" };
    let mut changed: Vec<String> = Vec::new();
    for name in &targets {
        if let Some(entry) = manifest.crates.get_mut(name) {
            if entry.pinned == pinned {
                println!("{name} is already {verb}");
            } else {
                entry.pinned = pinned;
                changed.push(format!("{verb} {name} at {}", entry.version));
            }
        }
    }
    if !changed.is_empty() {
        manifest.store(prefix)?;
        for line in changed {
            println!("{line}");
        }
    }
    Ok(())
}

fn cmd_pinned(prefix: &Path, check: bool, json: bool) -> ExitCode {
    // Everything needed for the listing is resolved before writing
    // anything to stdout, so failures cannot leave a partial listing.
    let outcome = (|| {
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        // Statuses come from one source, never a blend: the recorded report,
        // or a fresh pinned-only query that is deliberately not persisted —
        // a partial snapshot would misinform `list`.
        let report = if check {
            Some(Report::new(
                prefix,
                check_versions(manifest.crates.iter().filter(|(_, e)| e.pinned), || false)?
                    .expect("a `|| false` token never cancels"),
            )?)
        } else {
            match cache_dir().and_then(|cache| Report::load(&cache, prefix)) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("warning: {e:#}");
                    None
                }
            }
        };
        let identity = report::identity(prefix)?;
        Ok::<_, anyhow::Error>((manifest, report, identity))
    })();
    let (manifest, report, identity) = match outcome {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    // The exit code answers one question; a crate the report does not
    // cover contributes nothing — absence of knowledge is not an update.
    let any_outdated = manifest
        .crates
        .iter()
        .filter(|(_, entry)| entry.pinned)
        .any(|(name, entry)| {
            Version::parse(&entry.version)
                .ok()
                .and_then(|current| report.as_ref()?.status_for(name, &current))
                .is_some_and(|status| matches!(status, Status::Outdated(_)))
        });
    if json {
        // The same cross-prefix map list --json uses: the two documents
        // promise entry-for-entry equality on the pinned subset.
        let also = prefixes::also_installed(prefix);
        let output = json::PinnedOutput::build(identity, &manifest, report.as_ref(), &also);
        if let Err(e) = json::print(&output) {
            eprintln!("error: {e:#}");
            return ExitCode::from(EXIT_ERROR);
        }
    } else {
        let mut any_pinned = false;
        for (name, entry) in manifest.crates.iter().filter(|(_, e)| e.pinned) {
            any_pinned = true;
            let locked = if entry.locked { " [locked]" } else { "" };
            // Three states, silent on the third: newer known, known current, or
            // not covered — nothing printed, because nothing is known.
            let status = Version::parse(&entry.version)
                .ok()
                .and_then(|current| report.as_ref()?.status_for(name, &current))
                .map(|status| match status {
                    Status::Outdated(latest) => format!(" -> {latest}"),
                    Status::UpToDate => " (up to date)".to_owned(),
                })
                .unwrap_or_default();
            println!("{name} {}{locked}{status}", entry.version);
        }
        if !any_pinned {
            println!("no pinned crates under {}", prefix.display());
        }
        // Freshness on stderr, as in `list`: it is for the reader, not a
        // parser. Under `--check` the statuses are from this very moment
        // and the line would only state the obvious.
        if !check {
            if let Some(r) = &report {
                eprintln!("update check: {}", report::describe_age(r.age()));
            } else {
                eprintln!(
                    "no update check recorded; run `cargo lbin checkupdate` or use `--check`"
                );
            }
        }
    }
    if any_outdated {
        ExitCode::from(EXIT_UPDATES)
    } else {
        ExitCode::from(EXIT_NO_UPDATES)
    }
}

fn cmd_remove(prefix: &Path, crates: &[String]) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let policy = privileged::Policy::for_prefix(prefix);
    let mut manifest = Manifest::load(prefix)?;
    let bin_dir = prefix.join("bin");
    let mut removed_any = false;
    for name in crates {
        let Some(entry) = manifest.crates.remove(name) else {
            eprintln!("warning: `{name}` is not in the manifest, skipping");
            continue;
        };
        let paths: Vec<PathBuf> = entry.bins.iter().map(|b| bin_dir.join(b)).collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        privileged::remove_files(policy, &refs)?;
        // Commit per removal: a later failure in the batch must not undo
        // the bookkeeping for what is already gone from disk.
        manifest.store(prefix)?;
        println!("removed {name} ({})", entry.bins.join(", "));
        removed_any = true;
    }
    if !removed_any {
        bail!("nothing to remove");
    }
    Ok(())
}

fn cmd_list(prefix: &Path, json: bool) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
    let manifest = Manifest::load(prefix)?;
    // What the other known prefixes carry — lockless by design; see the
    // prefixes module for why an annotation must never wait on a
    // foreign lock.
    let also = prefixes::also_installed(prefix);
    // Purely local: the last `checkupdate` result, if any. An unreadable
    // report is a warning — the listing itself does not depend on it.
    let report = match cache_dir().and_then(|cache| Report::load(&cache, prefix)) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("warning: {e:#}");
            None
        }
    };
    if json {
        // A document either way: an empty prefix is `"crates": []`, not a
        // sentence a script would have to recognize.
        let output =
            json::ListOutput::build(report::identity(prefix)?, &manifest, report.as_ref(), &also);
        return json::print(&output);
    }
    if manifest.crates.is_empty() {
        println!("no crates installed under {}", prefix.display());
        return Ok(());
    }
    for (name, entry) in &manifest.crates {
        let locked = if entry.locked { " [locked]" } else { "" };
        let pinned = if entry.pinned { " [pinned]" } else { "" };
        // Three states; the last stays silent rather than masquerade: not
        // covered by the last check means nothing is known.
        let status = Version::parse(&entry.version)
            .ok()
            .and_then(|current| report.as_ref()?.status_for(name, &current))
            .map(|status| match status {
                Status::Outdated(latest) => format!(" -> {latest}"),
                Status::UpToDate => " (up to date)".to_owned(),
            })
            .unwrap_or_default();
        let also = prefixes::describe_for(&also, name);
        println!(
            "{name} {}{locked}{pinned}{also} ({}){status}",
            entry.version,
            entry.bins.join(", ")
        );
    }
    // Status goes to stderr: it is for the person reading the terminal,
    // not for whatever may be parsing stdout.
    if let Some(r) = report {
        eprintln!("update check: {}", report::describe_age(r.age()));
    } else {
        eprintln!("no update check recorded; run `cargo lbin checkupdate`");
    }
    Ok(())
}

/// Resolve an explicit crate selection against the manifest. Every name must
/// be installed; all unknown names are reported in one error so the user
/// fixes the command once, not once per typo. Duplicates collapse.
fn select_targets(manifest: &Manifest, crates: &[String]) -> Result<BTreeSet<String>> {
    let unknown: Vec<&str> = crates
        .iter()
        .filter(|n| !manifest.crates.contains_key(n.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        bail!("not installed: {}", unknown.join(", "));
    }
    Ok(crates.iter().cloned().collect())
}

/// Query the index for the given entries and record every answer;
/// network errors abort rather than under-report. Nothing relevant
/// offered counts as current.
/// One index request per crate, `should_cancel` consulted between them:
/// `Ok(None)` is a cancelled run — an answer, distinct from a failure,
/// and the check happens before each request so a cancel never pays for
/// one more round-trip than the one already in flight. The CLI passes
/// `|| false`; the TUI passes its cancel token.
fn check_versions<'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a Entry)>,
    should_cancel: impl Fn() -> bool,
) -> Result<Option<Vec<Checked>>> {
    let mut checked = Vec::new();
    for (name, entry) in entries {
        if should_cancel() {
            return Ok(None);
        }
        let current = Version::parse(&entry.version)
            .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
        let versions = index::published_versions(name)?;
        let latest = index::latest_relevant(&versions, &current)
            .filter(|latest| *latest > current)
            .unwrap_or_else(|| current.clone());
        checked.push(Checked {
            name: name.clone(),
            current,
            latest,
        });
    }
    Ok(Some(checked))
}

/// A release as `info` prints it: the version, flagged if yanked.
fn release_label(release: &index::Release) -> String {
    if release.yanked {
        format!("{} [yanked]", release.version)
    } else {
        release.version.to_string()
    }
}

/// One crate's `info` block, two sources: history lines may name a
/// yanked release (flagged); the `installed` verdict uses
/// `checkupdate`'s rules and must never contradict it.
fn describe_info(name: &str, releases: &[index::Release], installed: Option<&Entry>) -> String {
    // Formatting into a String cannot fail; the `let _ =` discards the
    // Result the macros return for the general `fmt::Write` case.
    use std::fmt::Write as _;
    let summary = index::summarize(releases);
    let mut out = format!("{name}\n");
    if let Some(stable) = &summary.latest_stable {
        let _ = writeln!(out, "  latest:      {}", release_label(stable));
    } else {
        out.push_str("  latest:      (no stable release)\n");
    }
    if let Some(pre) = &summary.latest_pre {
        let _ = writeln!(out, "  pre-release: {}", release_label(pre));
    }
    let _ = write!(out, "  releases:    {}", summary.total);
    if summary.yanked > 0 {
        let _ = write!(out, " ({} yanked)", summary.yanked);
    }
    out.push('\n');
    let Some(entry) = installed else {
        out.push_str("  installed:   no\n");
        return out;
    };
    let _ = write!(out, "  installed:   {}", entry.version);
    let live: Vec<Version> = releases
        .iter()
        .filter(|r| !r.yanked)
        .map(|r| r.version.clone())
        .collect();
    if live.is_empty() {
        out.push_str(" (no non-yanked releases)\n");
        return out;
    }
    let newer = Version::parse(&entry.version).ok().and_then(|current| {
        index::latest_relevant(&live, &current).filter(|latest| *latest > current)
    });
    if let Some(latest) = newer {
        let _ = writeln!(out, " (update available: {latest})");
    } else {
        out.push_str(" (up to date)\n");
    }
    out
}

/// Read-only and network-bound like `checkupdate`: manifest snapshot
/// under a shared lock, queries unlocked; each name independent, exit
/// code says whether everything was found.
fn cmd_info(prefix: &Path, crates: &[String]) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let manifest = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    // Input order, first occurrence wins: answers read in the order asked
    // (`update` sorts — a build sequence should not depend on typing).
    let mut names: Vec<&str> = Vec::new();
    for name in crates {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    let mut failures: Vec<anyhow::Error> = Vec::new();
    let mut shown = 0usize;
    for name in &names {
        match index::releases(name) {
            Ok(Some(releases)) => {
                if shown > 0 {
                    println!();
                }
                print!(
                    "{}",
                    describe_info(name, &releases, manifest.crates.get(*name))
                );
                shown += 1;
            }
            // `info` is exact by design; the fuzzy question lives one
            // command over, and a miss is the moment to say so.
            Ok(None) => failures.push(anyhow::anyhow!(
                "{}; try `cargo lbin search {name}`",
                index::not_found(name)
            )),
            Err(e) => failures.push(e),
        }
    }
    // Errors are reported after all results, so stdout stays contiguous
    // and stderr is not interleaved with it. A single lookup that failed
    // is simply the command's error — one line, no summary restating it.
    if failures.is_empty() {
        return Ok(());
    }
    if names.len() == 1 {
        return Err(failures.remove(0));
    }
    for e in &failures {
        eprintln!("error: {e:#}");
    }
    bail!("{} of {} lookups failed", failures.len(), names.len())
}

/// Longest description `search` prints before cutting; a preview line,
/// not a README.
const SEARCH_DESCRIPTION_WIDTH: usize = 72;

/// Lay out search hits as aligned rows. `installed` maps a crate name to
/// its installed version; matching hits get a `*` and the version.
fn format_search_hits(hits: &[api::Hit], installed: &BTreeMap<String, String>) -> String {
    use std::fmt::Write as _;
    let name_w = hits.iter().map(|h| h.name.len()).max().unwrap_or(0);
    let version_w = hits.iter().map(|h| h.version.len()).max().unwrap_or(0);
    let mut out = String::new();
    for hit in hits {
        let mark = if installed.contains_key(&hit.name) {
            '*'
        } else {
            ' '
        };
        let mut description: String = hit
            .description
            .chars()
            .take(SEARCH_DESCRIPTION_WIDTH)
            .collect();
        if hit.description.chars().count() > SEARCH_DESCRIPTION_WIDTH {
            description.push('…');
        }
        let _ = write!(
            out,
            "{mark} {:<name_w$}  {:<version_w$}  {description}",
            hit.name, hit.version
        );
        if let Some(have) = installed.get(&hit.name) {
            let _ = write!(out, "  [installed {have}]");
        }
        out.push('\n');
    }
    out
}

/// Keyword search for choosing a name; one API request, then the
/// manifest (shared lock, briefly) marks hits already installed. No
/// results is an answer, not an error.
fn cmd_search(prefix: &Path, query: &[String], limit: u8) -> Result<()> {
    let query = query.join(" ");
    let hits = api::search(&query, usize::from(limit))?;
    if hits.is_empty() {
        println!("no crates match `{query}`");
        return Ok(());
    }
    let installed: BTreeMap<String, String> = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
            .crates
            .into_iter()
            .map(|(name, entry)| (name, entry.version))
            .collect()
    };
    print!("{}", format_search_hits(&hits, &installed));
    if hits.iter().any(|h| installed.contains_key(&h.name)) {
        println!("* installed under {}", prefix.display());
    }
    Ok(())
}

/// One roff page per command, from the same clap definitions `--help`
/// prints — the man page and the help text cannot drift. Pages land as
/// files (not stdout): there are many, and a packager's %install wants
/// paths, not a stream to split.
fn cmd_man(dir: &Path) -> Result<()> {
    use clap::CommandFactory;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // build() finalizes and propagates the global args (--prefix,
    // --user) into the subcommands, so each page documents the flags
    // the command actually accepts.
    let mut cmd = Cli::command();
    cmd.build();
    let write = |name: &str, cmd: clap::Command| -> Result<()> {
        let mut buf = Vec::new();
        // `.title()` names the page header; the SYNOPSIS comes from the
        // command's bin_name, which the caller sets to the real
        // invocation — mangen falls back to the bare subcommand name
        // otherwise, and a SYNOPSIS saying `install [OPTIONS]` would
        // document a command nobody can type.
        clap_mangen::Man::new(cmd)
            .title(name.to_uppercase())
            .render(&mut buf)
            .with_context(|| format!("rendering man page for {name}"))?;
        let path = dir.join(format!("{name}.1"));
        fs::write(&path, buf).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    };
    write("cargo-lbin", cmd.clone().bin_name("cargo-lbin"))?;
    for sub in cmd.get_subcommands() {
        // Skip clap's implicit help pseudo-command; every real
        // subcommand gets its page.
        if sub.get_name() == "help" {
            continue;
        }
        write(
            &format!("cargo-lbin-{}", sub.get_name()),
            sub.clone()
                .bin_name(format!("cargo-lbin {}", sub.get_name())),
        )?;
    }
    Ok(())
}

/// Print a static completion script from the same clap definitions
/// `--help` and `man` render — the third surface that cannot drift.
fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "cargo-lbin", &mut std::io::stdout());
}

/// How many older versions `downgrade` lists. Beyond that, the user
/// knows the number they want and `install NAME@VERSION` takes it.
const DOWNGRADE_CHOICES: usize = 10;

/// Interpret the version-prompt answer: a 1-based number, or nothing
/// to abort. Anything else errors — one question, one answer.
fn parse_choice(answer: &str, count: usize) -> Result<Option<usize>> {
    let answer = answer.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let n: usize = answer
        .parse()
        .with_context(|| format!("`{answer}` is not a number between 1 and {count}"))?;
    if n == 0 || n > count {
        bail!("`{n}` is not between 1 and {count}");
    }
    Ok(Some(n - 1))
}

/// Offer older versions and install the chosen one, pinned; same
/// release-relevance policy as `update`. Interactive by design — no
/// `--yes`; scripts have `install NAME@VERSION`.
fn cmd_downgrade(prefix: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    // Snapshot under a shared lock; query and prompt run unlocked. The
    // install re-checks under the exclusive lock: the chosen version will
    // land, but landing must still be a downgrade.
    let entry = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
            .crates
            .remove(name)
            .with_context(|| format!("`{name}` is not installed under {}", prefix.display()))?
    };
    let current = Version::parse(&entry.version)
        .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
    let releases = index::releases(name)?.ok_or_else(|| index::not_found(name))?;
    let candidates = index::downgrade_candidates(&releases, &current);
    if candidates.is_empty() {
        println!("{name} {current} is installed; no older version to go back to");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "downgrade asks which version to install; without a terminal, use `cargo lbin install {name}@VERSION`"
        );
    }
    println!("{name} {current} is installed; older versions on crates.io:");
    let shown = &candidates[..candidates.len().min(DOWNGRADE_CHOICES)];
    for (i, v) in shown.iter().enumerate() {
        println!("  {}) {v}", i + 1);
    }
    if candidates.len() > shown.len() {
        println!(
            "  and {} older; use `cargo lbin install {name}@VERSION` for one of those",
            candidates.len() - shown.len()
        );
    }
    print!(
        "select a version to install (1-{}), or Enter/q to abort: ",
        shown.len()
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let Some(pick) = parse_choice(&answer, shown.len())? else {
        println!("aborted");
        return Ok(());
    };
    let version = &shown[pick];
    let cache = cache_dir()?;
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // The choice was relative to `current`: removed meanwhile would
    // resurrect, changed meanwhile could upgrade under a command called
    // downgrade — the newer statement about the prefix wins.
    let fresh = manifest.crates.get(name).with_context(|| {
        format!("`{name}` was removed while a version was being chosen; run the command again")
    })?;
    let fresh_version = Version::parse(&fresh.version)
        .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
    if fresh_version != current {
        bail!(
            "`{name}` changed from {current} to {fresh_version} while a version was being chosen; \
             run the command again"
        );
    }
    let locked = fresh.locked;
    println!("downgrading {name} {current} -> {version}");
    // The chosen version is installed and pinned by the same path as
    // `install NAME@VERSION`.
    install_and_commit(
        prefix,
        &cache,
        &mut manifest,
        name,
        Some(version),
        locked,
        PinPolicy::Infer,
        &mut Frontend::Terminal,
    )?;
    Ok(())
}

fn cmd_checkupdate(prefix: &Path, json: bool) -> ExitCode {
    // Shared lock covers only the snapshot; index queries run unlocked so
    // a slow crates.io cannot starve writers.
    let outcome = (|| {
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        Report::new(
            prefix,
            check_versions(&manifest.crates, || false)?.expect("a `|| false` token never cancels"),
        )
    })();
    match outcome {
        Ok(report) => {
            // Persist the snapshot before reporting; a failed write is a warning
            // — the check succeeded and the exit code must say so.
            if let Err(e) = cache_dir().and_then(|cache| report.store(&cache)) {
                eprintln!("warning: could not save update report: {e:#}");
            }
            let any = report.crates.iter().any(Checked::is_outdated);
            if json {
                if let Err(e) = json::print(&json::CheckOutput::from_report(&report)) {
                    eprintln!("error: {e:#}");
                    return ExitCode::from(EXIT_ERROR);
                }
            } else {
                for o in report.crates.iter().filter(|c| c.is_outdated()) {
                    println!("{} {} -> {}", o.name, o.current, o.latest);
                }
            }
            if any {
                ExitCode::from(EXIT_UPDATES)
            } else {
                ExitCode::from(EXIT_NO_UPDATES)
            }
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn cmd_update(prefix: &Path, crates: &[String], all: bool, yes: bool) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let cache = cache_dir()?;
    // Phase 1: read-only snapshot under a shared lock, released before
    // the prompt — an unanswered "proceed?" must not block the prefix.
    // Phase 2 reloads and re-verifies anyway.
    let snapshot = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    // Selection validated against the snapshot before any network: typos
    // fail in milliseconds. With `--all`, pinned crates are not part of
    // the plan — a pinned crate's failed lookup must not stop every
    // unpinned update; skipped ones are named.
    let skipped_pinned: Vec<&str> = if all {
        snapshot
            .crates
            .iter()
            .filter(|(_, entry)| entry.pinned)
            .map(|(name, _)| name.as_str())
            .collect()
    } else {
        Vec::new()
    };
    let targets: BTreeSet<String> = if all {
        snapshot
            .crates
            .iter()
            .filter(|(_, entry)| !entry.pinned)
            .map(|(name, _)| name.clone())
            .collect()
    } else {
        let targets = select_targets(&snapshot, crates)?;
        refuse_pinned(&snapshot, crates)?;
        targets
    };
    for name in &skipped_pinned {
        println!(
            "{name} {} [pinned, skipped]",
            snapshot.crates[*name].version
        );
    }
    let outdated: Vec<Checked> = check_versions(
        snapshot
            .crates
            .iter()
            .filter(|(name, _)| targets.contains(name.as_str())),
        || false,
    )?
    .expect("a `|| false` token never cancels")
    .into_iter()
    .filter(Checked::is_outdated)
    .collect();
    // Explicitly named crates that need nothing get a line each: the user
    // asked about them by name and should not have to infer "up to date"
    // from silence.
    if !all {
        for name in &targets {
            if !outdated.iter().any(|o| &o.name == name) {
                let version = snapshot.crates[name].version.as_str();
                println!("{name} {version} is up to date");
            }
        }
    }
    if outdated.is_empty() {
        if all && skipped_pinned.is_empty() {
            println!("everything is up to date");
        } else if all {
            println!(
                "nothing to update; {} pinned crate(s) skipped",
                skipped_pinned.len()
            );
        }
        return Ok(());
    }
    for o in &outdated {
        println!("{} {} -> {}", o.name, o.current, o.latest);
    }
    if !yes && !confirm("proceed with update?")? {
        println!("aborted");
        return Ok(());
    }
    // Phase 2, in its own function: exclusive lock, fresh manifest, one
    // crate at a time.
    apply_updates(prefix, &cache, &outdated)
}

/// The mutating half of `update`: exclusive lock, fresh manifest, each
/// planned update re-verified — whatever no longer matches the
/// snapshot is skipped with a note, never acted on blindly.
fn apply_updates(prefix: &Path, cache: &Path, outdated: &[Checked]) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // Each crate is its own unit: report, roll back, move on — the crates
    // are independent, and undoing finished work buys no consistency.
    let total = outdated.len();
    let mut updated = 0usize;
    let mut skipped: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for (i, o) in outdated.iter().enumerate() {
        println!("[{}/{total}] {}", i + 1, o.name);
        // Pinned since confirmation counts as changed state too: the pin
        // is newer than the plan, and the newer statement wins.
        match manifest.crates.get(&o.name) {
            Some(entry) if entry.version == o.current.to_string() && !entry.pinned => {
                let locked = entry.locked;
                // The build may land newer than `latest` if a release arrives
                // mid-update; the manifest records what was built.
                match install_and_commit(
                    prefix,
                    cache,
                    &mut manifest,
                    &o.name,
                    None,
                    locked,
                    PinPolicy::Infer,
                    &mut Frontend::Terminal,
                ) {
                    Ok(_) => updated += 1,
                    Err(err) => {
                        eprintln!("error: updating `{}` failed: {err:#}", o.name);
                        failed.push(&o.name);
                    }
                }
            }
            _ => {
                eprintln!(
                    "skipping `{}`: state changed since the update was confirmed",
                    o.name
                );
                skipped.push(&o.name);
            }
        }
    }
    println!("updated {updated} of {total}");
    // Asked for `total` updates; any shortfall exits non-zero — the user
    // reads the exit code, and "not done" is the fact.
    let mut shortfall = Vec::new();
    if !failed.is_empty() {
        shortfall.push(format!("failed: {}", failed.join(", ")));
    }
    if !skipped.is_empty() {
        shortfall.push(format!("skipped: {}", skipped.join(", ")));
    }
    if !shortfall.is_empty() {
        bail!(
            "{} of {total} updates not applied ({})",
            total - updated,
            shortfall.join("; ")
        );
    }
    Ok(())
}

/// The pin bit a committed entry ends up with. `install`'s inference
/// is its own contract, not every caller's: migrate names an exact
/// version only *for a pinned source* — the pin is what makes the
/// version intent — and its pin promise either way is "the bit travels
/// unchanged". Part of the one manifest commit — a second corrective
/// write would open exactly the stranded-state window the snapshot
/// machinery exists to prevent.
#[derive(Clone, Copy)]
enum PinPolicy {
    /// `install`'s inference: an exact-version request pins; otherwise a
    /// pin already present is carried over — never dropped by a rewrite.
    Infer,
    /// The caller states the final bit outright.
    Exactly(bool),
}

/// Everything `migrate` preserves about a source entry, captured under
/// the snapshot lock. A guard on the *source*, not a description of
/// the destination: the version here is what the source must still be
/// for the retirement to proceed, while the destination follows the
/// pin — exactly this version when pinned, the latest when not.
/// Exhaustive destructuring on purpose: a new field fails compilation
/// and forces a decision — no silent guesses about state this command
/// is about to delete.
pub(crate) struct MigrationSnapshot {
    version: Version,
    bins: Vec<String>,
    locked: bool,
    pinned: bool,
}

impl MigrationSnapshot {
    /// A snapshot from parts a frontend already holds — how the confirmed
    /// plan is *frozen*: built at the keypress, handed to the worker
    /// unchanged; a fresh snapshot after the `y` could bless a version the
    /// person never saw. Exhaustive for `capture`'s reason.
    #[cfg(feature = "tui")]
    pub(crate) fn from_parts(
        name: &str,
        version: &str,
        bins: Vec<String>,
        locked: bool,
        pinned: bool,
    ) -> Result<Self> {
        Ok(Self {
            version: Version::parse(version)
                .with_context(|| format!("`{name}` has an unparseable version `{version}`"))?,
            bins,
            locked,
            pinned,
        })
    }

    fn capture(name: &str, entry: &Entry) -> Result<Self> {
        let Entry {
            version,
            bins,
            locked,
            pinned,
        } = entry;
        Ok(Self {
            version: Version::parse(version)
                .with_context(|| format!("`{name}` has an unparseable version `{version}`"))?,
            bins: bins.clone(),
            locked: *locked,
            pinned: *pinned,
        })
    }

    /// Whether `entry` is still the entry this snapshot was taken from —
    /// the question both revalidations ask. The same exhaustive
    /// destructuring as `capture`, for the same reason.
    fn still_matches(&self, entry: &Entry) -> bool {
        let Entry {
            version,
            bins,
            locked,
            pinned,
        } = entry;
        Version::parse(version).is_ok_and(|v| v == self.version)
            && *bins == self.bins
            && *locked == self.locked
            && *pinned == self.pinned
    }
}

/// How one migration ended, short of an error — which ends at the
/// destination commit. Past that point everything is a partial
/// success: hiding the rebuilt install would invite a blind re-run.
#[derive(Debug)]
pub(crate) enum MigrateOutcome {
    /// Rebuilt at the destination; `version` is what the destination
    /// actually committed — the stage's verified truth, so no caller
    /// reports the plan where it means the result. `already_retired`
    /// says whether the source entry was found already gone (retired
    /// during the build — the goal state, by other hands).
    Moved {
        already_retired: bool,
        version: Version,
    },
    /// Destination committed, source not retired — changed under the plan
    /// or the retirement failed; the reason says which.
    Incomplete(String),
}
/// Phase 0 of `migrate`: the read-only plan, snapshot under a shared
/// lock released before the prompt (`update`'s rule). Everything is
/// revalidated under real locks later; `None` = aborted at the prompt.
fn plan_migration(
    prefix: &Path,
    to: &Path,
    crates: &[String],
    all: bool,
    yes: bool,
) -> Result<Option<Vec<(String, MigrationSnapshot)>>> {
    let snapshot_manifest = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    let names: BTreeSet<String> = if all {
        snapshot_manifest.crates.keys().cloned().collect()
    } else {
        select_targets(&snapshot_manifest, crates)?
    };
    if names.is_empty() {
        bail!("nothing to migrate");
    }
    let mut snapshots: Vec<(String, MigrationSnapshot)> = Vec::with_capacity(names.len());
    for name in names {
        let snap = MigrationSnapshot::capture(&name, &snapshot_manifest.crates[&name])?;
        snapshots.push((name, snap));
    }
    for (name, snap) in &snapshots {
        // The plan states what each crate *gets*, not only what it is:
        // the confirmation is the contract, and "1.2.3 -> latest" for an
        // unpinned crate is exactly the difference a person may want to
        // veto. The snapshot's version is still printed for both — it is
        // the source state the revalidations will hold the migration to.
        println!(
            "{name} {}: {} -> {} [{}]",
            snap.version,
            prefix.display(),
            to.display(),
            if snap.pinned {
                "pinned; the exact version is rebuilt"
            } else {
                "unpinned; the latest version is installed"
            }
        );
    }
    if !yes && !confirm("proceed with migration?")? {
        println!("aborted");
        return Ok(None);
    }
    Ok(Some(snapshots))
}

fn cmd_migrate(prefix: &Path, to: &Path, crates: &[String], all: bool, yes: bool) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    // `Path` equality is component-wise; symlinked spellings are the
    // person's to know — the prefixes module's lexical stance.
    if prefix == to {
        bail!(
            "source and destination are the same prefix ({})",
            prefix.display()
        );
    }
    let cache = cache_dir()?;
    let Some(snapshots) = plan_migration(prefix, to, crates, all, yes)? else {
        return Ok(());
    };
    // Each crate its own unit, `update --all`'s batch rule: report and
    // move on.
    let total = snapshots.len();
    let mut moved = 0usize;
    let mut incomplete: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for (i, (name, snap)) in snapshots.iter().enumerate() {
        println!("[{}/{total}] {name}", i + 1);
        match migrate_one(
            prefix,
            to,
            &cache,
            name,
            snap,
            &mut MigrateFrontend::Terminal,
        ) {
            Ok(MigrateOutcome::Moved {
                already_retired,
                version,
            }) => {
                // The words live with the caller: migrate_one reports
                // data, the CLI speaks CLI — and speaks the version the
                // destination committed, never the plan's.
                if already_retired {
                    println!(
                        "migrated {name} {version}: already retired from {}",
                        prefix.display()
                    );
                } else {
                    println!(
                        "migrated {name} {version}: retired from {}",
                        prefix.display()
                    );
                }
                moved += 1;
            }
            Ok(MigrateOutcome::Incomplete(reason)) => {
                eprintln!("warning: incomplete migration: {reason}");
                incomplete.push(name);
            }
            Err(err) => {
                eprintln!("error: migrating `{name}` failed: {err:#}");
                failed.push(name);
            }
        }
    }
    println!("migrated {moved} of {total}");
    // The command was asked for `total` migrations; anything short of
    // that exits non-zero — an incomplete migration is a *safe*
    // shortfall (the destination stands), but a shortfall: the source
    // the person asked to retire is still there, in whole or in part.
    let mut shortfall = Vec::new();
    if !failed.is_empty() {
        shortfall.push(format!(
            "failed before the destination committed: {}",
            failed.join(", ")
        ));
    }
    if !incomplete.is_empty() {
        shortfall.push(format!(
            "destination committed, source not retired: {}",
            incomplete.join(", ")
        ));
    }
    if !shortfall.is_empty() {
        bail!(
            "{} of {total} migrations not completed ({})",
            total - moved,
            shortfall.join("; ")
        );
    }
    Ok(())
}

/// The shapes a migration can speak through — a request, not a
/// `Frontend`: `migrate_one` owns its early-revalidation checkpoint
/// (it borrows the snapshot and the source path), so the caller names
/// the flavor and `migrate_one` assembles the real frontend around its
/// own checkpoint. Terminal is the CLI, unchanged; Captured is the TUI
/// driving a migration through the same Build panel, cancel door and
/// sudo roundtrip as an install.
pub(crate) enum MigrateFrontend<'a> {
    Terminal,
    /// Keeps the lifetime honest when the tui feature is off — the same
    /// phantom `Frontend::Never` carries, for the same reason.
    #[cfg(not(feature = "tui"))]
    #[allow(dead_code)]
    Never(std::marker::PhantomData<&'a ()>),
    #[cfg(feature = "tui")]
    Captured {
        on_line: &'a mut dyn FnMut(LineKind, &str),
        before_placement: &'a mut dyn FnMut(&Path) -> Result<()>,
        control: &'a BuildControl,
    },
}

/// Phase A wholesale: the destination's exclusive lock (through the
/// flavor's policy and notice path), the no-force refusal, the frozen
/// plan's checkpoint composed behind the placement door, and the
/// rebuild itself. Everything in here may still fail as a plain error:
/// nothing has committed until this returns.
fn rebuild_at_destination(
    source: &Path,
    dest: &Path,
    cache: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<Version> {
    let _dest_lock = match frontend {
        MigrateFrontend::Terminal => StateLock::acquire(dest, &Mode::Exclusive)?,
        #[cfg(not(feature = "tui"))]
        MigrateFrontend::Never(_) => unreachable!(),
        #[cfg(feature = "tui")]
        MigrateFrontend::Captured { on_line, .. } => StateLock::acquire_with(
            dest,
            &Mode::Exclusive,
            privileged::Policy::for_prefix(dest).screen_owned(),
            &mut |l| on_line(LineKind::Notice, l),
        )?,
    };
    let mut dest_manifest = Manifest::load(dest)?;
    if let Some(existing) = dest_manifest.crates.get(name) {
        bail!(
            "`{name}` is already installed under {} at {}; migrate refuses to overwrite \
             it with the migrating install — remove one side first (no --force by design)",
            dest.display(),
            existing.version
        );
    }
    // The early revalidation, at the placement door: nonblocking and
    // advisory — abort while aborting is free (the stage is the only
    // casualty); the authoritative pass runs in phase B under the real
    // lock. Notices stay silent in both shapes, as `try_acquire_with`
    // documents.
    let checkpoint_policy = match frontend {
        MigrateFrontend::Terminal => privileged::Policy::for_prefix(source),
        #[cfg(not(feature = "tui"))]
        MigrateFrontend::Never(_) => unreachable!(),
        #[cfg(feature = "tui")]
        MigrateFrontend::Captured { .. } => privileged::Policy::for_prefix(source).screen_owned(),
    };
    let mut checkpoint = || -> Result<()> {
        let advisory =
            StateLock::try_acquire_with(source, &Mode::Shared, checkpoint_policy, &mut |_| {})?;
        let Some(_lock) = advisory else {
            bail!(
                "source prefix {} is busy; aborting before the destination commits",
                source.display()
            );
        };
        let current = Manifest::load(source)?;
        match current.crates.get(name) {
            Some(entry) if snap.still_matches(entry) => Ok(()),
            Some(_) => bail!(
                "`{name}` changed under {} since the plan; aborting before the \
                 destination commits",
                source.display()
            ),
            None => bail!(
                "`{name}` is no longer installed under {}; aborting before the \
                 destination commits",
                source.display()
            ),
        }
    };
    // The version request follows the pin — the pin is what makes a
    // version part of the person's intent: pinned rebuilds exactly the
    // pinned version, unpinned asks for nothing and gets what cargo
    // resolves as latest. Migrate preserves policy, not necessarily
    // version; the snapshot's version still guards the *source* in both
    // revalidations.
    // `Exactly(snap.pinned)`: the bit travels unchanged inside the same
    // manifest commit — a second corrective store would open a window in
    // which the destination is right and the intent is wrong.
    let version = snap.pinned.then_some(&snap.version);
    let installed = match frontend {
        MigrateFrontend::Terminal => install_and_commit(
            dest,
            cache,
            &mut dest_manifest,
            name,
            version,
            snap.locked,
            PinPolicy::Exactly(snap.pinned),
            &mut Frontend::Checkpointed {
                checkpoint: &mut checkpoint,
            },
        )?,
        #[cfg(not(feature = "tui"))]
        MigrateFrontend::Never(_) => unreachable!(),
        #[cfg(feature = "tui")]
        MigrateFrontend::Captured {
            on_line,
            before_placement,
            control,
        } => install_and_commit(
            dest,
            cache,
            &mut dest_manifest,
            name,
            version,
            snap.locked,
            PinPolicy::Exactly(snap.pinned),
            &mut Frontend::Captured {
                on_line: &mut **on_line,
                before_placement: &mut **before_placement,
                control,
                checkpoint: Some(&mut checkpoint),
            },
        )?,
    };
    Ok(installed)
}

/// One migration, sequential by design: destination first, source
/// second. Migrate never *waits* on one prefix while holding the
/// other's lock and never holds two exclusive locks (the only overlap
/// is the nonblocking advisory probe) — two exclusive locks would park
/// both prefixes behind a build and need a deadlock order nothing
/// enforces. The price is one honest window: between destination
/// commit and source retirement the crate exists in both prefixes —
/// one `remove` fixes it, the listing annotates the known pair, and
/// for custom prefixes the command's own message is the durable
/// record. The order is load-bearing: no failure or crash ever leaves
/// the person without one complete installation (a crash
/// mid-retirement can leave a partial source; `remove` is `rm -f` and
/// cleans the remainder).
fn migrate_one(
    source: &Path,
    dest: &Path,
    cache: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<MigrateOutcome> {
    let installed = rebuild_at_destination(source, dest, cache, name, snap, frontend)?;

    // Phase B: the source, under its exclusive lock. From here nothing
    // may surface as a plain error — the destination has committed, and
    // every failure is an *incomplete migration* whose message leads with
    // that (a bare "failed" invites a re-run the already-installed
    // refusal bounces). Outcomes carry data; the caller owns the words —
    // the privilege probes included: failing while *asking* is no
    // exception. The reason names the version the destination committed:
    // two installations stand, and "what is actually over there" is the
    // fact the person cleans up by.
    match retire_with_frontend(source, name, snap, frontend) {
        Ok(Retirement::Retired) => Ok(MigrateOutcome::Moved {
            already_retired: false,
            version: installed,
        }),
        // Someone retired it during the build. The destination install
        // was explicitly asked for and stands; there is simply nothing
        // left to retire, which is the goal state.
        Ok(Retirement::AlreadyGone) => Ok(MigrateOutcome::Moved {
            already_retired: true,
            version: installed,
        }),
        Ok(Retirement::Mismatch) => Ok(MigrateOutcome::Incomplete(format!(
            "`{name}` {installed} is installed at {} and stays: the entry under {} changed \
             during the migration, so the source is deliberately not retired — `remove` \
             retires whichever side is wrong",
            dest.display(),
            source.display()
        ))),
        Err(e) => Ok(MigrateOutcome::Incomplete(format!(
            "`{name}` {installed} is installed at {} and stays; retiring it from {} did not \
             complete: {e:#} — resolve that and `remove` the source installation, do not \
             re-run the migration blindly (it will refuse: the destination already has the \
             crate)",
            dest.display(),
            source.display()
        ))),
    }
}

/// What the authoritative pass found and did at the source.
enum Retirement {
    /// Matched the snapshot; binaries removed, manifest committed.
    Retired,
    /// No entry anymore; nothing to do.
    AlreadyGone,
    /// An entry that no longer matches the snapshot; left untouched.
    Mismatch,
}

/// Phase B assembled for the frontend's shape: policy, lock notices and
/// the pre-retirement credential revalidation all follow the flavor,
/// then `retire_source` does the work. All errors bubble; the caller
/// owns the "destination already committed" framing, because only it
/// knows that context.
fn retire_with_frontend(
    source: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<Retirement> {
    match frontend {
        MigrateFrontend::Terminal => retire_source(
            source,
            name,
            snap,
            privileged::Policy::for_prefix(source),
            &mut |l| eprintln!("{l}"),
        ),
        #[cfg(not(feature = "tui"))]
        MigrateFrontend::Never(_) => unreachable!(),
        #[cfg(feature = "tui")]
        MigrateFrontend::Captured {
            on_line,
            before_placement,
            ..
        } => {
            let policy = privileged::Policy::for_prefix(source).screen_owned();
            // A retirement that will escalate revalidates credentials first,
            // through the same roundtrip as placement. One accepted edge: the
            // revalidation runs *before* the blocking source lock, so a long wait
            // can outlive the timestamp and end as Incomplete — safe; freshness
            // after an arbitrary wait would need the roundtrip *under* the lock,
            // a hostage-taking not worth the edge.
            if placement_needs_privilege(policy, source)? {
                before_placement(source)?;
            }
            retire_source(source, name, snap, policy, &mut |l| {
                on_line(LineKind::Notice, l);
            })
        }
    }
}

/// Phase B proper: exclusive source lock, authoritative revalidation
/// against a freshly loaded manifest, then the removal. Errors bubble;
/// the caller owns the framing.
fn retire_source(
    source: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    policy: privileged::Policy,
    notice: &mut dyn FnMut(&str),
) -> Result<Retirement> {
    let _src_lock = StateLock::acquire_with(source, &Mode::Exclusive, policy, notice)?;
    let mut src_manifest = Manifest::load(source)?;
    match src_manifest.crates.get(name) {
        Some(entry) if snap.still_matches(entry) => {}
        Some(_) => return Ok(Retirement::Mismatch),
        None => return Ok(Retirement::AlreadyGone),
    }
    let entry = src_manifest
        .crates
        .remove(name)
        .expect("matched by the revalidation above");
    let bin_dir = source.join("bin");
    let paths: Vec<PathBuf> = entry.bins.iter().map(|b| bin_dir.join(b)).collect();
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    privileged::remove_files(policy, &refs)?;
    src_manifest.store(source)?;
    Ok(Retirement::Retired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(names: &[&str]) -> Manifest {
        let mut m = Manifest::default();
        for n in names {
            m.crates.insert(
                (*n).to_owned(),
                Entry {
                    version: "1.0.0".to_owned(),
                    bins: vec![(*n).to_owned()],
                    locked: false,
                    pinned: false,
                },
            );
        }
        m
    }

    #[test]
    fn cli_shape_is_verified() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn update_requires_explicit_selection() {
        // A bare `update` is a usage error, not "update everything".
        assert!(Cli::try_parse_from(["cargo-lbin", "update"]).is_err());
        // Names and --all are mutually exclusive.
        assert!(Cli::try_parse_from(["cargo-lbin", "update", "--all", "foo"]).is_err());
        assert!(Cli::try_parse_from(["cargo-lbin", "update", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["cargo-lbin", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["cargo-lbin", "checkupdate", "--json"]).is_ok());
        // `--json` is per command, not global: it must not be accepted
        // where it would silently do nothing.
        assert!(Cli::try_parse_from(["cargo-lbin", "--json", "list"]).is_err());
        assert!(Cli::try_parse_from(["cargo-lbin", "install", "--json", "bat"]).is_err());
        assert!(Cli::try_parse_from(["cargo-lbin", "update", "foo", "bar", "-y"]).is_ok());
        // The cargo-subcommand form strips "lbin" in main(); the parser
        // itself must not accept it.
        assert!(Cli::try_parse_from(["cargo-lbin", "lbin", "update", "--all"]).is_err());
    }

    #[test]
    fn search_rows_align_and_mark_installed() {
        let hits = [
            api::Hit {
                name: "scx_beerland".to_owned(),
                version: "1.1.3".to_owned(),
                description: "A sched_ext scheduler".to_owned(),
            },
            api::Hit {
                name: "bat".to_owned(),
                version: "0.26.0".to_owned(),
                description: "x".repeat(SEARCH_DESCRIPTION_WIDTH + 5),
            },
        ];
        let installed = BTreeMap::from([("bat".to_owned(), "0.25.0".to_owned())]);
        let out = format_search_hits(&hits, &installed);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("  scx_beerland  1.1.3   A sched_ext scheduler"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("* bat           0.26.0  "),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with("  [installed 0.25.0]"), "{}", lines[1]);
        // Description cut at the width, with an ellipsis, before the marker.
        let desc = lines[1].rsplit("  ").nth(1).unwrap();
        assert_eq!(desc.chars().count(), SEARCH_DESCRIPTION_WIDTH + 1);
        assert!(desc.ends_with('…'));
    }

    #[test]
    fn info_describes_installed_state_with_checkupdate_rules() {
        let rel = |v: &str, yanked: bool| index::Release {
            version: Version::parse(v).unwrap(),
            yanked,
        };
        let releases = [
            rel("1.0.0", false),
            rel("1.1.0", true),
            rel("1.2.0", false),
            rel("2.0.0-rc.1", false),
        ];
        let m = manifest_with(&["foo"]);
        let installed = m.crates.get("foo");

        let out = describe_info("foo", &releases, installed);
        assert!(out.contains("latest:      1.2.0"), "{out}");
        assert!(out.contains("pre-release: 2.0.0-rc.1"), "{out}");
        assert!(out.contains("releases:    4 (1 yanked)"), "{out}");
        // Installed 1.0.0 is stable: the rc is not offered, 1.2.0 is.
        assert!(
            out.contains("installed:   1.0.0 (update available: 1.2.0)"),
            "{out}"
        );

        let out = describe_info("foo", &releases, None);
        assert!(out.contains("installed:   no"), "{out}");

        // Installed at the newest stable: up to date, rc still not offered.
        let mut m = manifest_with(&["foo"]);
        m.crates.get_mut("foo").unwrap().version = "1.2.0".to_owned();
        let out = describe_info("foo", &releases, m.crates.get("foo"));
        assert!(out.contains("installed:   1.2.0 (up to date)"), "{out}");

        // History and eligibility diverge: the newest stable is yanked, so
        // it is shown flagged, while the installed 1.0.0 has nowhere to go.
        let releases = [rel("1.0.0", false), rel("1.1.0", true)];
        let out = describe_info("foo", &releases, installed);
        assert!(out.contains("latest:      1.1.0 [yanked]"), "{out}");
        assert!(out.contains("installed:   1.0.0 (up to date)"), "{out}");

        // Everything yanked: `checkupdate` would refuse this crate, and
        // `info` must not call it "up to date".
        let releases = [rel("1.0.0", true)];
        let out = describe_info("foo", &releases, installed);
        assert!(out.contains("latest:      1.0.0 [yanked]"), "{out}");
        assert!(
            out.contains("installed:   1.0.0 (no non-yanked releases)"),
            "{out}"
        );
    }

    #[test]
    fn pinned_crates_are_refused_by_name_all_at_once() {
        let mut m = manifest_with(&["bat", "fd", "ripgrep"]);
        m.crates.get_mut("bat").unwrap().pinned = true;
        m.crates.get_mut("fd").unwrap().pinned = true;
        let err = refuse_pinned(&m, &["ripgrep".into(), "bat".into(), "fd".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("bat") && err.contains("fd"), "{err}");
        assert!(!err.contains("ripgrep"), "{err}");
        // The suggested command is complete and runnable as printed.
        assert!(err.contains("`cargo lbin unpin bat fd`"), "{err}");
        // Unpinned selection, and names not in the manifest, pass: the
        // latter are `select_targets`' problem, not this check's.
        assert!(refuse_pinned(&m, &["ripgrep".into(), "nope".into()]).is_ok());
    }

    #[test]
    fn completions_cover_every_shell_and_every_command() {
        use clap::{CommandFactory, ValueEnum};
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        // Whatever the commands are at the time; the test does not keep
        // its own list, which is the point.
        assert_ne!(names, Vec::<String>::new());
        for shell in clap_complete::Shell::value_variants() {
            let mut out = Vec::new();
            clap_complete::generate(*shell, &mut Cli::command(), "cargo-lbin", &mut out);
            let script = String::from_utf8(out).unwrap();
            assert_ne!(script, "", "{shell}");
            for name in &names {
                assert!(script.contains(name.as_str()), "{shell}: missing `{name}`");
            }
        }
    }

    #[test]
    fn downgrade_choice_is_a_number_or_nothing() {
        assert_eq!(parse_choice("2", 3).unwrap(), Some(1));
        assert_eq!(parse_choice(" 3\n", 3).unwrap(), Some(2));
        assert_eq!(parse_choice("", 3).unwrap(), None);
        assert_eq!(parse_choice("\n", 3).unwrap(), None);
        assert_eq!(parse_choice("q", 3).unwrap(), None);
        assert_eq!(parse_choice("Q", 3).unwrap(), None);
        for bad in ["0", "4", "-1", "1.1.2", "one", "1 2"] {
            assert!(parse_choice(bad, 3).is_err(), "{bad}");
        }
    }

    #[test]
    fn downgrade_command_takes_one_name() {
        assert!(Cli::try_parse_from(["cargo-lbin", "downgrade", "bat"]).is_ok());
        assert!(Cli::try_parse_from(["cargo-lbin", "downgrade"]).is_err());
        assert!(Cli::try_parse_from(["cargo-lbin", "downgrade", "bat", "fd"]).is_err());
        // No `--yes`: the answer is the version, and a script has
        // `install NAME@VERSION`.
        assert!(Cli::try_parse_from(["cargo-lbin", "downgrade", "bat", "--yes"]).is_err());
    }

    #[test]
    fn pin_commands_parse() {
        assert!(Cli::try_parse_from(["cargo-lbin", "pin", "bat"]).is_ok());
        assert!(Cli::try_parse_from(["cargo-lbin", "unpin", "bat", "fd"]).is_ok());
        assert!(Cli::try_parse_from(["cargo-lbin", "pin"]).is_err());
    }

    #[test]
    fn select_targets_reports_all_unknown_names_at_once() {
        let m = manifest_with(&["foo", "bar"]);
        let err = select_targets(&m, &["foo".into(), "nope".into(), "nada".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope") && err.contains("nada"), "{err}");
        assert!(!err.contains("foo"), "{err}");
    }

    #[test]
    fn select_targets_collapses_duplicates() {
        let m = manifest_with(&["foo", "bar"]);
        let targets = select_targets(&m, &["bar".into(), "foo".into(), "bar".into()]).unwrap();
        assert_eq!(targets.into_iter().collect::<Vec<_>>(), ["bar", "foo"]);
    }

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
        assert!(
            !cache
                .join("stage")
                .join(std::process::id().to_string())
                .join("straycrate")
                .exists(),
            "the stage is gone, and nothing is left alive to touch it"
        );
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
        // …and the stage is removed rather than kept as evidence.
        assert!(
            !cache
                .join("stage")
                .join(std::process::id().to_string())
                .join("slowcrate")
                .exists(),
            "a cancelled build leaves no stage behind"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migration_snapshot_protects_every_entry_field() {
        let base = Entry {
            version: "1.2.3".into(),
            bins: vec!["foo".into()],
            locked: true,
            pinned: true,
        };
        let snap = MigrationSnapshot::capture("foo", &base).unwrap();
        assert!(snap.still_matches(&base));

        let mut changed = base.clone();
        changed.version = "1.2.4".into();
        assert!(!snap.still_matches(&changed), "version is protected");

        let mut changed = base.clone();
        changed.bins.push("fooctl".into());
        assert!(!snap.still_matches(&changed), "the bin set is protected");

        let mut changed = base.clone();
        changed.locked = false;
        assert!(
            !snap.still_matches(&changed),
            "the locked flag is protected"
        );

        let mut changed = base.clone();
        changed.pinned = false;
        assert!(!snap.still_matches(&changed), "the pin is protected");
    }

    /// Shared scaffolding for the migrate tests: a prefix with a
    /// manifest entry and a placed binary, as a finished install leaves
    /// them.
    fn seeded_prefix(root: &Path, dir: &str, name: &str, locked: bool, pinned: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let prefix = root.join(dir);
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        fs::write(prefix.join("bin").join(name), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(
            prefix.join("bin").join(name),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut manifest = Manifest::default();
        manifest.crates.insert(
            name.to_owned(),
            Entry {
                version: "0.1.0".into(),
                bins: vec![name.to_owned()],
                locked,
                pinned,
            },
        );
        manifest.store(&prefix).unwrap();
        prefix
    }

    /// A fake cargo staging `name` 0.1.0, the migrate tests' build.
    fn staging_fake(root: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 mkdir -p \"$4/bin\"\n\
                 printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/{name}\"\n\
                 chmod 755 \"$4/bin/{name}\"\n\
                 printf '%s' '{{\"installs\":{{\"{name} 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{{\"bins\":[\"{name}\"]}}}}}}' > \"$4/.crates2.json\"\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A fake cargo with a registry: `--version =X` stages exactly X,
    /// no version request stages 0.2.0 — the fake's "latest". This is
    /// the fake for tests about *which* version a pipeline asks for;
    /// `staging_fake` above, blind to the request, cannot tell an
    /// exact rebuild from a latest install.
    fn versioned_fake(root: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 ver=0.2.0\n\
                 for a in \"$@\"; do\n\
                 case \"$a\" in =*) ver=${{a#=}};; esac\n\
                 done\n\
                 mkdir -p \"$4/bin\"\n\
                 printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/{name}\"\n\
                 chmod 755 \"$4/bin/{name}\"\n\
                 printf '%s' \"{{\\\"installs\\\":{{\\\"{name} $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{{\\\"bins\\\":[\\\"{name}\\\"]}}}}}}\" > \"$4/.crates2.json\"\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn migrate_rebuilds_at_the_destination_and_retires_the_source() {
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-moves");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", true, true);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&staging_fake(&root, "okcrate"));

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let outcome = migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap();
        assert!(matches!(
            &outcome,
            MigrateOutcome::Moved {
                already_retired: false,
                version,
            } if version.to_string() == "0.1.0"
        ));

        let dest_manifest = Manifest::load(&dest).unwrap();
        let entry = &dest_manifest.crates["okcrate"];
        assert_eq!(entry.version, "0.1.0");
        assert!(entry.pinned, "the pin bit travels with the crate");
        assert!(entry.locked, "the --locked flag travels with the crate");
        assert!(dest.join("bin/okcrate").is_file(), "rebuilt and placed");

        let src_manifest = Manifest::load(&source).unwrap();
        assert!(!src_manifest.crates.contains_key("okcrate"), "retired");
        assert!(!source.join("bin/okcrate").exists(), "binary removed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_carries_an_unpinned_entry_unpinned() {
        // An unpinned migration makes no exact-version request, so
        // inference would happen to agree — but the travelling bit is
        // migrate's stated promise (PinPolicy::Exactly), and this test
        // keeps the promise the load-bearing one.
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-unpinned");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&staging_fake(&root, "okcrate"));

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap();
        assert!(
            !Manifest::load(&dest).unwrap().crates["okcrate"].pinned,
            "an unpinned crate arrives unpinned — the bit travels as stated policy"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_passes_a_healthy_prefix_and_names_every_broken_invariant() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-verify");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);

        // Healthy: the seeded claim holds, no findings.
        let manifest = Manifest::load(&prefix).unwrap();
        assert!(
            verify_entries(&prefix, &manifest).0.is_empty(),
            "a healthy prefix verifies clean"
        );

        // Missing: the person did `rm ~/.local/bin/okcrate` by hand.
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("is missing"), "{errors:?}");
        assert!(
            errors[0].message.contains("install okcrate"),
            "the finding names the repair: {errors:?}"
        );
        assert!(
            errors[0]
                .message
                .contains(&format!("--prefix={}", prefix.display())),
            "the hint repairs the prefix that was audited, not the default: {errors:?}"
        );

        // Wrong type: a directory answers to the name.
        fs::create_dir(prefix.join("bin/okcrate")).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert!(
            errors[0].message.contains("not a regular file"),
            "{errors:?}"
        );
        fs::remove_dir(prefix.join("bin/okcrate")).unwrap();

        // A symlink is structural drift even when its target runs fine:
        // lbin places regular files, and verify checks structure.
        std::os::unix::fs::symlink("/bin/sh", prefix.join("bin/okcrate")).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert!(
            errors[0].message.contains("is a symlink"),
            "a healthy target does not excuse the drift: {errors:?}"
        );
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();

        // And a dangling one is honestly a symlink finding, never a
        // lying "missing".
        std::os::unix::fs::symlink("/nonexistent/target", prefix.join("bin/okcrate")).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert!(
            errors[0].message.contains("is a symlink") && !errors[0].message.contains("missing"),
            "{errors:?}"
        );
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();

        // Not executable: the file is back but the mode is wrong.
        fs::write(prefix.join("bin/okcrate"), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(
            prefix.join("bin/okcrate"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert!(errors[0].message.contains("not executable"), "{errors:?}");
        fs::set_permissions(
            prefix.join("bin/okcrate"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        // A pinned crate's remedy names the pinned version — a bare
        // `install` would be refused by the pin itself.
        let mut pinned = Manifest::load(&prefix).unwrap();
        pinned.crates.get_mut("okcrate").unwrap().pinned = true;
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let (errors, _) = verify_entries(&prefix, &pinned);
        assert!(
            errors[0].message.contains("install okcrate@0.1.0"),
            "the pinned remedy is the exact re-pin: {errors:?}"
        );

        // A locked entry's remedy carries --locked: a repair that
        // silently changes the entry's build policy is not a repair.
        let mut locked = Manifest::load(&prefix).unwrap();
        locked.crates.get_mut("okcrate").unwrap().locked = true;
        let (errors, _) = verify_entries(&prefix, &locked);
        assert!(
            errors[0].message.contains("--locked"),
            "the locked remedy keeps the policy: {errors:?}"
        );
        fs::write(prefix.join("bin/okcrate"), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(
            prefix.join("bin/okcrate"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        // Unparseable entry: hand-edited state.
        let mut broken = Manifest::load(&prefix).unwrap();
        broken.crates.get_mut("okcrate").unwrap().version = "not-a-version".into();
        let (errors, _) = verify_entries(&prefix, &broken);
        assert!(errors[0].message.contains("unparseable"), "{errors:?}");

        // Duplicate claim: two entries, one binary name — a state lbin
        // never writes, so only a hand-built manifest can carry it.
        let mut dup = Manifest::load(&prefix).unwrap();
        dup.crates.insert(
            "othercrate".into(),
            Entry {
                version: "0.1.0".into(),
                bins: vec!["okcrate".into()],
                locked: false,
                pinned: false,
            },
        );
        let (errors, _) = verify_entries(&prefix, &dup);
        assert!(
            errors.iter().any(|e| e.message.contains("claimed by")
                && e.message.contains("`okcrate`")
                && e.message.contains("`othercrate`")),
            "the duplicate claim names both owners: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_installs_the_latest_version_for_an_unpinned_crate() {
        // The contract in one test: migrate preserves policy, not
        // necessarily version. Without a pin the version was never part
        // of the intent, so the destination gets what a fresh `install`
        // would — the registry's latest — and the outcome reports that
        // version, not the plan's.
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-latest");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let outcome = migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap();
        assert!(
            matches!(
                &outcome,
                MigrateOutcome::Moved {
                    already_retired: false,
                    version,
                } if version.to_string() == "0.2.0"
            ),
            "the outcome speaks the installed version, not the plan's: {outcome:?}"
        );

        let entry = &Manifest::load(&dest).unwrap().crates["okcrate"];
        assert_eq!(entry.version, "0.2.0", "unpinned migrates to latest");
        assert!(!entry.pinned, "and stays unpinned — policy preserved");
        assert!(
            !Manifest::load(&source)
                .unwrap()
                .crates
                .contains_key("okcrate"),
            "the source 0.1.0 still matched its snapshot and was retired"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_never_prepares_the_prefix() {
        // The read-only contract at its sharpest edge: a fresh prefix.
        // `acquire` would create the lock here; "never writes" does not round
        // two small writes down to zero.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-ro");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("fresh");
        fs::create_dir_all(&prefix).unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        assert_eq!(
            report.crates,
            Some(0),
            "a fresh prefix is known-zero — knowledge, not absence"
        );
        assert!(report.errors.is_empty());
        assert!(
            !prefix.join("share").exists(),
            "verify prepared state on a prefix it promised only to read"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_reaches_the_states_the_validated_loader_refuses() {
        // The real path: a manifest `load` refuses, written to disk — verify
        // still names both invariants through `load_unvalidated`.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-load");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        fs::write(
            Manifest::path(&prefix),
            r#"{"crates":{
                "acrate":{"version":"not-a-version","bins":["shared"]},
                "bcrate":{"version":"0.1.0","bins":["shared"]}
            }}"#,
        )
        .unwrap();
        assert!(
            Manifest::load(&prefix).is_err(),
            "the validated loader refuses this state; that is its job"
        );
        let manifest = Manifest::load_unvalidated(&prefix).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert!(
            errors.iter().any(|e| e.message.contains("unparseable")),
            "the bad version became a finding: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.message.contains("claimed by")),
            "the duplicate claim became a finding: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_reports_an_undeserializable_manifest_as_its_one_finding() {
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-garbage");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        fs::write(Manifest::path(&prefix), "not json at all").unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].message.contains("cannot be parsed"),
            "the deepest inconsistency is a finding, not a failed audit: {:?}",
            report.errors
        );
        assert_eq!(
            report.crates, None,
            "behind a missing brace may sit forty entries — the count is unknown"
        );
        assert!(
            report.errors[0].message.contains("repair"),
            "serde refused the bytes, so repair-or-restore is honest: {:?}",
            report.errors
        );

        // Non-UTF-8 is the parse world, not I/O: read_to_string would have
        // laundered this into InvalidData and the wrong finding.
        fs::write(Manifest::path(&prefix), b"\xff\xfe not utf8").unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        assert!(
            report.errors[0].message.contains("cannot be parsed"),
            "corrupt bytes are corruption, not I/O: {:?}",
            report.errors
        );
        assert!(
            report.errors[0].message.contains("repair"),
            "read bytes that serde refused earn repair-or-restore: {:?}",
            report.errors
        );

        // The other world (EISDIR): content may be healthy, so no restore
        // advice.
        fs::remove_file(Manifest::path(&prefix)).unwrap();
        fs::create_dir(Manifest::path(&prefix)).unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        assert!(
            report.errors[0].message.contains("cannot be inspected"),
            "an I/O failure says nothing about the content: {:?}",
            report.errors
        );
        assert!(
            !report.errors[0].message.contains("restore"),
            "no repair advice over bytes nobody has seen: {:?}",
            report.errors
        );
        assert_eq!(report.crates, None, "unread bytes count nothing");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_distinguishes_missing_from_uninspectable() {
        // ENOTDIR through a *valid* name (`bin` is a file): not NotFound, and
        // "missing" would be a lie.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-inspect");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        fs::write(prefix.join("bin"), "a file, not a directory").unwrap();
        let mut manifest = Manifest::default();
        manifest.crates.insert(
            "weird".into(),
            Entry {
                version: "0.1.0".into(),
                bins: vec!["tool".into()],
                locked: false,
                pinned: false,
            },
        );
        let (errors, _) = verify_entries(&prefix, &manifest);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].message.contains("cannot be inspected"),
            "not-NotFound is not \"missing\": {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_reinstall_hint_is_safe_to_paste() {
        // The regression that keeps "pasteable" honest: a space and an
        // apostrophe in the prefix.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-quote");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "cargo lbin's test", "okcrate", false, false);
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let manifest = Manifest::load(&prefix).unwrap();
        let (errors, _) = verify_entries(&prefix, &manifest);
        let expected = format!("--prefix='{}/cargo lbin'\\''s test'", root.display());
        assert!(
            errors[0].message.contains(&expected),
            "the prefix is single-quoted with the classic apostrophe dance:\n  \
             finding: {}\n  expected fragment: {expected}",
            errors[0]
        );

        // A control character has no honest pasteable spelling: sanitize
        // would launder the newline into a command naming a different path.
        let sneaky = seeded_prefix(&root, "with\nnewline", "okcrate", false, false);
        fs::remove_file(sneaky.join("bin/okcrate")).unwrap();
        let manifest = Manifest::load(&sneaky).unwrap();
        let (errors, _) = verify_entries(&sneaky, &manifest);
        assert!(
            !errors[0].message.contains("cargo lbin install"),
            "a command the sanitizer would falsify is no command: {errors:?}"
        );
        assert!(
            errors[0].message.contains("cannot be spelled"),
            "the finding says why no command is offered: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pasteable_prefix_refuses_what_it_cannot_spell() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            pasteable_prefix(Path::new("/usr/local")).as_deref(),
            Some("--prefix=/usr/local")
        );
        // The = spelling keeps a dash-leading relative prefix from being
        // lexable as another option.
        assert_eq!(
            pasteable_prefix(Path::new("--weird")).as_deref(),
            Some("--prefix=--weird")
        );
        assert_eq!(
            pasteable_prefix(Path::new("/tmp/my lbin")).as_deref(),
            Some("--prefix='/tmp/my lbin'")
        );
        assert_eq!(pasteable_prefix(Path::new("/tmp/a\nb")), None);
        let non_utf8 = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(
            pasteable_prefix(non_utf8),
            None,
            "display() is lossy here; a lossy command is not a true one"
        );
    }

    #[test]
    fn shell_quote_leaves_boring_paths_bare() {
        assert_eq!(shell_quote("/usr/local"), "/usr/local");
        assert_eq!(shell_quote("/tmp/my lbin"), "'/tmp/my lbin'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn a_broken_entry_anywhere_silences_every_reinstall_hint() {
        // One validate-class breach anywhere bounces `install` off `load`;
        // hints next to sound crates must be withheld.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-poison");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let mut manifest = Manifest::load_unvalidated(&prefix).unwrap();
        manifest.crates.insert(
            "poison".into(),
            Entry {
                version: "not-a-version".into(),
                bins: vec!["poison".into()],
                locked: false,
                pinned: false,
            },
        );
        let (errors, _) = verify_entries(&prefix, &manifest);
        let missing = errors
            .iter()
            .find(|e| e.message.contains("is missing"))
            .expect("the sound crate's disk finding still exists");
        assert!(
            !missing.message.contains("cargo lbin install"),
            "a hint that bounces off load is no hint: {missing}"
        );
        assert!(
            missing
                .message
                .contains("once the manifest findings above are repaired"),
            "the finding says why the command is withheld: {missing}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_mirrors_the_whole_validate_set_and_stats_only_sound_names() {
        // The full mirror, collected not first-bailed — and the safety gate:
        // the path-like name is neither stat'd (a planted file outside bin/
        // must yield no disk verdict) nor handed to the PATH scan.
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-mirror");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        // The escape target: joined and stat'd, `../outside` would resolve
        // here and the finding would wrongly be about the disk.
        fs::write(prefix.join("outside"), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(prefix.join("outside"), fs::Permissions::from_mode(0o755)).unwrap();
        // A sound binary, so the sound half of the entry verifies clean.
        fs::write(prefix.join("bin/good"), "#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(prefix.join("bin/good"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut manifest = Manifest::default();
        manifest.crates.insert(
            "0badname".into(),
            Entry {
                version: "0.1.0".into(),
                bins: Vec::new(),
                locked: false,
                pinned: false,
            },
        );
        manifest.crates.insert(
            "weird".into(),
            Entry {
                version: "0.1.0".into(),
                bins: vec!["../outside".into(), "good".into(), "good".into()],
                locked: false,
                pinned: false,
            },
        );
        let (errors, checkable) = verify_entries(&prefix, &manifest);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("not a valid crate name")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("declares no binaries")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("not one plain filename")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.message.contains("listed twice")),
            "{errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.message.contains("outside")
                && (e.message.contains("missing") || e.message.contains("not executable"))),
            "the path-like name produced a disk verdict — it was stat'd: {errors:?}"
        );
        assert_eq!(
            checkable,
            vec!["good".to_owned()],
            "only sound names reach the PATH scan"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_findings_carry_their_subjects_as_data() {
        // The reason --json exists: kind/crate/bin/path/hint as fields,
        // not regexes over message — through the real disk pass.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-data");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, true);
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        let f = &report.errors[0];
        assert_eq!(f.kind, "binary-missing");
        assert_eq!(f.krate.as_deref(), Some("okcrate"));
        assert_eq!(f.bin.as_deref(), Some("okcrate"));
        assert_eq!(
            f.path.as_deref(),
            Some(prefix.join("bin/okcrate").as_path())
        );
        let hint = f
            .hint
            .as_deref()
            .expect("a missing binary names its repair");
        assert!(hint.starts_with("cargo lbin install okcrate@"), "{hint}");
        assert!(
            f.message.contains(hint),
            "the human line embeds the same command the field carries"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_data_fields_keep_control_characters_unlaundered() {
        // The scope this pins is textual: a control character smuggled
        // into a crate name reaches the Finding's String fields
        // unlaundered — JSON escaping preserves it without laundering
        // the diagnosed value — while the human-rendered message is
        // sanitized as before. It deliberately claims nothing about
        // arbitrary non-UTF-8 bytes in a PathBuf; that is not what
        // these fields carry.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-raw");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        fs::write(
            Manifest::path(&prefix),
            r#"{"crates":{"esc\u001bcrate":{"version":"0.1.0","bins":["esccrate"]}}}"#,
        )
        .unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        let f = report
            .errors
            .iter()
            .find(|f| f.kind == "invalid-crate-name")
            .expect("the smuggled name is invalid");
        assert!(
            f.krate.as_deref().is_some_and(|k| k.contains('\u{1b}')),
            "the crate field keeps the real bytes: {:?}",
            f.krate
        );
        assert!(
            !f.message.contains('\u{1b}'),
            "the human line stays terminal-safe"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_findings_are_terminal_safe() {
        // Through the real path: ESC survives deserialization (legal JSON)
        // — the report boundary sanitizes for both renderers.
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-sanitize");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        fs::write(
            Manifest::path(&prefix),
            r#"{"crates":{"esccrate":{"version":"0.1.0\u001b[31m","bins":["esccrate"]}}}"#,
        )
        .unwrap();
        let report = verify_prefix(&prefix, &mut |_| {}).unwrap();
        assert!(
            !report.errors.is_empty(),
            "the smuggled version is at least unparseable"
        );
        for finding in report.errors.iter().chain(report.warnings.iter()) {
            let line = &finding.message;
            assert!(
                !line.chars().any(|c| c.is_control() && c != '\t'),
                "a finding reached the boundary with a control char: {line:?}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn man_writes_one_roff_page_per_command() {
        // The contract: a page for the top command and one per real
        // subcommand, each a roff document (.TH header), none for
        // clap's implicit help.
        let dir = std::env::temp_dir().join("cargo-lbin-test-man");
        let _ = fs::remove_dir_all(&dir);
        cmd_man(&dir).unwrap();
        let top = fs::read_to_string(dir.join("cargo-lbin.1")).unwrap();
        assert!(
            top.contains("\n.TH CARGO-LBIN 1"),
            "a titled roff page: {top:.80}"
        );
        for sub in ["install", "verify", "clean", "migrate", "man"] {
            let page = dir.join(format!("cargo-lbin-{sub}.1"));
            let text = fs::read_to_string(&page)
                .unwrap_or_else(|e| panic!("{} missing: {e}", page.display()));
            assert!(
                text.contains(&format!("\n.TH CARGO-LBIN-{} 1", sub.to_uppercase())),
                "{} carries its own title",
                page.display()
            );
            // The SYNOPSIS documents the command as typed (roff escapes
            // the hyphen), not the bare subcommand name mangen would
            // fall back to without bin_name.
            assert!(
                text.contains(&format!("cargo\\-lbin {sub}")),
                "{}: the SYNOPSIS carries the real invocation",
                page.display()
            );
            assert!(
                text.contains("\\-\\-prefix") && text.contains("\\-\\-user"),
                "{}: build() propagated the global flags onto the page",
                page.display()
            );
        }
        assert!(
            !dir.join("cargo-lbin-help.1").exists(),
            "the implicit help pseudo-command earns no page"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_removes_exactly_what_was_asked_and_nothing_unasked() {
        // The contract after the opt-in turn: nothing requested is an
        // error; --stages removes exactly scan_stale_stages' answer (the
        // lockstep with verify) and spares logs; a named retention takes
        // old logs and spares stages; dry-run touches nothing; an
        // overflowed retention errors without removing a byte.
        let cache = std::env::temp_dir().join("cargo-lbin-test-clean");
        let _ = fs::remove_dir_all(&cache);
        let live = cache.join("stage").join(std::process::id().to_string());
        let dead = cache.join("stage").join(u32::MAX.to_string());
        let junk = cache.join("stage").join("not-a-pid");
        for d in [&live, &dead, &junk] {
            fs::create_dir_all(d).unwrap();
        }
        let logs = cache.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let old_log = logs.join("build-old.log");
        let new_log = logs.join("build-new.log");
        fs::write(&old_log, "old").unwrap();
        fs::write(&new_log, "new").unwrap();
        let ancient = std::time::SystemTime::now() - std::time::Duration::from_hours(90 * 24);
        fs::File::options()
            .write(true)
            .open(&old_log)
            .unwrap()
            .set_modified(ancient)
            .unwrap();

        // A mutating command does nothing it was not asked to do.
        assert!(clean_cache(&cache, false, false, None).is_err());
        assert!(
            dead.exists() && old_log.exists(),
            "the refusal removed nothing"
        );

        // The removal set is scan_stale_stages' answer, by construction.
        let named = scan_stale_stages(&cache).unwrap();
        assert!(named.contains(&dead) && named.contains(&junk) && !named.contains(&live));

        clean_cache(&cache, true, true, Some(30)).unwrap();
        assert!(
            dead.exists() && junk.exists() && old_log.exists(),
            "dry-run removed something"
        );

        // An absurd DAYS is an error, never a wrapped cutoff.
        assert!(clean_cache(&cache, false, true, Some(u64::MAX)).is_err());
        assert!(
            dead.exists() && old_log.exists(),
            "the overflow attempt removed nothing"
        );

        // Logs alone: stages are spared even when ownerless.
        clean_cache(&cache, false, false, Some(30)).unwrap();
        assert!(dead.exists() && junk.exists(), "unasked stages are spared");
        assert!(!old_log.exists(), "the old log is gone");
        assert!(new_log.exists(), "the fresh log stays");

        // Stages alone: exactly verify's set, logs untouched.
        clean_cache(&cache, false, true, None).unwrap();
        assert!(live.exists(), "the living stage is spared");
        assert!(
            !dead.exists() && !junk.exists(),
            "verify-named debris is gone"
        );
        assert!(new_log.exists(), "unasked logs are spared");
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn check_versions_honors_the_cancel_before_the_first_request() {
        // Deterministic and offline by design: the token is consulted
        // before each index request, so a pre-set cancel returns Ok(None)
        // without touching the network at all.
        let entry = Entry {
            version: "1.0.0".into(),
            bins: vec!["x".into()],
            locked: false,
            pinned: false,
        };
        let name = "anything".to_owned();
        let result = check_versions([(&name, &entry)], || true).unwrap();
        assert!(
            result.is_none(),
            "a cancelled run is an answer, not a report"
        );
    }

    #[test]
    fn scan_stale_stages_reports_dead_pids_and_spares_the_living() {
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-stages");
        let _ = fs::remove_dir_all(&root);
        let cache = root.join("cache");

        // No stage directory at all: silence, not an error.
        assert_eq!(scan_stale_stages(&cache).unwrap(), Vec::<PathBuf>::new());

        // A live PID (ours), a PID /proc cannot know, a name that is
        // not a PID at all — and, in the v2 namespace, a leased-layout
        // run whose PID is just as dead (its liveness is the lease's to
        // answer, not /proc's) plus junk, because one rule judges both
        // namespaces.
        let live = cache.join("stage").join(std::process::id().to_string());
        let dead = cache.join("stage").join(u32::MAX.to_string());
        let junk = cache.join("stage").join("not-a-pid");
        let leased = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(format!("{}-0123456789abcdef", u32::MAX));
        let junk_v2 = cache.join(crate::stage::RUN_NAMESPACE).join("not-a-run");
        // Cross-namespace names: a LIVE bare PID in stage-v2 (so /proc
        // could only spare it — proving it gets no vote there), and a
        // leased name in stage/ where nothing legally writes one.
        let legacy_in_v2 = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(std::process::id().to_string());
        let leased_in_legacy = cache
            .join("stage")
            .join(format!("{}-00000000000000cd", u32::MAX));
        for d in [
            &live,
            &dead,
            &junk,
            &leased,
            &junk_v2,
            &legacy_in_v2,
            &leased_in_legacy,
        ] {
            fs::create_dir_all(d).unwrap();
        }
        let stale = scan_stale_stages(&cache).unwrap();
        assert!(
            !stale.contains(&live),
            "a running instance's stage is not debris"
        );
        assert!(stale.contains(&dead), "a dead PID's stage is debris");
        assert!(stale.contains(&junk), "a non-PID name is debris");
        assert!(
            !stale.contains(&leased),
            "a leased-layout run is never the /proc heuristic's to condemn"
        );
        assert!(stale.contains(&junk_v2), "junk is junk in stage-v2 too");
        assert!(
            stale.contains(&legacy_in_v2),
            "a bare PID in stage-v2 is debris: /proc has no vote outside stage/"
        );
        assert!(
            stale.contains(&leased_in_legacy),
            "a leased name in stage/ is debris: nothing legally writes one there"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_rebuilds_the_pinned_version_even_when_latest_is_newer() {
        // The other half of the contract, against a registry that
        // *would* offer 0.2.0: a pin makes the version intent, and the
        // destination gets exactly it. `staging_fake`, blind to the
        // request, could never fail this test; `versioned_fake` can.
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-pinned-exact");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, true);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let outcome = migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap();
        assert!(matches!(
            &outcome,
            MigrateOutcome::Moved {
                already_retired: false,
                version,
            } if version.to_string() == "0.1.0"
        ));

        let entry = &Manifest::load(&dest).unwrap().crates["okcrate"];
        assert_eq!(entry.version, "0.1.0", "pinned rebuilds the exact version");
        assert!(entry.pinned, "and stays pinned");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_refuses_a_crate_already_at_the_destination() {
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-refuses");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = seeded_prefix(&root, "dest", "okcrate", false, false);
        // No fake: the refusal must land before any build is attempted.
        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let err = migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no --force by design"),
            "the refusal names the policy: {err:#}"
        );
        assert!(
            Manifest::load(&source)
                .unwrap()
                .crates
                .contains_key("okcrate"),
            "the source is untouched by a refusal"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_aborts_before_the_destination_commits_when_the_source_changed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-aborts");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();

        // A fake cargo that mutates the source manifest mid-build — the race
        // the early revalidation exists to catch.
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        let src_manifest_path = Manifest::path(&source);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 printf '%s' '{{\"version\":1,\"crates\":{{\"okcrate\":{{\"version\":\"0.2.0\",\"bins\":[\"okcrate\"],\"locked\":false,\"pinned\":false}}}}}}' > \"{}\"\n\
                 mkdir -p \"$4/bin\"\n\
                 printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/okcrate\"\n\
                 chmod 755 \"$4/bin/okcrate\"\n\
                 printf '%s' '{{\"installs\":{{\"okcrate 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{{\"bins\":[\"okcrate\"]}}}}}}' > \"$4/.crates2.json\"\n\
                 exit 0\n",
                src_manifest_path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        let err = migrate_one(
            &source,
            &dest,
            &root.join("cache"),
            "okcrate",
            &snap,
            &mut MigrateFrontend::Terminal,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("aborting before the destination commits"),
            "the abort names its moment: {err:#}"
        );
        assert!(
            Manifest::load(&dest).unwrap().crates.is_empty(),
            "the destination committed nothing"
        );
        assert!(
            !dest.join("bin/okcrate").exists(),
            "no binary was placed at the destination"
        );
        assert_eq!(
            Manifest::load(&source).unwrap().crates["okcrate"].version,
            "0.2.0",
            "the source keeps its newer truth; migrate touched nothing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retirement_is_authoritative_and_touches_nothing_on_mismatch() {
        let root = std::env::temp_dir().join("cargo-lbin-test-retire");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();

        // Mismatch: the entry changed after the snapshot — nothing is
        // removed, the newer truth stays.
        let mut m = Manifest::load(&source).unwrap();
        m.crates.get_mut("okcrate").unwrap().version = "0.2.0".into();
        m.store(&source).unwrap();
        assert!(matches!(
            retire_source(
                &source,
                "okcrate",
                &snap,
                privileged::Policy::for_prefix(&source),
                &mut |_| {}
            )
            .unwrap(),
            Retirement::Mismatch
        ));
        assert!(
            source.join("bin/okcrate").is_file(),
            "mismatch removes nothing"
        );

        // Match: retired — binary gone, entry gone.
        let mut m = Manifest::load(&source).unwrap();
        m.crates.get_mut("okcrate").unwrap().version = "0.1.0".into();
        m.store(&source).unwrap();
        assert!(matches!(
            retire_source(
                &source,
                "okcrate",
                &snap,
                privileged::Policy::for_prefix(&source),
                &mut |_| {}
            )
            .unwrap(),
            Retirement::Retired
        ));
        assert!(!source.join("bin/okcrate").exists());
        assert!(
            !Manifest::load(&source)
                .unwrap()
                .crates
                .contains_key("okcrate")
        );

        // Already gone: the goal state, nothing to do.
        assert!(matches!(
            retire_source(
                &source,
                "okcrate",
                &snap,
                privileged::Policy::for_prefix(&source),
                &mut |_| {}
            )
            .unwrap(),
            Retirement::AlreadyGone
        ));
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
        let snap =
            MigrationSnapshot::from_parts("okcrate", "0.1.0", vec!["okcrate".into()], false, false)
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

    #[cfg(feature = "tui")]
    #[test]
    fn captured_frontend_runs_the_whole_pipeline() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-captured-pipeline");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("fakebin");
        let prefix = root.join("prefix");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();

        // A fake cargo that stages one binary; the stage root is the
        // argument after --root ($4).
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            "#!/bin/sh\n\
             echo '   Compiling okcrate v0.1.0' >&2\n\
             mkdir -p \"$4/bin\"\n\
             printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/okcrate\"\n\
             chmod 755 \"$4/bin/okcrate\"\n\
             printf '%s' '{\"installs\":{\"okcrate 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{\"bins\":[\"okcrate\"]}}}' > \"$4/.crates2.json\"\n\
             exit 0\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        // RAII guard: the fake is cleared on drop, panics included.
        let _fake = crate::stage::FakeCargo::install(&script);

        let cache = root.join("cache");
        let mut manifest = Manifest::default();
        let mut lines: Vec<(LineKind, String)> = Vec::new();
        let mut checkpoints = 0usize;
        let control = BuildControl::new();
        let result = install_and_commit(
            &prefix,
            &cache,
            &mut manifest,
            "okcrate",
            None,
            false,
            PinPolicy::Infer,
            &mut Frontend::Captured {
                on_line: &mut |k, l| lines.push((k, l.to_owned())),
                before_placement: &mut |_| {
                    checkpoints += 1;
                    Ok(())
                },
                control: &control,
                checkpoint: None,
            },
        );
        result.unwrap();
        assert!(
            matches!(control.phase(), BuildPhase::Placement),
            "a finished install crossed the placement door"
        );

        assert_eq!(
            checkpoints, 0,
            "a user-writable prefix never reaches the checkpoint — the \
             frontend must not be made to poke sudo when nothing will \
             escalate"
        );
        assert!(
            lines
                .iter()
                .any(|(k, l)| *k == LineKind::Cargo && l.contains("Compiling okcrate")),
            "cargo output reaches the frontend as cargo's: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|(k, l)| *k == LineKind::Notice && l.starts_with("installed okcrate 0.1.0")),
            "the pipeline note arrives classified, not as anonymous text: {lines:?}"
        );
        assert!(prefix.join("bin/okcrate").is_file(), "binary placed");
        let stored = Manifest::load(&prefix).unwrap();
        assert!(stored.crates.contains_key("okcrate"), "manifest committed");
        let _ = fs::remove_dir_all(&root);
    }
}
