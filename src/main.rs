mod api;
mod hints;
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
#[cfg(test)]
mod test_support;
mod text;
#[cfg(feature = "tui")]
mod tui;
mod validate;
mod verify;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hints::{pasteable_path_arg, pasteable_prefix, unpin_hint};
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
#[cfg(feature = "tui")]
pub(crate) use verify::verify_prefix;
pub(crate) use verify::{Finding, VerifyReport};
use verify::{cmd_clean, cmd_verify};

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
        #[arg(
            required_unless_present = "all",
            conflicts_with = "all",
            value_name = "NAME[@VERSION]"
        )]
        crates: Vec<String>,
        /// Build with the crate's committed Cargo.lock (reproducible; skips
        /// newer dependency releases until the crate itself releases)
        #[arg(long, conflicts_with = "reinstall")]
        locked: bool,
        /// Rebuild what is already installed, exactly as the manifest
        /// records it
        ///
        /// The entry is the specification: its version, its pin and its
        /// `--locked` are all carried over. For rebuilding after a
        /// toolchain, libc or compiler-flag change — new artifacts, same
        /// logical installation. Naming a version or `--locked`
        /// alongside it is a usage error: the entry already says both.
        ///
        /// Cargo still uses the registry and its caches to build that
        /// version; what is not asked is *which* version or policy to
        /// apply.
        #[arg(long)]
        reinstall: bool,
        /// Rebuild every crate installed under the prefix
        ///
        /// Only with `--reinstall`, and it changes the scope, nothing
        /// else: each entry is rebuilt as its own specification, pinned
        /// ones included — a pin holds a version, and this is the
        /// operation that does not change one. The plan is printed and
        /// confirmed first, because a sweep can mean a great many
        /// builds.
        #[arg(long, requires = "reinstall")]
        all: bool,
        /// Skip the confirmation prompt (only the sweep asks one)
        ///
        /// Refused without `--all`, because there is no prompt to skip
        /// there — a flag the parser accepts and the command ignores is
        /// worse than one it refuses. Checked in dispatch, not with
        /// clap's `requires = "all"`: on clap 4.6 that constraint is
        /// satisfied by a `SetTrue` flag's own default, so `install
        /// --reinstall -y foo` parses (verified; `required_unless_present`
        /// checks runtime presence and is why `crates` works). The only
        /// thing `requires` enforced was `--all`'s *own* requirement of
        /// `--reinstall`, transitively — a misleading error for the
        /// wrong reason, gone once `--reinstall` is on the line.
        #[arg(long, short)]
        yes: bool,
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
    /// having no live owner — the set is `verify`'s own, so the two
    /// cannot drift. For leased stages (0.13's layout) the owner is a
    /// kernel lock: removal takes the stage's lease exclusively and
    /// holds it through the delete, and a lease held right now defers
    /// that stage to a later pass. For pre-lease stages ownership is
    /// still a PID heuristic — the owning cargo-lbin is gone, but a
    /// build it spawned may survive it — which is why removal is
    /// explicit and never a default. `--logs-older-than DAYS` removes
    /// failure logs past that age — the person names the retention,
    /// lbin does not invent one. The cache is the user's own; the
    /// prefix state lock is not taken and sudo is never used.
    /// `--dry-run` lists what would go and removes nothing.
    Clean {
        /// List what would be removed without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Remove stage directories with no live owner
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
    /// Reads the last recorded update check by default; `--check` asks
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
    ///
    /// `--versions` appends the full published version set, in
    /// descending semver order, with yanked releases marked — the
    /// answer to "install foo@X, but which X exists?". It is still
    /// information about the crate, so it lives here and not in a
    /// command of its own.
    Info {
        #[arg(required = true)]
        crates: Vec<String>,
        /// List every published release, highest version first ([yanked] marked)
        #[arg(long)]
        versions: bool,
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
        eprintln!(
            "run it as your normal user; sudo is requested only for the privileged \
             filesystem operations that need it"
        );
        eprintln!("(set CARGO_LBIN_ALLOW_ROOT=1 only in environments where root is the only user)");
        return ExitCode::from(EXIT_ERROR);
    }
    // Said once, before any command reads it — and for `--to` where that
    // second prefix is parsed. A JSON consumer is unaffected: this goes
    // to stderr, like every other advisory.
    if let Some(note) = bin_dir_prefix_note(&cli.prefix) {
        eprintln!("warning: {note}");
    }
    if let Cmd::Migrate { ref to, .. } = cli.cmd
        && let Some(note) = bin_dir_prefix_note(to)
    {
        eprintln!("warning: {note}");
    }
    let result = match cli.cmd {
        // Two scopes, two entry points: `--all` names the prefix, a list
        // names its members, and clap has already ruled out both at once.
        Cmd::Install { all: true, yes, .. } => cmd_reinstall_all(&cli.prefix, yes),
        Cmd::Install { yes: true, .. } => Err(anyhow::anyhow!(
            "`-y` skips the confirmation `--all` asks for; without it there is no prompt to skip"
        )),
        Cmd::Install {
            ref crates,
            locked,
            reinstall,
            ..
        } => cmd_install(&cli.prefix, crates, locked, reinstall),
        Cmd::Remove { ref crates } => cmd_remove(&cli.prefix, crates),
        Cmd::Verify { json } => cmd_verify(&cli.prefix, json),
        Cmd::Pin { ref crates } => cmd_set_pinned(&cli.prefix, crates, true),
        Cmd::Unpin { ref crates } => cmd_set_pinned(&cli.prefix, crates, false),
        Cmd::Pinned { check, json } => return cmd_pinned(&cli.prefix, check, json),
        Cmd::List { json } => cmd_list(&cli.prefix, json),
        #[cfg(feature = "tui")]
        Cmd::Tui => tui::run(&cli.prefix),
        Cmd::Info {
            ref crates,
            versions,
        } => cmd_info(&cli.prefix, crates, versions),
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
enum ShadowReport {
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
fn install_and_commit(
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

/// Pre-build warnings for an install batch about to create a second
/// cross-prefix copy: one block per foreign managed copy of a
/// requested crate that is absent from this prefix's manifest.
/// Absent-here on purpose — the plan's word is "before creating the
/// second copy": a reinstall of a crate both sides already carry
/// creates nothing, and verify already names the standing duplication.
/// A warning and never an error: double installation is legal, and
/// migrate is named — as a genuinely pasteable command when both
/// prefixes have an honest shell spelling, and not at all otherwise —
/// for the person who meant to move, not copy. Emitted before the
/// first build, so the whole batch can still be abandoned before any
/// minutes are invested.
fn duplicate_install_warnings<'a>(
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    duplicate_install_warnings_from(&prefixes::also_installed(prefix), prefix, manifest, names)
}

/// The builder behind `duplicate_install_warnings`, over any
/// cross-prefix map — split out so tests exercise the real message
/// construction (prefixes' own tests already cover the map's loading).
/// Lines, not blocks: the first carries the severity word, the
/// continuations do not, and every line is sanitized — both prefixes
/// are environment-borne, the 0.7.0 rule applies.
fn duplicate_install_warnings_from<'a>(
    also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    duplicate_install_warning_lines(
        &cross_prefix_duplicates_from(also, prefix, manifest, names),
        prefix,
    )
}

/// One foreign managed copy that an install batch is about to
/// duplicate — the *decision* as facts, shared by every surface. The
/// CLI renders these into warning lines; the TUI feeds the same lines
/// into its panel today and may render the fields natively tomorrow —
/// either way the answer to "will this install create a duplicate?"
/// has exactly one author, and no surface parses a string built for
/// another.
struct CrossPrefixDuplicate {
    name: String,
    other_prefix: PathBuf,
    other_version: String,
    /// The pasteable migrate command, present only when both prefixes
    /// have an honest shell spelling (see `pasteable_path_arg`); the
    /// spelling question is decided here, with the facts, so no
    /// renderer can disagree about it.
    migrate_hint: Option<String>,
}

/// The decision behind `duplicate_install_warnings`, over any
/// cross-prefix map: which requested crates, absent from this prefix's
/// manifest, are managed elsewhere — one entry per foreign copy.
fn cross_prefix_duplicates_from<'a>(
    also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<CrossPrefixDuplicate> {
    let mut duplicates = Vec::new();
    for name in names {
        if manifest.crates.contains_key(name) {
            continue;
        }
        let Some(entries) = also.get(name) else {
            continue;
        };
        for other in entries {
            let migrate_hint = match (
                pasteable_path_arg("--prefix", &other.prefix),
                pasteable_path_arg("--to", prefix),
            ) {
                (Some(source), Some(dest)) => {
                    Some(format!("cargo lbin migrate {name} {source} {dest}"))
                }
                _ => None,
            };
            duplicates.push(CrossPrefixDuplicate {
                name: name.to_owned(),
                other_prefix: other.prefix.clone(),
                other_version: other.version.clone(),
                migrate_hint,
            });
        }
    }
    duplicates
}

/// The CLI's words over the shared facts: three lines per duplicate,
/// every one sanitized for the terminal. The migrate line is printed
/// exactly when the facts carry an honest command — the sanitize
/// boundary protects the terminal, not the shell; a laundered path
/// would be safe to paste and wrong to run, and quoting is what keeps
/// `/tmp/$(touch owned)` a directory name instead of a command. With
/// no honest spelling the mechanism is still named, worded so nobody
/// mistakes it for a pasteable hint. Same rule as the verify reinstall
/// hint.
fn duplicate_install_warning_lines(
    duplicates: &[CrossPrefixDuplicate],
    prefix: &Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    for dup in duplicates {
        lines.push(text::sanitize(&format!(
            "warning: `{}` is already managed under {} @{}",
            dup.name,
            dup.other_prefix.display(),
            dup.other_version
        )));
        lines.push(text::sanitize(&format!(
            "this will install another copy under {}",
            prefix.display()
        )));
        match &dup.migrate_hint {
            Some(hint) => {
                lines.push(text::sanitize(&format!(
                    "use `{hint}` if you intended to move it"
                )));
            }
            None => lines.push(text::sanitize(
                "use `cargo lbin migrate` with explicit --prefix/--to \
                 if you intended to move it",
            )),
        }
    }
    lines
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

/// The state half of the escalation union: the manifest plus, where
/// escalation is possible, the lock file. Its own question because it
/// is its own write set — a pin flip must not be called privileged over a
/// read-only bin it never touches.
#[cfg(feature = "tui")]
fn state_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    Ok(policy.probe_destination(&prefix.join("share/cargo-lbin"))?
        || (matches!(policy.sudo, privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(prefix)))
}

/// Is the late escalation certain enough to announce?
///
/// Only when both probes answered and answered this exact way: the
/// source escalates, the destination does not. An `Err` is not a
/// quiet "no" — a destination whose privilege cannot be judged may
/// refuse the migration outright a moment later, and a heads-up about
/// a build that never starts is exactly the false promise the notice
/// exists to avoid. Unknown announces nothing.
fn late_escalation_certain(source: &Result<bool>, dest: &Result<bool>) -> bool {
    matches!((source, dest), (Ok(true), Ok(false)))
}

/// The heads-up for a migration whose privileged half comes last.
///
/// It promises the escalation, not the prompt: retiring from a
/// privileged prefix will need sudo, and whether sudo *asks* depends
/// on a timestamp nobody can predict from here. Shared by both
/// surfaces, like the requirement line it precedes.
fn late_escalation_note(name: &str, source: &Path) -> String {
    text::sanitize(&format!(
        "retiring `{name}` from {} needs sudo after the build; \
         a password may be requested then",
        source.display()
    ))
}

/// The same heads-up for a run of builds placing into a privileged
/// prefix.
///
/// Placement asks at each member's own door, after that member is
/// built, so on a long batch the first prompt arrives minutes in and
/// with nothing on screen that explains why. Said once before the
/// first build, it promises the escalation and not the prompt —
/// sudo's timestamp decides whether it asks at all, and after the
/// first member it usually will not.
#[cfg(feature = "tui")]
pub(crate) fn batch_escalation_note(prefix: &Path) -> String {
    text::sanitize(&format!(
        "builds run unprivileged; placing under {} needs sudo after each \
         build, and a password may be requested then",
        prefix.display()
    ))
}

/// A prefix whose last component is `bin` — almost certainly one
/// directory too deep.
///
/// A prefix is the parent of `bin`, so `--prefix /usr/local/bin` puts
/// binaries in `/usr/local/bin/bin` and state in
/// `/usr/local/bin/share`: legal, occasionally even intended, and
/// usually a slip that only shows up later as a PATH complaint about a
/// directory nobody meant to create. So it is a warning and never a
/// refusal — the tool says what it read and what that implies, and the
/// person decides. `None` when there is nothing to say, so the caller
/// prints only when there is.
fn bin_dir_prefix_note(prefix: &Path) -> Option<String> {
    if prefix.file_name()? != "bin" {
        return None;
    }
    // A bare relative `bin` has an empty parent, and "did you mean ?"
    // helps nobody: the directory it means is the current one, spelled
    // the way a shell would take it back.
    let parent = prefix.parent()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Some(text::sanitize(&format!(
        "prefix {} ends in `bin`, so binaries go to {} — a prefix is the parent of `bin`; \
         did you mean {}?",
        prefix.display(),
        prefix.join("bin").display(),
        parent.display()
    )))
}

/// The escalation union for operations placing/removing under bin. The
/// build preflight, in-place removal and both retirements — the
/// captured one and the CLI's — must never disagree about whether a
/// prefix needs privilege; whether a password is actually *asked* for
/// is sudo's timestamp to decide, and a warm one means none. Private
/// copies of this `||` would drift.
/// No longer TUI-only for exactly that reason: the CLI's migration
/// asks the same question before it announces the password it needs.
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
    fn with_current<R>(&self, f: impl FnOnce(&BuildControl) -> R) -> Option<R> {
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
        // The module doc has promised this from the start: `U` cannot
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

fn cmd_install(prefix: &Path, crates: &[String], locked: bool, reinstall: bool) -> Result<()> {
    // Parsed and de-duplicated first: the pin check runs once against the
    // manifest as it is now (see `parse_all`).
    let specs = InstallSpec::parse_all(crates)?;
    if reinstall && let Some(s) = specs.iter().find(|s| s.version.is_some()) {
        // Two answers to "which version", and no reason to prefer one:
        // the entry already states it.
        bail!(
            "`--reinstall` takes the version from the manifest; \
             drop `@` from `{}` or drop `--reinstall`",
            s.name
        );
    }
    let cache = cache_dir()?;
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // A pin is the more deliberate and durable statement, and every
    // plain install re-interprets the entry it lands on — bare by
    // resolving the newest release, `@VERSION` by re-pinning. Both are
    // refused before the first build; the pin comes off explicitly
    // first. `--reinstall` restates the entry, pin included, so it has
    // nothing to refuse. And a member whose artifact specification
    // changes nothing — same effective version, same `--locked` — is
    // redirected to the verb that says what it does (see
    // `refuse_diagonal`).
    if !reinstall {
        let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        refuse_pinned(prefix, &manifest, &names)?;
        for spec in &specs {
            refuse_diagonal(prefix, &manifest, spec, locked)?;
        }
    }
    // Every plan is resolved before the first build: "not installed" is
    // knowable from the manifest alone, and a batch that discovers it
    // after twenty minutes of compiling spent that time to report
    // something it knew at the start.
    let plans: Vec<Option<ReinstallPlan>> = specs
        .iter()
        .map(|s| {
            reinstall
                .then(|| reinstall_plan(&manifest, &s.name))
                .transpose()
        })
        .collect::<Result<_>>()?;
    let mut frontend = Frontend::Terminal;
    // Before the first build: the person still holds the whole batch
    // and has invested nothing.
    for w in duplicate_install_warnings(prefix, &manifest, specs.iter().map(|s| s.name.as_str())) {
        frontend.warning(&w);
    }
    // From here on the batch is a plural, not a molecule. The whole
    // plan was refused or admitted above as one request — a contract
    // refusal mid-loop is structurally impossible — and each member
    // below is its own unit: build, collision check, placement,
    // manifest commit, exactly `install_and_commit`'s boundary. A
    // member's failure is reported where it happened and the loop
    // carries on, the policy `update --all`, `--reinstall --all` and
    // `migrate` have always used: an error in one crate says nothing
    // about the next, and stopping bought no atomicity — the state after a
    // failure is partial either way, and the only thing a break ever
    // produced was no answer about the rest. The exit code speaks for
    // the request as a whole: any shortfall is non-zero.
    let total = specs.len();
    let mut installed = 0usize;
    let mut failed: Vec<&str> = Vec::new();
    for (i, (spec, plan)) in specs.iter().zip(&plans).enumerate() {
        if total > 1 {
            println!("[{}/{total}] {}", i + 1, spec.name);
        }
        // `--reinstall` reads the entry as the specification — version,
        // pin and `--locked` together — so what it rebuilds is this
        // installation rather than whatever the registry now calls
        // newest. The read above needs no further guarding: this lock
        // was taken before the manifest was loaded and is held across
        // every build in the batch, so nothing else can move an entry
        // between the plan and its commit.
        let (version, locked, pin) = match plan {
            Some(plan) => (
                Some(&plan.version),
                plan.locked,
                PinPolicy::Exactly(plan.pinned),
            ),
            // `Infer` is install's own rule: an exact version pins,
            // otherwise an existing pin is carried over.
            None => (spec.version.as_ref(), locked, PinPolicy::Infer),
        };
        match install_and_commit(
            prefix,
            &cache,
            &mut manifest,
            &spec.name,
            version,
            locked,
            pin,
            ShadowReport::OnCommit,
            &mut frontend,
        ) {
            Ok(_) => installed += 1,
            // A single named crate keeps its exact error as the exit:
            // there is no batch to summarise.
            Err(err) if total == 1 => return Err(err),
            Err(err) => {
                eprintln!("error: installing `{}` failed: {err:#}", spec.name);
                failed.push(spec.name.as_str());
            }
        }
    }
    if total > 1 {
        println!("installed {installed} of {total}");
        if !failed.is_empty() {
            // Asked for `total` installs; the shortfall is the exit
            // status, mirrored on `apply_updates`.
            bail!(
                "{} of {total} installs not carried out (failed: {})",
                total - installed,
                failed.join(", ")
            );
        }
    }
    Ok(())
}

/// `install --reinstall --all`: the same operation, every entry.
///
/// Scope is the only thing `--all` changes. Each crate is rebuilt as
/// its own specification, so a pinned one is included rather than
/// skipped — `update --all` skips pins because it would move them off
/// their version, and this is precisely the operation that does not.
/// The pair is worth stating: `update --all` changes versions where
/// policy allows, `install --reinstall --all` changes neither version
/// nor policy and rebuilds the state as it stands.
///
/// The plan is shown and confirmed first. Not because the operation is
/// dangerous — it changes neither version nor policy — but because the
/// scope is large and each entry costs a full build, and this tool
/// shows what it is about to spend.
///
/// Failures do not stop the sweep, as in `update --all`: crates are
/// independent, and one that no longer builds under a new toolchain is
/// the reason to know about the rest, not to stop asking. The exit code
/// answers whether the confirmed plan was carried out in full.
fn cmd_reinstall_all(prefix: &Path, yes: bool) -> Result<()> {
    let cache = cache_dir()?;
    // Phase 1: read-only snapshot under a shared lock, released before
    // the prompt — an unanswered "proceed?" must not block the prefix.
    // Phase 2 reloads and re-verifies anyway. The single-crate path
    // needs none of this: it has no prompt, so its exclusive lock spans
    // the whole operation and nothing can move underneath it.
    let snapshot = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    let names: Vec<String> = snapshot.crates.keys().cloned().collect();
    if names.is_empty() {
        println!("nothing installed under {}", prefix.display());
        return Ok(());
    }
    // Resolved before anything is shown: a plan that cannot be made is
    // not a plan to confirm.
    let planned: Vec<(String, ReinstallPlan)> = names
        .into_iter()
        .map(|n| reinstall_plan(&snapshot, &n).map(|p| (n, p)))
        .collect::<Result<_>>()?;
    for (name, plan) in &planned {
        let pinned = if plan.pinned { " [pinned]" } else { "" };
        let locked = if plan.locked { " [locked]" } else { "" };
        println!("{name} {}{pinned}{locked}", plan.version);
    }
    println!(
        "rebuild {} crate(s) under {}",
        planned.len(),
        prefix.display()
    );
    if !yes && !confirm("proceed with reinstall?")? {
        println!("aborted");
        return Ok(());
    }
    apply_reinstalls(prefix, &cache, &planned)
}

/// The mutating half of the sweep: exclusive lock, fresh manifest, each
/// confirmed plan re-verified against it.
///
/// What was shown is what is rebuilt. The plan named three facts per
/// crate — version, pin, `--locked` — so all three are re-checked: a
/// crate updated, pinned, unpinned or removed during the prompt is no
/// longer the crate that was confirmed, and it is skipped with a note
/// rather than rebuilt against a state nobody agreed to. A crate
/// installed during the prompt is not swept either: it was not in the
/// plan.
fn apply_reinstalls(
    prefix: &Path,
    cache: &Path,
    planned: &[(String, ReinstallPlan)],
) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    let total = planned.len();
    let mut rebuilt = 0usize;
    let mut skipped: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    let mut frontend = Frontend::Terminal;
    for (i, (name, plan)) in planned.iter().enumerate() {
        println!("[{}/{total}] {name}", i + 1);
        let unchanged = manifest.crates.get(name).is_some_and(|e| {
            e.version == plan.version.to_string()
                && e.pinned == plan.pinned
                && e.locked == plan.locked
        });
        if !unchanged {
            eprintln!("skipping `{name}`: state changed since the reinstall was confirmed");
            skipped.push(name);
            continue;
        }
        match install_and_commit(
            prefix,
            cache,
            &mut manifest,
            name,
            Some(&plan.version),
            plan.locked,
            PinPolicy::Exactly(plan.pinned),
            ShadowReport::OnCommit,
            &mut frontend,
        ) {
            Ok(_) => rebuilt += 1,
            Err(err) => {
                eprintln!("error: rebuilding `{name}` failed: {err:#}");
                failed.push(name);
            }
        }
    }
    println!("rebuilt {rebuilt} of {total}");
    // Asked for `total` rebuilds; any shortfall exits non-zero, as
    // everywhere else: the exit code answers whether the confirmed plan
    // was carried out in full.
    let mut shortfall = Vec::new();
    if !failed.is_empty() {
        shortfall.push(format!("failed: {}", failed.join(", ")));
    }
    if !skipped.is_empty() {
        shortfall.push(format!("skipped: {}", skipped.join(", ")));
    }
    if !shortfall.is_empty() {
        bail!(
            "{} of {total} rebuilds not applied ({})",
            total - rebuilt,
            shortfall.join("; ")
        );
    }
    Ok(())
}

/// What `--reinstall` rebuilds: the entry, read as a specification.
///
/// Three facts, not one. The version says what to build; the pin and
/// `--locked` say what this installation *is*, and a rebuild that
/// dropped either would change the thing it claims to preserve — an
/// unpinned crate must not come back pinned just because the version
/// was named, and a crate built reproducibly must not quietly start
/// resolving dependencies afresh.
struct ReinstallPlan {
    version: Version,
    pinned: bool,
    locked: bool,
}

fn reinstall_plan(manifest: &Manifest, name: &str) -> Result<ReinstallPlan> {
    // Nothing to re-install, and installing the newest instead would be
    // answering a question nobody asked.
    let entry = manifest
        .crates
        .get(name)
        .with_context(|| format!("not installed: {name}"))?;
    // Destructured exhaustively, like `MigrationSnapshot`: this claims
    // to rebuild the entry, so a field added later must be considered
    // here rather than silently dropped. `bins` is the one part the
    // build decides for itself — the rebuild recomputes it.
    let Entry {
        version,
        bins: _,
        locked,
        pinned,
        // Like `bins`: the rebuild recomputes it — the new build files
        // its own testimony.
        built_with_rustc: _,
    } = entry;
    let version = Version::parse(version).with_context(|| {
        format!("`{name}` records an unparsable version ({version}); verify says more")
    })?;
    Ok(ReinstallPlan {
        version,
        pinned: *pinned,
        locked: *locked,
    })
}

/// A contract refusal from an install gate.
///
/// The requested lifecycle operation was rejected by a deliberate
/// contract gate **before its build or managed-state mutation
/// began**: no build ran, no binaries changed, and this operation
/// wrote no manifest entry. That is the whole guarantee, stated at
/// exactly the size the code can keep. Bookkeeping outside the
/// crate's lifecycle may well have happened — acquiring the state
/// lock can create its file, and a bare install's lookup has already
/// recorded what the registry answered, refusal or not, because
/// knowledge is born at the query. And on a moving registry the same
/// command may stop refusing tomorrow: the decision is a function of
/// the answered state, not a constant of the command line.
///
/// Distinct in kind from the authorization refusal at the placement
/// door, which can follow a successful build ("built, but not
/// placed"): that one is about permission to place a result, this
/// one about the contract of the request itself — the name says
/// `Contract` so the two never blur. Today the two install gates
/// emit it (the pin gate and the diagonal); the remaining contract
/// gates — downgrade's pin check, migrate's same-prefix and
/// overwrite refusals — still speak plain errors and adopt the type
/// when they are next touched. The
/// interface renders the category ("refused" instead of "failed"),
/// tests assert it instead of matching message text, and exit codes
/// deliberately stay undistinguished until a real consumer appears.
pub struct ContractRefusal(String);

impl std::fmt::Debug for ContractRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for ContractRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ContractRefusal {}

/// The gates' constructor; also what tests hand to helpers that
/// classify errors.
pub(crate) fn contract_refusal(message: String) -> anyhow::Error {
    anyhow::Error::new(ContractRefusal(message))
}

/// `bail!` for contract gates: the same early return, carrying the
/// `ContractRefusal` category.
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(crate::contract_refusal(format!($($arg)*)))
    };
}

/// Error if any of `crates` is pinned: a pin is the more deliberate
/// and durable statement, so it wins; the message says how to change
/// that, scoped to the prefix the refusal is about.
fn refuse_pinned(prefix: &Path, manifest: &Manifest, crates: &[String]) -> Result<()> {
    let pinned: Vec<&str> = crates
        .iter()
        .filter(|n| manifest.crates.get(n.as_str()).is_some_and(|e| e.pinned))
        .map(String::as_str)
        .collect();
    if !pinned.is_empty() {
        let names = pinned.join(" ");
        refuse!(
            "pinned: {} {}",
            pinned.join(", "),
            unpin_hint(prefix, &names)
        );
    }
    Ok(())
}

/// The install diagonal: does the requested artifact specification
/// match what is already installed? Only version and `--locked` count
/// — they are the specification of the artifact. The pin is
/// deliberately absent: it is a constraint on future operations, not a
/// parameter of the build, so it is never a reason to build (`pin`
/// sets it without one), and a pinned entry never reaches this check —
/// `refuse_pinned` runs first.
fn is_install_diagonal(
    entry_locked: bool,
    current: &Version,
    target: &Version,
    locked: bool,
) -> bool {
    entry_locked == locked && current == target
}

/// The diagonal's two refusals, chosen by how the version was stated.
/// Split from the resolution below so the contract is testable without
/// a registry.
///
/// For a bare install, reaching the diagonal means a rebuild is the
/// only remaining effect, and `--reinstall` names that intent
/// directly. For `@current-version`, pinning is an additional,
/// independent intent: the refusal names both `pin` and
/// `--reinstall`, and their composition expresses pin+rebuild without
/// giving it another verb.
fn refuse_on_diagonal(
    name: &str,
    entry_locked: bool,
    current: &Version,
    target: &Version,
    named: bool,
    locked: bool,
    scope: Option<&str>,
) -> Result<()> {
    if !is_install_diagonal(entry_locked, current, target, locked) {
        return Ok(());
    }
    // The redirect is a command meant for pasting, so it carries the
    // operation's scope; without an honest spelling it names the verbs
    // instead.
    let Some(arg) = scope else {
        if named {
            refuse!(
                "{name} {target} is already installed with this policy; \
                 `pin` keeps it, `install --reinstall` rebuilds it"
            );
        }
        refuse!(
            "{name} {target} is already the newest release with this policy; \
             `install --reinstall` rebuilds it"
        );
    };
    if named {
        refuse!(
            "{name} {target} is already installed with this policy\n  \
             to pin the current installation:  cargo lbin pin {name} {arg}\n  \
             to rebuild it:                    cargo lbin install --reinstall {name} {arg}"
        );
    }
    refuse!(
        "{name} {target} is already the newest release with this policy\n  \
         to rebuild it: cargo lbin install --reinstall {name} {arg}"
    );
}

/// Refuse a plain `install` whose artifact specification changes
/// nothing.
///
/// Version and `--locked` are the artifact specification; the pin is
/// not part of it. Runs after `refuse_pinned`, so any entry seen here
/// is unpinned. For a named version the comparison is local; for a
/// bare install the effective version is the newest eligible release,
/// by the same rules `update` uses — one index query per
/// already-managed member, made before the first build under the lock
/// the builds already hold. The refusal costs a verb, not a
/// capability. Build if and only if the artifact specification changes
/// or `--reinstall` asks; a pin is never the cause of a build, and
/// never removed as a side effect.
fn refuse_diagonal(
    prefix: &Path,
    manifest: &Manifest,
    spec: &InstallSpec,
    locked: bool,
) -> Result<()> {
    let Some(entry) = manifest.crates.get(&spec.name) else {
        return Ok(());
    };
    let name = &spec.name;
    let scope = pasteable_prefix(prefix);
    let scope = scope.as_deref();
    let current = Version::parse(&entry.version)
        .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
    if let Some(v) = &spec.version {
        return refuse_on_diagonal(name, entry.locked, &current, v, true, locked, scope);
    }
    // Every bare install resolves the newest release, so every bare
    // install asks — a deliberate lookup, not a side effect. Whether
    // the gate then refuses or a build runs, the registry has answered
    // about this crate and the answer is recorded: knowledge is born
    // at the query, not at the outcome.
    //
    // The `--locked` flip is the one caller that does not need the
    // answer — the build is real on policy grounds alone, the
    // project's one "unlock" — so on that path the lookup is
    // opportunistic: a failure records nothing and blocks nothing,
    // because the build about to run will ask the registry itself and
    // fail louder. Off the flip the diagonal cannot be decided without
    // the answer, and a failed lookup stays the error it is.
    let flip = entry.locked != locked;
    let latest = match index::published_versions(name) {
        Ok(versions) => index::latest_relevant(&versions, &current),
        Err(_) if flip => None,
        Err(e) => return Err(e),
    };
    if let Some(latest) = &latest {
        record_knowledge(
            prefix,
            vec![Checked {
                name: name.clone(),
                current: current.clone(),
                latest: latest.clone(),
                // Stamped at the answer, before any wait the write may
                // incur: knowledge is ordered by the query, not by
                // persistence.
                checked_at: Some(report::now_secs()),
            }],
        );
    }
    if flip {
        return Ok(());
    }
    let Some(latest) = latest else {
        return Ok(());
    };
    refuse_on_diagonal(name, entry.locked, &current, &latest, false, locked, scope)
}

/// The freshness footer `list` and `pinned` share, on stderr: the
/// watermark when it covers everything on display, otherwise which
/// problem prevents one — a check that does not cover the listing, or
/// no check at all — with the pasteable command that fixes it.
/// `extra` is the surface's own tail for that hint (`pinned` offers
/// `--check` too). One copy, because two footers that drift is how
/// the same prefix gets two opinions about its own freshness.
fn freshness_footer(
    prefix: &Path,
    report: Option<&Report>,
    watermark: Option<std::time::Duration>,
    extra: &str,
) {
    match (report, watermark) {
        (Some(_), Some(age)) => {
            eprintln!("update check: {}", report::describe_age(age));
        }
        (Some(_), None) => match pasteable_prefix(prefix) {
            Some(arg) => eprintln!(
                "update check does not cover everything listed; \
                 run `cargo lbin checkupdate {arg}`{extra}"
            ),
            None => eprintln!(
                "update check does not cover everything listed; \
                 run a `checkupdate` for this installation{extra}"
            ),
        },
        (None, _) => match pasteable_prefix(prefix) {
            Some(arg) => {
                eprintln!("no update check recorded; run `cargo lbin checkupdate {arg}`{extra}");
            }
            None => {
                eprintln!(
                    "no update check recorded; run a `checkupdate` for this installation{extra}"
                );
            }
        },
    }
}

/// The registry has just answered about these managed crates; write
/// that down.
///
/// The other half of `report.rs`'s opening sentence, at fact
/// granularity: whenever a managed-lifecycle operation resolves
/// update status, the answer is recorded, whether it asked about the
/// whole prefix or one crate — and only those operations; `info`
/// browses and records nothing. Best-effort
/// like every cache write — a failure is a warning, never the
/// operation's error, because the knowledge was for the report, not
/// for the operation that happened to acquire it.
fn record_knowledge(prefix: &Path, learned: Vec<Checked>) {
    if learned.is_empty() {
        return;
    }
    if let Err(e) = absorb_and_store(prefix, learned) {
        eprintln!("warning: could not record the update check: {e:#}");
    }
}

/// The one read-modify-write of the report cache, serialized by the
/// cache-side lock so a concurrent full sweep cannot land between the
/// load and the store and be quietly reverted. Returns the merged
/// report: `pinned --check` displays exactly what it recorded.
fn absorb_and_store(prefix: &Path, learned: Vec<Checked>) -> Result<Report> {
    let cache = cache_dir()?;
    let _lock = report::write_lock(&cache, prefix)?;
    let mut report = match Report::load(&cache, prefix)? {
        Some(report) => report,
        // First knowledge before any full check: a partial-born
        // report, with no baseline to invent.
        None => Report::partial(prefix, Vec::new())?,
    };
    report.absorb(learned);
    report.store(&cache)?;
    Ok(report)
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

/// The report `pinned --check` displays. The stored report absorbs
/// the pinned facts, each stamped at its own answer, and is shown as
/// merged: the top-level baseline stays the prefix's last full check
/// (or None when none happened), and only the pinned records carry
/// fresh stamps — both levels keep their meaning regardless of which
/// command produced the JSON. An empty pinned set asked the registry
/// nothing and records nothing — the same guard `record_knowledge`
/// keeps — and when the cache cannot be written, the answer is this
/// moment's facts alone, partial by construction.
fn checked_pinned_report(prefix: &Path, manifest: &Manifest) -> Result<Option<Report>> {
    let learned = check_versions(manifest.crates.iter().filter(|(_, e)| e.pinned), || false)?
        .expect("a `|| false` token never cancels");
    if learned.is_empty() {
        return Ok(
            match cache_dir().and_then(|cache| Report::load(&cache, prefix)) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("warning: {e:#}");
                    None
                }
            },
        );
    }
    Ok(match absorb_and_store(prefix, learned.clone()) {
        Ok(merged) => Some(merged),
        Err(e) => {
            eprintln!("warning: could not record the update check: {e:#}");
            Some(Report::partial(prefix, learned)?)
        }
    })
}

fn cmd_pinned(prefix: &Path, check: bool, json: bool) -> ExitCode {
    // Everything needed for the listing is resolved before writing
    // anything to stdout, so failures cannot leave a partial listing.
    let outcome = (|| {
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        // Statuses shown come from one source, never a blend: the
        // recorded report, or a fresh pinned-only query. The query's
        // facts are persisted per entry — the rule that once forbade
        // saving a partial snapshot guarded a single global timestamp,
        // and every fact now carries its own.
        let report = if check {
            checked_pinned_report(prefix, &manifest)?
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
            // Same watermark as `list`, over the pinned subset on
            // display; an empty subset asserts nothing.
            let pinned_entries = || {
                manifest
                    .crates
                    .iter()
                    .filter(|(_, e)| e.pinned)
                    .map(|(name, entry)| (name.as_str(), entry.version.as_str()))
            };
            if pinned_entries().next().is_some() {
                let watermark = report
                    .as_ref()
                    .and_then(|r| r.knowledge_watermark(pinned_entries()));
                freshness_footer(prefix, report.as_ref(), watermark, " or use `--check`");
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
    // Purely local: the last recorded update check, if any. An unreadable
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
    // not for whatever may be parsing stdout. The footer is the oldest
    // knowledge behind everything on display — and only that: one
    // listed crate the report cannot speak about makes any global age
    // a false sentence, so the footer says which problem it has
    // instead of picking an age that lies. An empty listing asserts
    // nothing and gets no footer.
    if !manifest.crates.is_empty() {
        let watermark = report.as_ref().and_then(|r| {
            r.knowledge_watermark(
                manifest
                    .crates
                    .iter()
                    .map(|(name, entry)| (name.as_str(), entry.version.as_str())),
            )
        });
        freshness_footer(prefix, report.as_ref(), watermark, "");
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
/// one more round-trip than the one already in flight. Who passes a
/// real token is a caller's choice: `r` and `U` do, because they ask
/// about a whole prefix; the CLI and the single-crate lookups pass
/// `|| false`, where one request is already in flight by the time
/// anyone could cancel it.
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
            // Stamped the moment this crate's answer arrived — not when
            // the batch, the command, or the store finishes. Knowledge
            // is ordered by the query, per fact.
            checked_at: Some(report::now_secs()),
        });
    }
    Ok(Some(checked))
}

/// The full published version set, in descending semver order, one
/// release per line with `release_label`'s yanked mark. Semver order,
/// not reverse chronology, on purpose: the question the section
/// answers — "install foo@X, but which X exists?" — lives on the
/// version axis, so a 1.9.7 backported *after* 2.0.0 still sorts below
/// it (the index stores publication order, which is neither). Everything
/// is listed, pre-releases and yanked included: this is history, and
/// eligibility is the `installed` line's business, not this section's.
fn describe_versions(releases: &[index::Release]) -> String {
    let mut out = String::from("  versions:\n");
    let mut sorted: Vec<&index::Release> = releases.iter().collect();
    sorted.sort_by(|a, b| b.version.cmp(&a.version));
    for release in sorted {
        // Formatting into a String cannot fail; see `describe_info`.
        use std::fmt::Write as _;
        let _ = writeln!(out, "    {}", release_label(release));
    }
    out
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
fn describe_info(
    name: &str,
    releases: &[index::Release],
    installed: Option<&Entry>,
    also: &[prefixes::AlsoIn],
) -> String {
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
    // The other prefix's copy is worth naming whether or not this
    // prefix has one: version skew is the point when both exist, and a
    // "not here, but over there" answers exactly the question asked.
    let mut also_lines = String::new();
    for other in also {
        // Sanitized like every human-readable cross-prefix line —
        // prefixes::describe already runs its output through
        // text::sanitize, because a prefix can come from the
        // environment; this new representation of the same data must
        // not carry a weaker policy. The version needs nothing: it is
        // semver-validated at the manifest boundary.
        let prefix = text::sanitize(&other.prefix.display().to_string());
        let _ = writeln!(also_lines, "  also in:     {prefix} @{}", other.version);
    }
    let Some(entry) = installed else {
        out.push_str("  installed:   no\n");
        out.push_str(&also_lines);
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
    } else {
        let newer = Version::parse(&entry.version).ok().and_then(|current| {
            index::latest_relevant(&live, &current).filter(|latest| *latest > current)
        });
        if let Some(latest) = newer {
            let _ = writeln!(out, " (update available: {latest})");
        } else {
            out.push_str(" (up to date)\n");
        }
    }
    // The entry's own facts, exactly as the manifest holds them — the
    // full single-crate view `list` gives in aggregate. yes/no over
    // bare flags: the label is the question, the value must read as
    // its answer.
    let flag = |b: bool| if b { "yes" } else { "no" };
    let _ = writeln!(out, "  pinned:      {}", flag(entry.pinned));
    let _ = writeln!(out, "  locked:      {}", flag(entry.locked));
    let _ = writeln!(out, "  binaries:    {}", entry.bins.join(", "));
    match &entry.built_with_rustc {
        // Cargo's own record for this install; shown at its first
        // line — the full report lives in the manifest and in JSON.
        Some(report) => {
            // Subprocess text (a wrapper can shape it): stored
            // verbatim, spoken sanitized.
            let first = text::sanitize(report.lines().next().unwrap_or(""));
            let _ = writeln!(out, "  built with:  {first}");
        }
        None => out.push_str("  built with:  unknown\n"),
    }
    out.push_str(&also_lines);
    out
}

/// Read-only and network-bound like `checkupdate`: manifest snapshot
/// under a shared lock, queries unlocked; each name independent, exit
/// code says whether everything was found.
fn cmd_info(prefix: &Path, crates: &[String], versions: bool) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let manifest = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    // The other prefix, read once for the whole batch — lockless by
    // prefixes' own charter: an annotation must never wait behind
    // someone's ten-minute build.
    let also = prefixes::also_installed(prefix);
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
                    describe_info(
                        name,
                        &releases,
                        manifest.crates.get(*name),
                        also.get(*name).map_or(&[][..], Vec::as_slice)
                    )
                );
                if versions {
                    print!("{}", describe_versions(&releases));
                }
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
        // The CLI's own gate: this runs on a real terminal — directly or
        // after a handoff — so a wait for someone else's lock should say
        // so, and a first lock file under a protected prefix may ask for
        // a password interactively.
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
            .crates
            .remove(name)
            .with_context(|| format!("`{name}` is not installed under {}", prefix.display()))?
    };
    // Same gate as install: a downgrade of a pinned crate would re-pin
    // it to another version — a re-interpretation of the standing
    // statement, not its preservation. The pin comes off explicitly.
    if entry.pinned {
        bail!("pinned: {name} {}", unpin_hint(prefix, name));
    }
    let current = Version::parse(&entry.version)
        .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
    let releases = index::releases(name)?.ok_or_else(|| index::not_found(name))?;
    let candidates = index::downgrade_candidates(&releases, &current);
    if candidates.is_empty() {
        println!("{name} {current} is installed; no older version to go back to");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        match pasteable_prefix(prefix) {
            Some(arg) => bail!(
                "downgrade asks which version to install; without a terminal, \
                 use `cargo lbin install {name}@VERSION {arg}`"
            ),
            None => bail!(
                "downgrade asks which version to install; without a terminal, \
                 use lbin's `install` operation with {name}@VERSION for this installation"
            ),
        }
    }
    println!("{name} {current} is installed; older versions on crates.io:");
    let shown = &candidates[..candidates.len().min(DOWNGRADE_CHOICES)];
    for (i, v) in shown.iter().enumerate() {
        println!("  {}) {v}", i + 1);
    }
    if candidates.len() > shown.len() {
        match pasteable_prefix(prefix) {
            Some(arg) => println!(
                "  and {} older; use `cargo lbin install {name}@VERSION {arg}` for one of those",
                candidates.len() - shown.len()
            ),
            None => println!(
                "  and {} older; one of those goes by lbin's `install` operation with {name}@VERSION for this installation",
                candidates.len() - shown.len()
            ),
        }
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
    if fresh.pinned {
        // A pin set while the prompt was open counts as changed state,
        // exactly as it does for `update`: the newer statement wins.
        match pasteable_prefix(prefix) {
            Some(arg) => bail!(
                "`{name}` was pinned while a version was being chosen; \
                 run `cargo lbin unpin {name} {arg}` first"
            ),
            None => bail!("`{name}` was pinned while a version was being chosen; unpin it first"),
        }
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
        ShadowReport::OnCommit,
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
            // Reconciled under the cache lock with anything that landed
            // while the queries ran: neither the baseline nor a fresher
            // per-crate fact travels backwards.
            if let Err(e) = cache_dir().and_then(|cache| report.store_full(&cache)) {
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
        refuse_pinned(prefix, &snapshot, crates)?;
        targets
    };
    for name in &skipped_pinned {
        println!(
            "{name} {} [pinned, skipped]",
            snapshot.crates[*name].version
        );
    }
    let checked = check_versions(
        snapshot
            .crates
            .iter()
            .filter(|(name, _)| targets.contains(name.as_str())),
        || false,
    )?
    .expect("a `|| false` token never cancels");
    // What the lookup learned outlives what this run does with it,
    // each fact stamped at its own answer inside check_versions.
    record_knowledge(prefix, checked.clone());
    let outdated: Vec<Checked> = checked.into_iter().filter(Checked::is_outdated).collect();
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
                    ShadowReport::OnCommit,
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
    built_with_rustc: Option<String>,
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
        built_with_rustc: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            version: Version::parse(version)
                .with_context(|| format!("`{name}` has an unparseable version `{version}`"))?,
            bins,
            built_with_rustc,
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
            built_with_rustc,
        } = entry;
        Ok(Self {
            version: Version::parse(version)
                .with_context(|| format!("`{name}` has an unparseable version `{version}`"))?,
            bins: bins.clone(),
            locked: *locked,
            pinned: *pinned,
            built_with_rustc: built_with_rustc.clone(),
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
            built_with_rustc,
        } = entry;
        Version::parse(version).is_ok_and(|v| v == self.version)
            && *bins == self.bins
            && *locked == self.locked
            && *pinned == self.pinned
            // A same-version rebuild under a new toolchain changes the
            // bytes while changing nothing else on this list — the
            // testimony is what notices it.
            && *built_with_rustc == self.built_with_rustc
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
    // The same heads-up the TUI gives, and for the same reason: where
    // the destination needs no password the privileged half is the
    // retirement, which happens after each build. Decided once for the
    // batch; emitted before each build, because that is where the wait
    // it warns about begins.
    let late_escalation = late_escalation_certain(
        &placement_needs_privilege(privileged::Policy::for_prefix(prefix), prefix),
        &placement_needs_privilege(privileged::Policy::for_prefix(to), to),
    );
    for (i, (name, snap)) in snapshots.iter().enumerate() {
        println!("[{}/{total}] {name}", i + 1);
        if late_escalation {
            eprintln!("warning: {}", late_escalation_note(name, prefix));
        }
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
/// nothing has committed until this returns. Returns the version the
/// destination committed and the binaries it committed with it — the
/// latter is the destination's own fact, which the source snapshot
/// cannot supply for an unpinned migration.
fn rebuild_at_destination(
    source: &Path,
    dest: &Path,
    cache: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<(Version, Vec<String>)> {
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
            // Phase B decides what stands on PATH; the report waits for it.
            ShadowReport::Deferred,
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
            ShadowReport::Deferred,
            &mut Frontend::Captured {
                on_line: &mut **on_line,
                before_placement: &mut **before_placement,
                control,
                checkpoint: Some(&mut checkpoint),
            },
        )?,
    };
    // What the destination actually has, not what the source had: an
    // unpinned migration installs the latest version, and a version can
    // add or drop binaries. The shadow report that follows Phase B asks
    // about these names.
    let bins = dest_manifest
        .crates
        .get(name)
        .with_context(|| format!("`{name}` vanished from {} after its commit", dest.display()))?
        .bins
        .clone();
    Ok((installed, bins))
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
/// What stands on `PATH` for a migrated crate, asked once the
/// retirement has answered.
///
/// A plain scan of the real state, so every outcome tells the truth
/// without predicting any of it: the source retired and a distro copy
/// now first — that copy is named; the retirement refused or failed
/// and the source still there — the source is named; nothing left to
/// shadow — nothing is said.
///
/// `<dest>/bin is not on PATH` is asked separately because it is not a
/// property of any shadowing file: a binary in a directory `PATH` does
/// not list is unreachable by bare name whether or not something else
/// carries the name. It is added only when no shadow note was
/// produced, since a note already ends with that verdict when it
/// applies.
fn migration_shadow_notes(dest: &Path, bins: &[String]) -> Vec<String> {
    let notes = shadow_notes(dest, bins);
    if !notes.is_empty() || bins.is_empty() {
        return notes;
    }
    let (Some(path_var), Ok(cwd)) = (std::env::var_os("PATH"), std::env::current_dir()) else {
        return notes;
    };
    let dest_bin = dest.join("bin");
    if shadow::prefix_on_path(&path_var, &dest_bin, &cwd) {
        return notes;
    }
    vec![text::sanitize(&format!(
        "{} is not on PATH",
        dest_bin.display()
    ))]
}

/// The deferred report, spoken through whichever channel the migration
/// is using.
fn report_migration_shadows(dest: &Path, bins: &[String], frontend: &mut MigrateFrontend<'_>) {
    for note in migration_shadow_notes(dest, bins) {
        let line = format!("warning: {note}");
        match frontend {
            MigrateFrontend::Terminal => eprintln!("{line}"),
            #[cfg(not(feature = "tui"))]
            MigrateFrontend::Never(_) => unreachable!(),
            #[cfg(feature = "tui")]
            MigrateFrontend::Captured { on_line, .. } => on_line(LineKind::Warning, &line),
        }
    }
}

fn migrate_one(
    source: &Path,
    dest: &Path,
    cache: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<MigrateOutcome> {
    let (installed, installed_bins) =
        rebuild_at_destination(source, dest, cache, name, snap, frontend)?;

    // Phase B: the source, under its exclusive lock. From here nothing
    // may surface as a plain error — the destination has committed, and
    // every failure is an *incomplete migration* whose message leads with
    // that (a bare "failed" invites a re-run the already-installed
    // refusal bounces). Outcomes carry data; the caller owns the words —
    // the privilege probes included: failing while *asking* is no
    // exception. The reason names the version the destination committed:
    // two installations stand, and "what is actually over there" is the
    // fact the person cleans up by.
    let retirement = retire_with_frontend(source, name, snap, frontend);
    // Every branch below describes a finished migration, so the state
    // is real now: whatever retirement did or refused to do is on disk.
    report_migration_shadows(dest, &installed_bins, frontend);
    match retirement {
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
        MigrateFrontend::Terminal => {
            let policy = privileged::Policy::for_prefix(source);
            // Placement says what its password is for; so does this. The
            // preauthorization is also where a refusal is a plain
            // failure rather than a half-done retirement discovered
            // three sudo calls later.
            privileged::preauthorize(
                source,
                placement_needs_privilege(policy, source)?,
                privileged::AuthPurpose::Retirement,
            )?;
            retire_source(source, name, snap, policy, &mut |l| eprintln!("{l}"))
        }
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
    use crate::test_support::{manifest_with, seeded_prefix};
    use crate::verify::{clean_cache, scan_stale_stages, verify_entries};

    /// The other prefix's copy is named whether or not this prefix has
    /// one: skew is the point when both exist, and "not here, but over
    /// there" answers exactly the question asked.
    #[test]
    fn info_names_the_other_prefixes_copy() {
        let rel = index::Release {
            version: Version::parse("1.2.0").unwrap(),
            yanked: false,
        };
        let releases = [rel];
        let also = [prefixes::AlsoIn {
            prefix: PathBuf::from("/usr/local"),
            version: "1.1.0".to_owned(),
        }];

        // Installed here too: the entry block first, the skew after it.
        let m = manifest_with(&["foo"]);
        let out = describe_info("foo", &releases, m.crates.get("foo"), &also);
        assert!(out.contains("binaries:    foo"), "{out}");
        assert!(out.contains("also in:     /usr/local @1.1.0"), "{out}");
        assert!(
            out.find("binaries:").unwrap() < out.find("also in:").unwrap(),
            "this prefix's facts first, the other prefix's after: {out}"
        );

        // Not installed here: the foreign copy is still the answer.
        let out = describe_info("foo", &releases, None, &also);
        assert!(out.contains("installed:   no"), "{out}");
        assert!(out.contains("also in:     /usr/local @1.1.0"), "{out}");

        // Nothing foreign: no line, not an empty one.
        let out = describe_info("foo", &releases, None, &[]);
        assert!(!out.contains("also in:"), "{out}");

        // A prefix is environment-borne: control characters in it are
        // neutralized, the same policy prefixes::describe applies.
        let hostile = [prefixes::AlsoIn {
            prefix: PathBuf::from("/usr/\x1b[31mlocal"),
            version: "1.1.0".to_owned(),
        }];
        let out = describe_info("foo", &releases, None, &hostile);
        assert!(
            !out.contains('\x1b'),
            "the escape byte must not reach the terminal: {out:?}"
        );
        assert!(out.contains("also in:"), "{out}");
    }

    /// The history section: descending semver order — not reverse
    /// chronology; the index's publication order is neither — with
    /// everything listed: yanked flagged, pre-releases included,
    /// because this is history, and eligibility is the `installed`
    /// line's business.
    #[test]
    fn versions_section_lists_history_in_descending_semver_with_yanked_marks() {
        let rel = |v: &str, yanked: bool| index::Release {
            version: Version::parse(v).unwrap(),
            yanked,
        };
        // Deliberately out of order and mixed: whatever order came in,
        // descending semver comes out.
        let releases = [
            rel("2.2.0", true),
            rel("2.4.0", false),
            rel("2.3.0", false),
            rel("2.5.0-rc.1", false),
            rel("2.3.1", false),
        ];
        let out = describe_versions(&releases);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            [
                "  versions:",
                "    2.5.0-rc.1",
                "    2.4.0",
                "    2.3.1",
                "    2.3.0",
                "    2.2.0 [yanked]",
            ],
            "{out}"
        );
    }

    /// The duplicate-install warning fires exactly when this install
    /// would create the second copy: the crate is absent here and
    /// managed over there. Both prefixes are environment-borne, so the
    /// lines are sanitized; the migrate hint is pasteable as printed.
    #[test]
    fn duplicate_install_warns_only_before_the_second_copy() {
        use std::collections::BTreeMap;
        let prefix = PathBuf::from("/home/u/.local");
        let mut also: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        also.insert(
            "foo".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "1.2.3".to_owned(),
            }],
        );

        // Absent here, managed there: the plan's three lines, verbatim
        // in shape, with a genuinely pasteable migrate hint —
        // flag=value form, like every other pasteable hint.
        let empty = Manifest::default();
        let lines = duplicate_install_warnings_from(&also, &prefix, &empty, std::iter::once("foo"));
        assert_eq!(
            lines,
            [
                "warning: `foo` is already managed under /usr/local @1.2.3",
                "this will install another copy under /home/u/.local",
                "use `cargo lbin migrate foo --prefix=/usr/local --to=/home/u/.local` \
                 if you intended to move it",
            ],
            "{lines:?}"
        );

        // Pasteable means shell-safe, not merely terminal-safe: a space
        // stays one argument, an apostrophe survives its own quoting,
        // and $() stays a directory name instead of a command.
        let spaced = PathBuf::from("/tmp/my lbin");
        let lines = duplicate_install_warnings_from(&also, &spaced, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains("--to='/tmp/my lbin'"),
            "a space is quoted into one argument: {lines:?}"
        );
        let hostile_shell = PathBuf::from("/tmp/$(touch owned)");
        let lines =
            duplicate_install_warnings_from(&also, &hostile_shell, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains("--to='/tmp/$(touch owned)'"),
            "command substitution is neutralized by quoting: {lines:?}"
        );
        let quoted = PathBuf::from("/tmp/o'brien");
        let lines = duplicate_install_warnings_from(&also, &quoted, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains(r"--to='/tmp/o'\''brien'"),
            "an apostrophe survives its own quoting: {lines:?}"
        );

        // Already installed here too: nothing new is created, verify
        // owns the standing duplication — no warning.
        let local = manifest_with(&["foo"]);
        assert!(
            duplicate_install_warnings_from(&also, &prefix, &local, std::iter::once("foo"))
                .is_empty(),
            "a reinstall creates no second copy"
        );

        // No foreign copy: silence.
        assert_eq!(
            duplicate_install_warnings_from(&also, &prefix, &empty, std::iter::once("bar")),
            Vec::<String>::new(),
            "nothing managed elsewhere, nothing to warn about"
        );

        // A control character has no honest shell spelling: the warning
        // stands, the terminal stays protected (the 0.7.0 rule), but no
        // exact command is printed — a sanitize-laundered path would be
        // safe to paste and wrong to run. Same rule as the verify
        // reinstall hint.
        let mut hostile: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        hostile.insert(
            "foo".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/\x1b[31mlocal"),
                version: "1.2.3".to_owned(),
            }],
        );
        let lines =
            duplicate_install_warnings_from(&hostile, &prefix, &empty, std::iter::once("foo"));
        assert_eq!(lines.len(), 3, "the warning itself stands: {lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\x1b')), "{lines:?}");
        assert_eq!(
            lines[2],
            "use `cargo lbin migrate` with explicit --prefix/--to \
             if you intended to move it",
            "no honest spelling: the mechanism is named, worded so nobody \
             mistakes it for a pasteable hint: {lines:?}"
        );
    }

    /// The decision matrix from the Phase V plan, at the facts level —
    /// the single authority both surfaces render from, so parity is by
    /// construction and this matrix is the parity test. The renderer's
    /// own shape (exactly three lines per fact, order preserved) is
    /// pinned alongside: a multi-crate batch must not lose a duplicate
    /// between deciding and wording.
    #[test]
    fn cross_prefix_duplicate_decision_matrix() {
        use std::collections::BTreeMap;
        let here = PathBuf::from("/tmp/custom prefix");
        let mut also: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        // `both` is managed under BOTH known others — legal exactly when
        // the current prefix is a custom one — and `there` under one.
        also.insert(
            "both".to_owned(),
            vec![
                prefixes::AlsoIn {
                    prefix: PathBuf::from("/usr/local"),
                    version: "1.0.0".to_owned(),
                },
                prefixes::AlsoIn {
                    prefix: PathBuf::from("/home/u/.local"),
                    version: "1.1.0".to_owned(),
                },
            ],
        );
        also.insert(
            "there".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "2.0.0".to_owned(),
            }],
        );
        let local = manifest_with(&["here-only", "there"]);

        // Only local: no duplicate to create.
        let d = cross_prefix_duplicates_from(&also, &here, &local, std::iter::once("here-only"));
        assert!(d.is_empty(), "local-only never warns");
        // Local AND foreign: the duplication stands already — silence.
        let d = cross_prefix_duplicates_from(&also, &here, &local, std::iter::once("there"));
        assert!(
            d.is_empty(),
            "a standing duplicate is verify's, not install's"
        );
        // Foreign only: one entry, with an honest quoted hint — the
        // custom prefix needs its space quoted.
        let empty = Manifest::default();
        let d = cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("there"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].other_version, "2.0.0");
        assert_eq!(
            d[0].migrate_hint.as_deref(),
            Some("cargo lbin migrate there --prefix=/usr/local --to='/tmp/custom prefix'"),
            "{:?}",
            d[0].migrate_hint
        );
        // Managed under both known others: one block per foreign copy —
        // the docs' promise, pinned.
        let d = cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("both"));
        assert_eq!(d.len(), 2, "one entry per foreign managed copy");
        assert_eq!(d[0].other_version, "1.0.0");
        assert_eq!(d[1].other_version, "1.1.0");
        // Unknown everywhere: silence.
        assert!(
            cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("nowhere"))
                .is_empty()
        );

        // The renderer: three lines per fact, order preserved — the
        // multi-copy batch loses nothing between deciding and wording.
        let lines = duplicate_install_warning_lines(&d_all(&also, &here, &empty), &here);
        assert_eq!(lines.len(), 3 * 3, "{lines:?}");
        assert!(lines[0].contains("`both`") && lines[3].contains("`both`"));
        assert!(lines[6].contains("`there`"));
    }

    /// The matrix's batch, in input order: both requested names.
    fn d_all(
        also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
        here: &Path,
        manifest: &Manifest,
    ) -> Vec<CrossPrefixDuplicate> {
        cross_prefix_duplicates_from(also, here, manifest, ["both", "there"].into_iter())
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
        assert!(Cli::try_parse_from(["cargo-lbin", "info", "foo", "--versions"]).is_ok());
        // Per command, like --json: accepted only where it does something.
        assert!(Cli::try_parse_from(["cargo-lbin", "list", "--versions"]).is_err());
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

        let out = describe_info("foo", &releases, installed, &[]);
        assert!(out.contains("latest:      1.2.0"), "{out}");
        assert!(out.contains("pre-release: 2.0.0-rc.1"), "{out}");
        assert!(out.contains("releases:    4 (1 yanked)"), "{out}");
        // Installed 1.0.0 is stable: the rc is not offered, 1.2.0 is.
        assert!(
            out.contains("installed:   1.0.0 (update available: 1.2.0)"),
            "{out}"
        );
        // The entry block: the manifest's own facts, in full.
        assert!(out.contains("pinned:      no"), "{out}");
        assert!(out.contains("locked:      no"), "{out}");
        assert!(out.contains("binaries:    foo"), "{out}");

        let out = describe_info("foo", &releases, None, &[]);
        assert!(out.contains("installed:   no"), "{out}");
        assert!(
            !out.contains("pinned:") && !out.contains("binaries:"),
            "no entry, no entry block: {out}"
        );

        // Pinned, locked, several binaries: every field speaks.
        let mut m = manifest_with(&["foo"]);
        {
            let e = m.crates.get_mut("foo").unwrap();
            e.pinned = true;
            e.locked = true;
            e.bins = vec!["foo".into(), "fooctl".into()];
        }
        let out = describe_info("foo", &releases, m.crates.get("foo"), &[]);
        assert!(out.contains("pinned:      yes"), "{out}");
        assert!(out.contains("locked:      yes"), "{out}");
        assert!(out.contains("binaries:    foo, fooctl"), "{out}");

        // Installed at the newest stable: up to date, rc still not offered.
        let mut m = manifest_with(&["foo"]);
        m.crates.get_mut("foo").unwrap().version = "1.2.0".to_owned();
        let out = describe_info("foo", &releases, m.crates.get("foo"), &[]);
        assert!(out.contains("installed:   1.2.0 (up to date)"), "{out}");

        // History and eligibility diverge: the newest stable is yanked, so
        // it is shown flagged, while the installed 1.0.0 has nowhere to go.
        let releases = [rel("1.0.0", false), rel("1.1.0", true)];
        let out = describe_info("foo", &releases, installed, &[]);
        assert!(out.contains("latest:      1.1.0 [yanked]"), "{out}");
        assert!(out.contains("installed:   1.0.0 (up to date)"), "{out}");

        // Everything yanked: `checkupdate` would refuse this crate, and
        // `info` must not call it "up to date".
        let releases = [rel("1.0.0", true)];
        let out = describe_info("foo", &releases, installed, &[]);
        assert!(out.contains("latest:      1.0.0 [yanked]"), "{out}");
        assert!(
            out.contains("installed:   1.0.0 (no non-yanked releases)"),
            "{out}"
        );
        // The regression that justified removing the early return: a
        // publication history with nothing live is not a reason to
        // withhold the local entry.
        assert!(
            out.contains("pinned:") && out.contains("locked:") && out.contains("binaries:    foo"),
            "an all-yanked history still shows the entry block: {out}"
        );
    }

    #[test]
    fn pinned_crates_are_refused_by_name_all_at_once() {
        let mut m = manifest_with(&["bat", "fd", "ripgrep"]);
        m.crates.get_mut("bat").unwrap().pinned = true;
        m.crates.get_mut("fd").unwrap().pinned = true;
        let err = refuse_pinned(
            Path::new("/usr/local"),
            &m,
            &["ripgrep".into(), "bat".into(), "fd".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bat") && err.contains("fd"), "{err}");
        assert!(!err.contains("ripgrep"), "{err}");
        // The suggested command is complete and runnable as printed —
        // scope included, so it is true wherever it is pasted.
        assert!(
            err.contains("`cargo lbin unpin bat fd --prefix=/usr/local`"),
            "{err}"
        );
        // Unpinned selection, and names not in the manifest, pass: the
        // latter are `select_targets`' problem, not this check's.
        assert!(
            refuse_pinned(
                Path::new("/usr/local"),
                &m,
                &["ripgrep".into(), "nope".into()]
            )
            .is_ok()
        );
    }

    #[test]
    fn diagonal_is_version_and_policy_only() {
        let cur = Version::parse("1.4.2").unwrap();
        let newer = Version::parse("1.5.0").unwrap();
        // Same version, same policy: the artifact specification stands.
        assert!(is_install_diagonal(false, &cur, &cur, false));
        assert!(is_install_diagonal(true, &cur, &cur, true));
        // Either axis moving is a real change and builds.
        assert!(!is_install_diagonal(false, &cur, &cur, true));
        assert!(!is_install_diagonal(true, &cur, &cur, false));
        assert!(!is_install_diagonal(false, &cur, &newer, false));
    }

    #[test]
    fn gates_refuse_with_the_typed_category() {
        let cur = Version::parse("1.0.0").unwrap();
        // The diagonal's refusal carries the category...
        let err = refuse_on_diagonal("foo", false, &cur, &cur, false, false, None).unwrap_err();
        assert!(err.downcast_ref::<ContractRefusal>().is_some());
        // ...and survives an anyhow context wrapper, so call sites may
        // annotate without erasing it.
        assert!(
            err.context("while installing")
                .downcast_ref::<ContractRefusal>()
                .is_some()
        );
        // The pin gate speaks the same type.
        let mut m = manifest_with(&["bat"]);
        m.crates.get_mut("bat").unwrap().pinned = true;
        let err = refuse_pinned(Path::new("/usr/local"), &m, &["bat".into()]).unwrap_err();
        assert!(err.downcast_ref::<ContractRefusal>().is_some());
        // An execution error is not a refusal: the category is opt-in
        // at the gate, never inferred from wording.
        assert!(
            anyhow::anyhow!("build exploded")
                .downcast_ref::<ContractRefusal>()
                .is_none()
        );
    }

    #[test]
    fn diagonal_refusals_name_the_right_verbs() {
        let cur = Version::parse("1.4.2").unwrap();
        // `@current-version`: two intents can hide behind it, and the
        // fork names both verbs.
        let scope = Some("--prefix=/usr/local");
        let err = refuse_on_diagonal("foo", false, &cur, &cur, true, false, scope)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("cargo lbin pin foo --prefix=/usr/local"),
            "{err}"
        );
        assert!(
            err.contains("cargo lbin install --reinstall foo --prefix=/usr/local"),
            "{err}"
        );
        // Bare on the newest: a rebuild is the only remaining effect.
        let err = refuse_on_diagonal("foo", false, &cur, &cur, false, false, scope)
            .unwrap_err()
            .to_string();
        assert!(err.contains("newest release"));
        assert!(
            err.contains("cargo lbin install --reinstall foo --prefix=/usr/local"),
            "{err}"
        );
        assert!(!err.contains("cargo lbin pin"));
        // No honest spelling for the prefix: verbs, not a command that
        // is safe to paste and wrong to run.
        let err = refuse_on_diagonal("foo", false, &cur, &cur, false, false, None)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("cargo lbin"), "{err}");
        // `--locked` flipping in either direction is a policy change
        // and builds; so does a different version.
        assert!(refuse_on_diagonal("foo", true, &cur, &cur, true, false, scope).is_ok());
        assert!(refuse_on_diagonal("foo", false, &cur, &cur, false, true, scope).is_ok());
        let newer = Version::parse("1.5.0").unwrap();
        assert!(refuse_on_diagonal("foo", false, &cur, &newer, true, false, scope).is_ok());
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

    /// The built-with line speaks cargo's record at its first line,
    /// sanitized — subprocess text never steers the terminal — and
    /// `None` says exactly `unknown`, its cause deliberately unguessed.
    #[test]
    fn describe_info_speaks_the_rustc_record_sanitized_or_unknown() {
        let releases = vec![index::Release {
            version: Version::parse("0.1.0").unwrap(),
            yanked: false,
        }];
        let mut entry = Entry {
            version: "0.1.0".to_owned(),
            bins: vec!["okcrate".to_owned()],
            locked: false,
            pinned: false,
            built_with_rustc: Some(
                "rustc 1.90.0 (abc 2025-01-01)\u{1b}[31m\nrelease: 1.90.0\n".to_owned(),
            ),
        };
        let shown = describe_info("okcrate", &releases, Some(&entry), &[]);
        assert!(
            shown.contains("built with:  rustc 1.90.0 (abc 2025-01-01)"),
            "the first line is spoken: {shown}"
        );
        assert!(
            !shown.contains('\u{1b}'),
            "and never a control byte from the record: {shown}"
        );

        entry.built_with_rustc = None;
        let shown = describe_info("okcrate", &releases, Some(&entry), &[]);
        assert!(
            shown.contains("built with:  unknown\n"),
            "None says unknown and no more: {shown}"
        );
    }

    #[test]
    fn migration_snapshot_protects_every_entry_field() {
        let base = Entry {
            version: "1.2.3".into(),
            bins: vec!["foo".into()],
            locked: true,
            pinned: true,
            built_with_rustc: None,
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

        let mut changed = base.clone();
        changed.built_with_rustc = Some("rustc 1.98.0 (feedface 2026-06-01)\n".into());
        assert!(
            !snap.still_matches(&changed),
            "the rustc provenance is protected"
        );
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

    /// `staging_fake` plus cargo's `rustc` record: `rustc_json` is the
    /// already-JSON-encoded report string.
    fn provenance_fake(root: &Path, name: &str, rustc_json: &str) -> PathBuf {
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
                 printf '%s' '{{\"installs\":{{\"{name} 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{{\"bins\":[\"{name}\"],\"rustc\":{rustc_json}}}}}}}' > \"$4/.crates2.json\"\n\
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

    /// A fake cargo whose build carries a second binary — the shape an
    /// unpinned migration meets when the newer version ships more than
    /// the source had.
    #[cfg(feature = "tui")]
    fn two_bin_fake(root: &Path, name: &str, extra: &str) -> PathBuf {
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
                 for b in {name} {extra}; do\n\
                 printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/$b\"\n\
                 chmod 755 \"$4/bin/$b\"\n\
                 done\n\
                 printf '%s' \"{{\\\"installs\\\":{{\\\"{name} $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{{\\\"bins\\\":[\\\"{name}\\\",\\\"{extra}\\\"]}}}}}}\" > \"$4/.crates2.json\"\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The report asks about the destination's binaries, which for an
    /// unpinned migration are the *new* version's — a set the source
    /// snapshot cannot describe. A binary the newer version adds must
    /// be scanned, and one it drops must not be: the report covers what
    /// was installed, not what used to be.
    // The captured frontend is the TUI's; without it there is no such
    // migration to report about.
    #[cfg(feature = "tui")]
    #[test]
    fn a_migrations_report_scans_the_binaries_the_destination_committed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-report-bins");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", false, false);
        let dest = root.join("dest");
        let distro_bin = root.join("distro/bin");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        fs::create_dir_all(&distro_bin).unwrap();
        // The distro ships only the helper — the name the source
        // version never had, and the one the new version adds.
        let helper = distro_bin.join("okcrate-helper");
        fs::write(&helper, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake =
            crate::stage::FakeCargo::install(&two_bin_fake(&root, "okcrate", "okcrate-helper"));

        let snap = MigrationSnapshot::capture(
            "okcrate",
            &Manifest::load(&source).unwrap().crates["okcrate"],
        )
        .unwrap();
        assert_eq!(snap.bins, vec!["okcrate".to_owned()], "the source's set");

        let said = std::cell::RefCell::new(Vec::new());
        let control = BuildControl::new();
        let outcome = {
            let mut on_line = |k: LineKind, l: &str| {
                if matches!(k, LineKind::Warning) {
                    said.borrow_mut().push(l.to_owned());
                }
            };
            let mut before_placement = |_: &Path| Ok(());
            // No extra gate here: `FakeCargo` already holds the one that
            // serializes spawn- and PATH-sensitive tests, and taking it
            // twice would deadlock. The system PATH stays on the end —
            // the fake cargo shells out to mkdir and chmod.
            let old_path = std::env::var_os("PATH");
            let scan_path = format!(
                "{}:{}:{}",
                distro_bin.display(),
                dest.join("bin").display(),
                old_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );
            // SAFETY: serialized by the FakeCargo lock this test holds.
            unsafe { std::env::set_var("PATH", scan_path) };
            let outcome = migrate_one(
                &source,
                &dest,
                &root.join("cache"),
                "okcrate",
                &snap,
                &mut MigrateFrontend::Captured {
                    on_line: &mut on_line,
                    before_placement: &mut before_placement,
                    control: &control,
                },
            );
            match old_path {
                // SAFETY: as above.
                Some(v) => unsafe { std::env::set_var("PATH", v) },
                None => unsafe { std::env::remove_var("PATH") },
            }
            outcome
        }
        .unwrap();
        assert!(
            matches!(outcome, MigrateOutcome::Moved { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            Manifest::load(&dest).unwrap().crates["okcrate"].bins,
            vec!["okcrate".to_owned(), "okcrate-helper".to_owned()],
            "the destination committed both binaries"
        );
        let said = said.into_inner();
        assert!(
            said.iter()
                .any(|l| l.contains("okcrate-helper") && l.contains(&helper.display().to_string())),
            "a binary the new version added is scanned too: {said:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The deferred report, in the four states Phase B can leave behind.
    /// Each is the real filesystem and the real PATH at the moment the
    /// migration finishes — nothing is predicted, so nothing can be
    /// predicted wrongly.
    #[test]
    fn a_migrations_shadow_report_describes_the_state_phase_b_left() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-shadow-after-retire");
        let _ = fs::remove_dir_all(&root);
        let source_bin = root.join("source/bin");
        let distro_bin = root.join("distro/bin");
        let dest = root.join("dest");
        for d in [&source_bin, &distro_bin, &dest.join("bin")] {
            fs::create_dir_all(d).unwrap();
        }
        let put = |dir: &Path| {
            let f = dir.join("tool");
            fs::write(&f, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
        };
        let bins = vec!["tool".to_owned()];
        let notes = |path: String| {
            // PATH is process-wide: the same gate the spawn-sensitive
            // tests use keeps this from racing them.
            let _serial = crate::stage::no_spawned_children();
            let old = std::env::var_os("PATH");
            // SAFETY: serialized by the gate above.
            unsafe { std::env::set_var("PATH", path) };
            let notes = migration_shadow_notes(&dest, &bins);
            match old {
                // SAFETY: as above.
                Some(v) => unsafe { std::env::set_var("PATH", v) },
                None => unsafe { std::env::remove_var("PATH") },
            }
            notes
        };
        let with_dest = format!(
            "{}:{}:{}",
            source_bin.display(),
            distro_bin.display(),
            dest.join("bin").display()
        );

        // Retired, and a copy further down PATH takes over the name: the
        // report names *that* copy, which the pre-retirement scan never
        // even looked at.
        put(&distro_bin);
        let after = notes(with_dest.clone());
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(
            after[0].contains(&distro_bin.join("tool").display().to_string()),
            "the copy that actually shadows now: {after:?}"
        );

        // Incomplete: retirement refused or failed, so the source stands
        // — and is reported, because it is still there.
        put(&source_bin);
        let incomplete = notes(with_dest.clone());
        assert_eq!(incomplete.len(), 1, "{incomplete:?}");
        assert!(
            incomplete[0].contains(&source_bin.join("tool").display().to_string()),
            "a source that survived is news: {incomplete:?}"
        );

        // Retired, nothing replaces it, and the destination is on PATH:
        // there is nothing true left to say.
        fs::remove_file(source_bin.join("tool")).unwrap();
        fs::remove_file(distro_bin.join("tool")).unwrap();
        assert!(notes(with_dest).is_empty(), "silence is the honest answer");

        // Same, but the destination is not on PATH: the fact that is
        // about the destination rather than any shadow stands alone.
        let without_dest = format!("{}:{}", source_bin.display(), distro_bin.display());
        let unreachable = notes(without_dest);
        assert_eq!(
            unreachable,
            vec![format!("{} is not on PATH", dest.join("bin").display())],
            "the binary is unreachable by name, and that is said plainly"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Unknown is not "no". A destination whose privilege cannot be
    /// judged may refuse the migration before any build starts, so
    /// promising a password after that build would be a promise about
    /// work that never happens. Both probes must answer, and answer
    /// this way.
    #[test]
    fn an_unjudgeable_prefix_announces_nothing() {
        let err = || Err(anyhow::anyhow!("only /usr/local may escalate"));
        assert!(
            late_escalation_certain(&Ok(true), &Ok(false)),
            "the case it is for"
        );
        assert!(
            !late_escalation_certain(&Ok(true), &err()),
            "a destination nobody can judge is not a destination that needs nothing"
        );
        assert!(
            !late_escalation_certain(&err(), &Ok(false)),
            "nor is the source"
        );
        assert!(!late_escalation_certain(&err(), &err()));
        // And the ordinary negatives.
        assert!(!late_escalation_certain(&Ok(false), &Ok(false)));
        assert!(
            !late_escalation_certain(&Ok(true), &Ok(true)),
            "a destination that escalates has already asked"
        );
    }

    /// The sentence promises a privileged operation, not a password
    /// prompt: sudo may have a fresh timestamp, and predicting that
    /// from here would be a promise the tool cannot keep.
    #[test]
    fn the_late_escalation_note_promises_privilege_not_a_prompt() {
        let note = late_escalation_note("scx_truther", Path::new("/usr/local"));
        assert_eq!(
            note,
            "retiring `scx_truther` from /usr/local needs sudo after the build; \
             a password may be requested then"
        );
        assert!(!note.contains("will ask"), "{note}");
        let hostile = PathBuf::from("/usr/\x1b[31mlocal");
        assert!(!late_escalation_note("foo", &hostile).contains('\x1b'));
    }

    /// A prefix is the parent of `bin`, so one ending in `bin` is
    /// almost certainly a slip — legal, occasionally intended, and
    /// worth saying once rather than letting it surface later as a PATH
    /// complaint about `.../bin/bin`.
    #[test]
    fn a_prefix_ending_in_bin_is_named_as_probably_one_level_too_deep() {
        let note = bin_dir_prefix_note(Path::new("/home/u/.local/bin"))
            .expect("a bin-suffixed prefix is worth a word");
        assert!(note.contains("/home/u/.local/bin/bin"), "{note}");
        assert!(note.contains("did you mean /home/u/.local?"), "{note}");

        // A bare relative `bin`: the parent is the current directory,
        // and it is spelled so rather than left blank.
        let relative = bin_dir_prefix_note(Path::new("bin")).expect("still worth a word");
        assert!(relative.contains("binaries go to bin/bin"), "{relative}");
        assert!(relative.contains("did you mean .?"), "{relative}");

        // Ordinary prefixes say nothing, including ones that merely
        // contain the word.
        assert!(bin_dir_prefix_note(Path::new("/usr/local")).is_none());
        assert!(bin_dir_prefix_note(Path::new("/opt/binutils")).is_none());
        assert!(bin_dir_prefix_note(Path::new("/")).is_none());

        // Environment-borne, so sanitized like every other external
        // string that reaches a terminal.
        let hostile = PathBuf::from("/usr/\x1b[31mlocal/bin");
        let note = bin_dir_prefix_note(&hostile).unwrap();
        assert!(!note.contains('\x1b'), "{note}");
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
        assert!(
            entry.built_with_rustc.is_none(),
            "a stage without the record reads as unknown"
        );
        assert!(entry.locked, "the --locked flag travels with the crate");
        assert!(dest.join("bin/okcrate").is_file(), "rebuilt and placed");

        let src_manifest = Manifest::load(&source).unwrap();
        assert!(!src_manifest.crates.contains_key("okcrate"), "retired");
        assert!(!source.join("bin/okcrate").exists(), "binary removed");
        let _ = fs::remove_dir_all(&root);
    }

    /// The destination records the destination build's own testimony —
    /// provenance is never copied from the source entry, and it lands
    /// byte-for-byte.
    #[test]
    fn migrate_files_the_destination_builds_rustc_not_the_sources() {
        let root = std::env::temp_dir().join("cargo-lbin-test-migrate-provenance");
        let _ = fs::remove_dir_all(&root);
        let source = seeded_prefix(&root, "source", "okcrate", true, true);
        let seeded = "rustc 0.0.1 (seeded 2020-01-01)\n";
        let mut m = Manifest::load(&source).unwrap();
        m.crates.get_mut("okcrate").unwrap().built_with_rustc = Some(seeded.into());
        m.store(&source).unwrap();
        let dest = root.join("dest");
        fs::create_dir_all(dest.join("bin")).unwrap();
        fs::create_dir_all(dest.join("share/cargo-lbin")).unwrap();
        let report = "rustc 1.90.0 (abc 2025-01-01)\nbinary: rustc\nrelease: 1.90.0\n";
        let _fake = crate::stage::FakeCargo::install(&provenance_fake(
            &root,
            "okcrate",
            &serde_json::to_string(report).unwrap(),
        ));

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
        assert!(matches!(outcome, MigrateOutcome::Moved { .. }));

        let dest_rustc = Manifest::load(&dest).unwrap().crates["okcrate"]
            .built_with_rustc
            .clone();
        assert_eq!(
            dest_rustc.as_deref(),
            Some(report),
            "the destination build's own record, verbatim"
        );
        assert_ne!(dest_rustc.as_deref(), Some(seeded), "never the source's");
        let _ = fs::remove_dir_all(&root);
    }

    /// `pin`/`unpin` move no bytes, so they touch no provenance.
    #[test]
    fn pin_and_unpin_leave_built_with_rustc_untouched() {
        let root = std::env::temp_dir().join("cargo-lbin-test-pin-provenance");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let report = "rustc 1.90.0 (abc 2025-01-01)\nrelease: 1.90.0\n";
        let mut m = Manifest::load(&prefix).unwrap();
        m.crates.get_mut("okcrate").unwrap().built_with_rustc = Some(report.into());
        m.store(&prefix).unwrap();

        let recorded = || {
            Manifest::load(&prefix).unwrap().crates["okcrate"]
                .built_with_rustc
                .clone()
        };
        cmd_set_pinned(&prefix, &["okcrate".into()], true).unwrap();
        assert_eq!(recorded().as_deref(), Some(report), "pin moves no bytes");
        cmd_set_pinned(&prefix, &["okcrate".into()], false).unwrap();
        assert_eq!(recorded().as_deref(), Some(report), "unpin moves no bytes");
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
                built_with_rustc: None,
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

    /// The feature's core claim, end to end through the real producer
    /// path: the lease belongs to the open file description, not to
    /// cargo-lbin. A fake cargo backgrounds a sleeper (which inherits
    /// the descriptor — the exec-survival the `FD_CLOEXEC` clearing
    /// exists for) and exits 1, so the install fails, keeps its run as
    /// forensics, and drops the creator's descriptor on return — the
    /// same closure a process death performs. While the orphan runs,
    /// verify must call the run owned and clean must not touch it; the
    /// moment the orphan exits, released is the verdict and clean may
    /// take the lease and finish. Deadlines watch the lease itself,
    /// not /proc: an unreaped zombie still has a /proc entry, but the
    /// kernel closed its descriptors at exit — the lock is the truth.
    #[test]
    fn a_lease_outlives_its_creator_while_any_inheritor_runs() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-inheritance");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        let cache = root.join("cache");
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();

        let pid_file = root.join("orphan.pid");
        let stop_file = root.join("orphan.stop");
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                // The iteration cap is the orphan's own safety net: a
                // test panicking before the stop file exists must not
                // strand a sleeper on the runner forever.
                "#!/bin/sh\n\
                 sh -c 'echo $$ > \"{pid}\"; n=0; \
                 while [ ! -e \"{stop}\" ] && [ \"$n\" -lt 600 ]; do n=$((n+1)); sleep 0.05; done' &\n\
                 exit 1\n",
                pid = pid_file.display(),
                stop = stop_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = crate::stage::FakeCargo::install(&script);

        let err = install_and_commit(
            &prefix,
            &cache,
            &mut Manifest::default(),
            "ghostcrate",
            None,
            false,
            PinPolicy::Infer,
            ShadowReport::OnCommit,
            &mut Frontend::Terminal,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("failed"),
            "the fake build fails by design: {err:#}"
        );

        // The failed run is kept as forensics, lease file included.
        let runs: Vec<PathBuf> = fs::read_dir(cache.join(crate::stage::RUN_NAMESPACE))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(runs.len(), 1, "one failed run, kept: {runs:?}");
        let run = runs[0].clone();

        // The orphan announced itself; the creator's descriptor is
        // already gone — install_and_commit returned above.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !pid_file.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the orphan never announced itself"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            crate::stage::probe_lease(&run),
            crate::stage::LeaseState::Held,
            "creator dead, inheritor alive: the lease stands"
        );
        assert_eq!(
            scan_stale_stages(&cache).unwrap(),
            Vec::<PathBuf>::new(),
            "an owned run is nobody's debris"
        );
        clean_cache(&cache, false, true, None).unwrap();
        assert!(run.exists(), "clean must not touch an owned run");

        // The orphan exits; its descriptors close with it, reaped or
        // not — the lease is the thing to watch. A fresh deadline: the
        // release gets its full allowance, not whatever the startup
        // wait left over.
        fs::write(&stop_file, b"").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while crate::stage::probe_lease(&run) != crate::stage::LeaseState::Released {
            assert!(
                std::time::Instant::now() < deadline,
                "the lease never released after the last inheritor exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stale = scan_stale_stages(&cache).unwrap();
        assert!(
            stale.contains(&run),
            "with the last inheritor gone, released convicts"
        );
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                clean_cache(&cache, false, true, None).unwrap();
                !run.exists()
            }),
            "clean takes the lease and finishes the job"
        );
        let _ = fs::remove_dir_all(&root);
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
            built_with_rustc: None,
        };
        let name = "anything".to_owned();
        let result = check_versions([(&name, &entry)], || true).unwrap();
        assert!(
            result.is_none(),
            "a cancelled run is an answer, not a report"
        );
    }

    /// `--reinstall` rebuilds the entry, not the newest release — and
    /// gives back the same entry: same version, same pin, same
    /// `--locked`. The fake cargo would happily build 0.2.0 if asked
    /// for "latest", so the version alone proves the registry was not
    /// consulted for the choice.
    #[test]
    fn reinstall_rebuilds_the_entry_and_returns_it_unchanged() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-entry");
        let _ = fs::remove_dir_all(&root);
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));

        for (pinned, locked) in [(true, true), (false, false), (true, false), (false, true)] {
            // seeded_prefix's order is (locked, pinned).
            let prefix = seeded_prefix(
                &root,
                &format!("p-{pinned}-{locked}"),
                "okcrate",
                locked,
                pinned,
            );
            cmd_install(&prefix, &["okcrate".to_owned()], false, true).unwrap();
            let entry = Manifest::load(&prefix).unwrap().crates["okcrate"].clone();
            assert_eq!(
                entry.version, "0.1.0",
                "the entry's version, not whatever latest resolves to"
            );
            assert_eq!(entry.pinned, pinned, "the pin is preserved, either way");
            assert_eq!(entry.locked, locked, "and so is --locked");
            assert!(
                prefix.join("bin/okcrate").exists(),
                "and the binary is placed"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// A fake cargo that builds whatever crate it is asked for: `$2` is
    /// the name, `$4` the stage root. The sweep needs it, because a
    /// sweep by definition names more than one crate.
    fn any_crate_fake(root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            "#!/bin/sh\n\
             name=$2\n\
             ver=0.2.0\n\
             for a in \"$@\"; do\n\
             case \"$a\" in =*) ver=${a#=};; esac\n\
             done\n\
             mkdir -p \"$4/bin\"\n\
             printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/$name\"\n\
             chmod 755 \"$4/bin/$name\"\n\
             printf '%s' \"{\\\"installs\\\":{\\\"$name $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{\\\"bins\\\":[\\\"$name\\\"]}}}\" > \"$4/.crates2.json\"\n\
             exit 0\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A fake cargo that refuses one crate by name and builds every
    /// other, for testing what a sweep does around a failure.
    fn failing_fake(root: &Path, failing: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake_bin = root.join("fakebin");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 name=$2\n\
                 [ \"$name\" = \"{failing}\" ] && {{ echo 'error: could not compile' >&2; exit 101; }}\n\
                 ver=0.2.0\n\
                 for a in \"$@\"; do\n\
                 case \"$a\" in =*) ver=${{a#=}};; esac\n\
                 done\n\
                 mkdir -p \"$4/bin\"\n\
                 printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/$name\"\n\
                 chmod 755 \"$4/bin/$name\"\n\
                 printf '%s' \"{{\\\"installs\\\":{{\\\"$name $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{{\\\"bins\\\":[\\\"$name\\\"]}}}}}}\" > \"$4/.crates2.json\"\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The sweep is the same operation, wider: every entry keeps its own
    /// version, pin and `--locked`, and a pinned crate is rebuilt rather
    /// than skipped — a pin holds a version, and this is the operation
    /// that does not change one.
    #[test]
    fn reinstall_all_rebuilds_every_entry_as_its_own_specification() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-all");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", true, true);
        // A second entry, unpinned and unlocked, seeded into the same
        // manifest: the sweep must treat the two differently.
        {
            let mut m = Manifest::load(&prefix).unwrap();
            m.crates.insert(
                "othercrate".to_owned(),
                Entry {
                    version: "0.1.0".to_owned(),
                    bins: vec!["othercrate".to_owned()],
                    locked: false,
                    pinned: false,
                    built_with_rustc: None,
                },
            );
            m.store(&prefix).unwrap();
        }
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));

        cmd_reinstall_all(&prefix, true).unwrap();

        let m = Manifest::load(&prefix).unwrap();
        let ok = &m.crates["okcrate"];
        assert_eq!(ok.version, "0.1.0", "the pinned entry keeps its version");
        assert!(
            ok.pinned && ok.locked,
            "and its policy: pinned={} locked={}",
            ok.pinned,
            ok.locked
        );
        let other = &m.crates["othercrate"];
        assert_eq!(other.version, "0.1.0", "and the unpinned one keeps its too");
        assert!(
            !other.pinned && !other.locked,
            "without acquiring a pin on the way: pinned={} locked={}",
            other.pinned,
            other.locked
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A crate that no longer builds is the reason to hear about the
    /// rest, not to stop asking: the sweep carries on, rebuilds what it
    /// can, and the exit code says the confirmed plan was not carried
    /// out in full. Since 0.17.0 this is every batch's policy —
    /// `install`'s named list iterates the same way.
    #[test]
    fn a_failing_crate_does_not_stop_the_sweep() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-all-failure");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "brokencrate", false, false);
        {
            let mut m = Manifest::load(&prefix).unwrap();
            m.crates.insert(
                "okcrate".to_owned(),
                Entry {
                    version: "0.1.0".to_owned(),
                    bins: vec!["okcrate".to_owned()],
                    locked: false,
                    pinned: false,
                    built_with_rustc: None,
                },
            );
            m.store(&prefix).unwrap();
        }
        // Both binaries are removed first, so a rebuild is the only way
        // either of them can come back.
        let _ = fs::remove_file(prefix.join("bin/brokencrate"));
        let _ = fs::remove_file(prefix.join("bin/okcrate"));
        let _fake = crate::stage::FakeCargo::install(&failing_fake(&root, "brokencrate"));

        let err = cmd_reinstall_all(&prefix, true).expect_err("a shortfall is not a success");
        let text = format!("{err:#}");
        assert!(text.contains("1 of 2 rebuilds not applied"), "{text}");
        assert!(text.contains("brokencrate"), "and it names which: {text}");

        assert!(
            prefix.join("bin/okcrate").exists(),
            "the crate after the failing one was still built"
        );
        assert!(
            !prefix.join("bin/brokencrate").exists(),
            "and the failing one placed nothing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `install a b c` follows the sweeps' policy: the failing member
    /// is reported, the one behind it still builds, and the shortfall
    /// is in the exit code — an error in B is a statement about B;
    /// only an explicit cancel is a statement about the rest.
    #[test]
    fn a_failing_member_does_not_stop_the_named_list() {
        let root = std::env::temp_dir().join("cargo-lbin-test-install-batch-carries-on");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "brokencrate", false, false);
        {
            let mut m = Manifest::load(&prefix).unwrap();
            m.crates.insert(
                "okcrate".to_owned(),
                Entry {
                    version: "0.1.0".to_owned(),
                    bins: vec!["okcrate".to_owned()],
                    locked: false,
                    pinned: false,
                    built_with_rustc: None,
                },
            );
            m.store(&prefix).unwrap();
        }
        let _ = fs::remove_file(prefix.join("bin/brokencrate"));
        let _ = fs::remove_file(prefix.join("bin/okcrate"));
        let _fake = crate::stage::FakeCargo::install(&failing_fake(&root, "brokencrate"));

        // `--reinstall` keeps the whole run offline: each member's
        // specification is read from the manifest, no registry asked.
        let err = cmd_install(
            &prefix,
            &["brokencrate".to_owned(), "okcrate".to_owned()],
            false,
            true,
        )
        .expect_err("a shortfall is not a success");
        let text = format!("{err:#}");
        assert!(text.contains("1 of 2 installs not carried out"), "{text}");
        assert!(text.contains("brokencrate"), "and it names which: {text}");
        assert!(
            prefix.join("bin/okcrate").exists(),
            "the member after the failing one was still built"
        );
        assert!(
            !prefix.join("bin/brokencrate").exists(),
            "and the failing one placed nothing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The prompt is a window, so what was confirmed is re-checked
    /// before it is acted on — all three facts the plan showed. A crate
    /// that moved in the meantime is skipped with a note, and the
    /// shortfall is in the exit code.
    #[test]
    fn a_plan_confirmed_against_a_state_that_moved_is_not_carried_out() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-all-moved");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        let _fake = crate::stage::FakeCargo::install(&any_crate_fake(&root));

        // A plan made against 0.1.0, unpinned and unlocked — the state
        // the person would have seen and agreed to.
        let planned = vec![(
            "okcrate".to_owned(),
            ReinstallPlan {
                version: Version::parse("0.1.0").unwrap(),
                pinned: false,
                locked: false,
            },
        )];

        // Each of the three facts alone is enough to disqualify it.
        for mutate in [
            (|e: &mut Entry| e.version = "0.3.0".to_owned()) as fn(&mut Entry),
            |e: &mut Entry| e.pinned = true,
            |e: &mut Entry| e.locked = true,
        ] {
            let mut m = Manifest::load(&prefix).unwrap();
            mutate(m.crates.get_mut("okcrate").unwrap());
            let expected = m.crates["okcrate"].version.clone();
            m.store(&prefix).unwrap();
            let _ = fs::remove_file(prefix.join("bin/okcrate"));

            let err = apply_reinstalls(&prefix, &root.join("cache"), &planned)
                .expect_err("a plan about a state that is gone");
            assert!(
                format!("{err:#}").contains("skipped: okcrate"),
                "the shortfall names it: {err:#}"
            );
            assert!(
                !prefix.join("bin/okcrate").exists(),
                "and nothing was rebuilt against the new state"
            );
            assert_eq!(
                Manifest::load(&prefix).unwrap().crates["okcrate"].version,
                expected,
                "the entry is left exactly as the other process left it"
            );
            // Restore the baseline for the next case.
            let mut m = Manifest::load(&prefix).unwrap();
            let e = m.crates.get_mut("okcrate").unwrap();
            e.version = "0.1.0".to_owned();
            e.pinned = false;
            e.locked = false;
            m.store(&prefix).unwrap();
        }

        // A crate that vanished during the prompt is the same answer.
        let mut m = Manifest::load(&prefix).unwrap();
        m.crates.remove("okcrate");
        m.store(&prefix).unwrap();
        let err = apply_reinstalls(&prefix, &root.join("cache"), &planned)
            .expect_err("nothing left to rebuild");
        assert!(format!("{err:#}").contains("skipped: okcrate"), "{err:#}");
        let _ = fs::remove_dir_all(&root);
    }

    /// An empty prefix is an answer, not a confirmation prompt over an
    /// empty list — and `--all` without `--reinstall` has no meaning to
    /// guess at.
    #[test]
    fn reinstall_all_says_so_when_there_is_nothing_to_rebuild() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-all-empty");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        Manifest::default().store(&prefix).unwrap();

        cmd_reinstall_all(&prefix, true).unwrap();
        assert!(
            Manifest::load(&prefix).unwrap().crates.is_empty(),
            "nothing installed, nothing done"
        );

        let bare = <Cli as clap::CommandFactory>::command().try_get_matches_from(vec![
            "cargo-lbin",
            "install",
            "--all",
        ]);
        assert!(bare.is_err(), "`--all` alone does not say what to install");
        let both = <Cli as clap::CommandFactory>::command().try_get_matches_from(vec![
            "cargo-lbin",
            "install",
            "--reinstall",
            "--all",
            "okcrate",
        ]);
        assert!(both.is_err(), "two ways of naming the scope");

        // `-y` skips a prompt, and only the sweep asks one: accepted
        // elsewhere it would be a flag the command ignores. Clap's
        // `requires` cannot express this for a bool flag (see the arg's
        // doc), so dispatch refuses the shape, and this pins the shape
        // it refuses — including the one `requires` let through.
        for args in [
            vec!["cargo-lbin", "install", "-y", "okcrate"],
            vec!["cargo-lbin", "install", "--reinstall", "-y", "okcrate"],
        ] {
            let cli = <Cli as clap::Parser>::try_parse_from(args.clone()).expect("parses");
            let Cmd::Install { all, yes, .. } = cli.cmd else {
                panic!("install: {args:?}")
            };
            assert!(yes && !all, "the shape dispatch refuses: {args:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Three refusals, all of the same shape: the entry is the
    /// specification, so a second source for any part of it is a usage
    /// error, and an absent entry is nothing to rebuild.
    #[test]
    fn reinstall_refuses_a_second_source_of_truth() {
        let root = std::env::temp_dir().join("cargo-lbin-test-reinstall-refusals");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);

        let err = cmd_install(&prefix, &["okcrate@0.1.0".to_owned()], false, true)
            .expect_err("two answers to which version");
        assert!(
            format!("{err:#}").contains("takes the version from the manifest"),
            "{err:#}"
        );

        let err = cmd_install(&prefix, &["nosuchcrate".to_owned()], false, true)
            .expect_err("nothing to rebuild");
        assert!(
            format!("{err:#}").contains("not installed: nosuchcrate"),
            "{err:#}"
        );

        // And it is a preflight, not a late discovery: an absent crate
        // behind a present one stops the batch before the first build,
        // because the manifest already knew.
        let _fake = crate::stage::FakeCargo::install(&versioned_fake(&root, "okcrate"));
        // The binary is removed first: a rebuild would put it back, so
        // its absence afterwards is evidence that no build ran — a
        // manifest comparison alone would pass even if `okcrate` had
        // been rebuilt to the same version.
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();
        let before = fs::read_to_string(prefix.join("share/cargo-lbin/manifest.json")).unwrap();
        let err = cmd_install(
            &prefix,
            &["okcrate".to_owned(), "nosuchcrate".to_owned()],
            false,
            true,
        )
        .expect_err("the batch refuses as a whole");
        assert!(
            format!("{err:#}").contains("not installed: nosuchcrate"),
            "{err:#}"
        );
        assert!(
            !prefix.join("bin/okcrate").exists(),
            "nothing was built before the knowable refusal"
        );
        assert_eq!(
            fs::read_to_string(prefix.join("share/cargo-lbin/manifest.json")).unwrap(),
            before,
            "and nothing was committed"
        );

        // `--locked` is refused by clap itself, where a flag conflict
        // belongs — before any prefix is touched.
        let conflict = <Cli as clap::CommandFactory>::command().try_get_matches_from(vec![
            "cargo-lbin",
            "install",
            "--reinstall",
            "--locked",
            "okcrate",
        ]);
        assert!(
            conflict.is_err(),
            "--locked and --reinstall both answer build policy"
        );

        // And the entry is untouched by any of it.
        let entry = Manifest::load(&prefix).unwrap().crates["okcrate"].clone();
        assert_eq!(entry.version, "0.1.0");
        assert!(!entry.pinned);
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

    /// A refusal after the build says which half happened. Cargo has
    /// already announced an install — of the *stage* — so an error that
    /// only said "sudo authentication failed" would leave the person
    /// guessing whether files landed.
    #[cfg(feature = "tui")]
    #[test]
    fn a_refusal_after_the_build_says_what_was_and_was_not_done() {
        let root = std::env::temp_dir().join("cargo-lbin-test-late-auth-refused");
        let _ = fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        let _fake = crate::stage::FakeCargo::install(&staging_fake(&root, "okcrate"));

        let control = BuildControl::new();
        let mut manifest = Manifest::default();
        let err = install_and_commit(
            &prefix,
            &root.join("cache"),
            &mut manifest,
            "okcrate",
            None,
            false,
            PinPolicy::Infer,
            ShadowReport::OnCommit,
            &mut Frontend::Captured {
                on_line: &mut |_, _| {},
                before_placement: &mut |_| Ok(()),
                control: &control,
                // Stands in for the door refusing on a prefix this user
                // can write: what matters here is the wording of a
                // refusal that lands after a finished build.
                checkpoint: Some(&mut || bail!("sudo authentication failed")),
            },
        )
        .expect_err("a refusal is a refusal");
        let text = format!("{err:#}");
        assert!(text.contains("the build finished"), "{text}");
        assert!(text.contains("placement did not begin"), "{text}");
        assert!(text.contains("manifest is unchanged"), "{text}");
        assert!(text.contains("sudo authentication failed"), "{text}");
        assert!(
            !manifest.crates.contains_key("okcrate")
                && Manifest::load(&prefix).unwrap().crates.is_empty(),
            "nothing is recorded that did not happen"
        );
        assert!(
            !prefix.join("bin/okcrate").exists(),
            "and nothing is placed"
        );
        let runs: Vec<PathBuf> = fs::read_dir(root.join("cache").join(crate::stage::RUN_NAMESPACE))
            .map(|e| e.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        assert_eq!(runs.len(), 1, "the stage is kept as forensics: {runs:?}");
        let _ = fs::remove_dir_all(&root);
    }

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
            ShadowReport::OnCommit,
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
