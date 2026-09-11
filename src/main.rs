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
    // Precedence: explicit --prefix, then $CARGO_LBIN_PREFIX, then
    // /usr/local — clap's env support handles the ordering and appends
    // the [env: ...] and [default: ...] annotations to --help on its
    // own. The point: a user who never wants sudo exports
    // CARGO_LBIN_PREFIX=~/.local once (expanded by the shell) and stops
    // typing --prefix on every command.
    // `help =`, not a doc comment: the text is help-text only, and
    // `<prefix>/bin` — the help's established notation, used twice in
    // the long about — parses as an unclosed HTML tag under rustdoc.
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
    // Deliberately overrides $CARGO_LBIN_PREFIX too: an alias exists to
    // be typed ad hoc, and ad hoc must beat ambient configuration —
    // only an explicit --prefix conflicts, because two explicit answers
    // to the same question deserve an error, not a precedence rule.
    // No clap-level conflict: clap counts an env-provided value as
    // "present", and the documented pattern — export CARGO_LBIN_PREFIX
    // once — must not lock --user out forever. The explicit-flag
    // conflict is enforced below via value_source instead.
    #[arg(long, global = true)]
    user: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

/// The user prefix `--user` stands for. `$HOME/.local` is the one
/// FHS-adjacent place a prefix layout maps onto untouched: bin/ is
/// blessed by systemd's file-hierarchy(7) and on PATH almost
/// everywhere, and share/cargo-lbin lands in the default XDG user data
/// location (a custom `XDG_DATA_HOME` points elsewhere, and lbin's state
/// deliberately follows the prefix, not the variable). No new layout,
/// no special cases — the same tree, owned by the user.
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
    /// Hold crates at their installed version
    ///
    /// A pinned crate is left out of `update --all` and refused by
    /// `update NAME` and `install NAME` until unpinned.
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
    /// Rebuild an installed crate under another prefix, then retire it
    /// here
    ///
    /// The exact installed version is rebuilt at the destination — never
    /// copied, so provenance is re-established by the same pipeline as
    /// `install` — carrying `--locked` and the pin. The entry here is
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
    // Either an explicit list of crates or `--all`, never neither: a bare
    // `update` has no obvious meaning once single-crate updates exist, and
    // "obvious" is exactly what an operation that rebuilds and replaces
    // system binaries must not be guessed at. Cargo-lbin does what it is
    // told, and `--all` is the user telling it.
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
    // Support both direct invocation (`cargo-lbin install foo`) and the
    // cargo-subcommand form (`cargo lbin install foo`), where cargo passes
    // "lbin" as the first argument. Strip that token if present so clap
    // sees the same argv either way.
    let args = std::env::args_os()
        .enumerate()
        .filter_map(|(i, a)| (!(i == 1 && a == *"lbin")).then_some(a));
    let matches = <Cli as clap::CommandFactory>::command().get_matches_from(args);
    let mut cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    if cli.user {
        // Two explicit answers to the same question deserve an error; an
        // ambient one (the env variable) yields to the ad hoc flag —
        // that is what an alias is for.
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
    // Running the whole program as root would execute cargo — build scripts
    // and proc macros included — with root privileges, undoing the one
    // security property the entire design rests on. `sudo cargo-lbin install foo`
    // typed out of habit must fail loudly, not succeed quietly. Parsing
    // happens first so `sudo cargo-lbin --help` still works. The override exists
    // for environments where root is the only user (containers, CI); there
    // the user/root distinction cargo-lbin protects is vacuous to begin with.
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
        Cmd::Pin { ref crates } => cmd_set_pinned(&cli.prefix, crates, true),
        Cmd::Unpin { ref crates } => cmd_set_pinned(&cli.prefix, crates, false),
        Cmd::Pinned { check, json } => return cmd_pinned(&cli.prefix, check, json),
        Cmd::List { json } => cmd_list(&cli.prefix, json),
        #[cfg(feature = "tui")]
        Cmd::Tui => tui::run(&cli.prefix),
        Cmd::Info { ref crates } => cmd_info(&cli.prefix, crates),
        Cmd::Search { ref query, limit } => cmd_search(&cli.prefix, query, limit),
        Cmd::Checkupdate { json } => return cmd_checkupdate(&cli.prefix, json),
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

/// Binaries the old entry installed that the new build no longer provides.
/// Without this cleanup an update from `foo 1.0` (foo, fooctl) to `foo 2.0`
/// (foo only) would strand `fooctl` on disk with the manifest already
/// having forgotten it.
fn obsolete_bins(old: &[String], new: &[String]) -> Vec<String> {
    old.iter().filter(|b| !new.contains(b)).cloned().collect()
}

/// Binaries the new build introduces that the old entry did not provide —
/// the mirror image of `obsolete_bins`. These, and only these, are removed
/// when an operation fails before its manifest commit: a pre-existing name
/// that was already overwritten stays in place (the manifest still owns it,
/// so a retry simply replaces it again), while a leftover *new* name would
/// make the retry collide with what looks like an unmanaged file.
fn newly_introduced_bins(old: &[String], new: &[String]) -> Vec<String> {
    obsolete_bins(new, old)
}

/// Bookkeeping for undoing a partially applied install: which binary names
/// are new in this operation, and which of those actually reached the disk.
///
/// This set is complete only because placement is atomic (see
/// `install_atomic`: same-directory temp + rename): a failed install leaves
/// nothing under the destination name, so "successfully placed new names"
/// and "new names present on disk" are the same set. If placement ever
/// stops being atomic, this bookkeeping — and the rollback built on it —
/// develops a hole.
struct RollbackSet {
    /// Names absent from the previous manifest entry for this crate.
    new_names: Vec<String>,
    /// Destinations among `new_names` that were actually placed.
    placed: Vec<PathBuf>,
}

impl RollbackSet {
    /// Must be taken from the manifest *before* the new entry is inserted;
    /// any later, every name looks pre-owned and the set silently comes out
    /// empty.
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

/// Best-effort removal of the newly placed binaries after a failure between
/// the first placement and the manifest commit.
///
/// Never returns an error: the original failure must propagate unmasked,
/// and the most likely reason to be here at all is sudo trouble (an expired
/// credential cache, an interrupted password prompt) — which would sink
/// these removals too. Removal is attempted per file so partial success is
/// possible; whatever survives is reported by name, because a leftover new
/// binary would otherwise greet the retry with a baffling "already exists
/// and is not managed by cargo-lbin".
///
/// Recovery after a rolled-back *update* additionally leans on two
/// properties elsewhere: `check_collisions` decides ownership by name, so
/// "manifest says 1.0, disk has 2.0" is still ours and a retry replaces it;
/// and `remove_files` is `rm -f`, so re-removing an obsolete binary a
/// previous attempt already deleted is a no-op. Content checksums in the
/// manifest would break the first property — if they are ever added, verify
/// them on remove only, never as an install precondition.
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

/// Refuse to clobber anything we do not own.
///
/// A destination is acceptable only if it does not exist, or if the manifest
/// says this very crate installed it. A file owned by another cargo-lbin-managed
/// crate or by nobody at all is an error, checked before placement so the
/// existing file is left untouched.
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
            // With Err-path rollback in place, the one way cargo-lbin itself
            // produces this state is a hard kill (SIGKILL, power loss)
            // between placement and the manifest commit — accepted as out
            // of scope for automatic recovery. This error is the orphan's
            // only symptom, so it names the manual way out; whether the
            // file really is such a leftover is the user's call.
            bail!(
                "{} already exists and is not managed by cargo-lbin \
                 (if it is a leftover from an interrupted run, remove it and retry)",
                dest.display()
            );
        }
    }
    Ok(())
}

/// What a captured line is, decided where it is spoken. Capture is not
/// presentation: a frontend shows cargo's stream live, promotes notices
/// to the live status, and keeps warnings visible past a success — and
/// none of that is possible if every line arrives as anonymous text.
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

/// The phases one in-place build moves through. Stored as a `u8` inside
/// `BuildControl`; the numeric values are the atomic encoding, nothing
/// more.
#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum BuildPhase {
    /// Building — or not even started yet. Cancellable.
    Building = 0,
    /// Placement has begun: privileged writes may be in flight, and a
    /// signal now could stop `sudo install` between two binaries. Too
    /// late to cancel; placement is seconds, not minutes.
    Placement = 1,
    /// A cancel was accepted while building. cargo hears it as SIGTERM
    /// to its group; the worker sees it at the placement door at the
    /// latest and never commits anything.
    Cancelling = 2,
}

/// Shared control block between the UI thread and one in-place build:
/// the worker's phase and cargo's process group id, both crossing
/// threads. The phase moves only by compare-and-swap, so the worker
/// stepping into placement and a cancel arriving at that exact moment
/// have exactly one winner — a plain "cancelled" flag read before the
/// checkpoint would let the request be accepted on screen after the
/// point of no return had already been crossed.
#[cfg(feature = "tui")]
pub(crate) struct BuildControl {
    phase: std::sync::atomic::AtomicU8,
    /// cargo's process group id: 0 before the spawn and again from the
    /// reap. Zeroing matters — a group id whose last member has been
    /// collected is the kernel's to reuse — and the withdrawal runs
    /// inside `build_captured`, immediately after `wait()` returns,
    /// ahead of the failure-log and stage-verification I/O that zeroing
    /// after the function's return would have left inside the window.
    /// The UI holds this block in an `Arc` per job, so a late signal
    /// can never address a different *cargo-lbin* build's group. What
    /// remains is small but honestly nonzero: the UI can load the id,
    /// lose the CPU across the reap, and signal a number the kernel has
    /// since handed to a stranger. Closing that for real means pidfds —
    /// a power plant for a mosquito; the window is accepted, and named
    /// here instead of denied.
    pgid: std::sync::atomic::AtomicI32,
}

/// What a cancel request found; the UI phrases each differently.
#[cfg(feature = "tui")]
#[derive(Debug)]
pub(crate) enum CancelOutcome {
    /// The request won: the build will not reach placement. SIGTERM
    /// went to the group if cargo was already running; if not, the
    /// spawn announcement delivers it (see `spawned`).
    Accepted,
    /// Already cancelling and the group still stood — escalated to
    /// SIGKILL. The person asked twice; cargo is not asked politely a
    /// second time.
    Killed,
    /// Already cancelling, with nothing left to signal: the group is
    /// not yet spawned or already reaped, and the worker is winding
    /// down on its own.
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

    /// Whether a cancel has been accepted. A cheap read for courtesy
    /// checks — refusing to start cargo, refusing to prompt for a
    /// password — never for the placement decision, which belongs to
    /// the compare-and-swap in `begin_placement` alone.
    pub fn cancelled(&self) -> bool {
        self.phase() == BuildPhase::Cancelling
    }

    /// The worker announces cargo's process group, straight from the
    /// spawn. A cancel accepted before there was anything to signal is
    /// delivered here: the order is store-then-check, mirroring
    /// `request_cancel`'s swap-then-load, so whichever side runs second
    /// sees the other's write and the signal is delivered at least once
    /// — possibly twice when the two interleave, which is harmless: a
    /// second SIGTERM to a group already dying changes nothing.
    pub fn spawned(&self, pgid: i32) {
        self.pgid.store(pgid, Self::ORD);
        if self.cancelled() {
            Self::signal(pgid, libc::SIGTERM);
        }
    }

    /// This side is done signalling the group: the leader is reaped
    /// and, on a cancellation, the survivors have been swept with
    /// SIGKILL — a group id stays alive with *any* member, so the
    /// leader's reap alone frees nothing. Called by `build_captured`
    /// itself; from here the id must not be signalled again, and once
    /// the group truly empties the kernel is free to reuse it.
    pub fn reaped(&self) {
        self.pgid.store(0, Self::ORD);
    }

    /// The one-way door into placement, crossed by the worker right
    /// before the first write anything would have to roll back. Refusal
    /// means a cancel won the race; the worker unwinds with nothing
    /// placed and nothing committed.
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
                    // Nothing left to signal: the group is not yet
                    // spawned or already reaped, and the worker is
                    // winding down on its own. Saying "SIGKILL sent"
                    // here would be a lie the UI repeats.
                    CancelOutcome::AlreadyStopping
                } else {
                    Self::signal(pgid, libc::SIGKILL);
                    CancelOutcome::Killed
                }
            }
            Err(_) => CancelOutcome::TooLate,
        }
    }

    /// Negative pid: the whole group — cargo and every rustc and build
    /// script it is running. The result is ignored on purpose: ESRCH
    /// means the group is already gone, which is the goal.
    fn signal(pgid: i32, sig: i32) {
        // SAFETY: kill(2) with a negative pid signals a process group;
        // no memory is touched and any error is an acceptable no-op.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// Marker error for a build that ended because the person cancelled it.
/// A distinct type, not a string: the worker classifies the outcome by
/// downcast instead of the UI guessing from the phase flag — a guess
/// that loses the race where cargo dies of its own causes an instant
/// before a late cancel is accepted, and would present cargo's real
/// error as "cancelled".
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
    /// The terminal, plus a caller-supplied checkpoint at the placement
    /// door — migrate's shape: cargo owns the terminal exactly as with
    /// `Terminal`, and the checkpoint is migrate's early revalidation of
    /// the source prefix, run after the build and before the destination
    /// commits anything. Distinct from `before_placement`, which exists
    /// for sudo: this one runs at every prefix, escalating or not.
    Checkpointed {
        checkpoint: &'a mut dyn FnMut() -> Result<()>,
    },
    /// A screen-owning frontend (the TUI): every line — cargo's and the
    /// pipeline's own — is forwarded instead of printed, classified at
    /// the source: the pipeline knows whether it speaks cargo's words, a
    /// note of its own, or a warning, and a frontend deciding what must
    /// survive a successful install cannot reconstruct that from text.
    /// Placement waits on a checkpoint confirming sudo credentials are
    /// still fresh, because a hidden password prompt would hang an
    /// alternate screen rather than show on it.
    #[cfg(feature = "tui")]
    Captured {
        on_line: &'a mut dyn FnMut(LineKind, &str),
        /// Called with the prefix the escalation is *for*: one worker
        /// can revalidate for two different prefixes in one job (a
        /// migration's destination placement and its source
        /// retirement), and the auth prompt must name the right one —
        /// guessing it from job state on the UI side would lie in
        /// exactly one direction of the pair.
        before_placement: &'a mut dyn FnMut(&Path) -> Result<()>,
        /// The cancel state machine shared with the UI thread; the
        /// pipeline reports the spawn and the reap through it and asks
        /// it for permission to place.
        control: &'a BuildControl,
        /// Composed *after* the placement door, when present: the door
        /// decides ownership of the operation first — a cancel that won
        /// the race must not so much as probe another prefix for a build
        /// that is already dying — and only the worker that crossed it
        /// runs the checkpoint, still ahead of anything committing. This
        /// is how a captured migration gets its early source
        /// revalidation; a plain install carries None.
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

    /// Warnings. stderr in the terminal; captured with their kind — a
    /// warning that melts into anonymous text is a warning lost. One
    /// callback still, one classification.
    fn warning(&mut self, s: &str) {
        match self {
            Frontend::Terminal | Frontend::Checkpointed { .. } => eprintln!("{s}"),
            #[cfg(feature = "tui")]
            Frontend::Captured { on_line, .. } => on_line(LineKind::Warning, s),
            #[cfg(not(feature = "tui"))]
            Frontend::Never(_) => unreachable!(),
        }
    }

    /// Whether the pipeline's own preauthorize should run. The terminal
    /// prompts fine; a captured frontend validated credentials before
    /// handing over and re-checks at the placement checkpoint, so a
    /// prompt from inside the pipeline would be exactly the hidden one
    /// this type exists to prevent.
    fn wants_preauthorize(&self) -> bool {
        matches!(self, Frontend::Terminal | Frontend::Checkpointed { .. })
    }

    /// The cancel door: crossed unconditionally right before the first
    /// write that would need rolling back. Distinct from
    /// `before_placement`, which exists for sudo and never runs at a
    /// user-writable prefix — "too late to cancel" must mean the same
    /// thing at every prefix. Deliberately the last checkpoint: the
    /// sudo revalidation above it can block on a human, and a cancel
    /// arriving during that wait must still win.
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
                // The order is the contract: CAS first, checkpoint
                // second. The door settles who owns the operation — a
                // lost race returns the typed cancellation and the
                // checkpoint is never consulted — and a checkpoint that
                // passes has still run before the first write anything
                // would have to roll back, which is exactly where the
                // migrate contract promises its early revalidation. The
                // brief window in which a cancel is already TooLate
                // while the destination is still untouched is the price
                // of one-way doors, and the checkpoint is nonblocking:
                // it can only wave the operation through or end it fast.
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

    /// The placement checkpoint: a no-op on the terminal, the frontend's
    /// re-validation hook when captured — a build can outlive sudo's
    /// credential timestamp. Called only when placement will escalate;
    /// a user-writable prefix never reaches it.
    // `prefix` is consumed only by the tui arm; the parameter is the
    // contract either way — the site that escalates names its target.
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

/// Build one crate, verify ownership of the destinations, place binaries,
/// clean up binaries the previous version provided but the new one does not,
/// and commit the manifest — all before the next crate is touched, so a
/// failure mid-batch never leaves installed files unrecorded.
///
/// Each crate gets its own stage directory, wiped before the build and
/// removed only once the manifest write has succeeded. A shared, persistent stage had two
/// failure modes: cargo could refuse a build over a stale binary from an
/// already-removed crate before our own collision check ever saw the real
/// prefix, and a reinstall with a different `--locked` flag could be
/// silently skipped as "already installed", recording a flag the staged
/// binary was never built with. A fresh stage eliminates both; nothing of
/// value is lost, since cargo's registry and build caches live elsewhere.
// Eight arguments, like `place_and_commit` below and for the same
// reason: these are the parameters of one install, and a struct naming
// the bundle would be built at every call site only to be destructured
// here.
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
) -> Result<()> {
    // Revalidate even though CLI input was already checked: on the update
    // path `name` comes from the manifest, and a hand-edited manifest must
    // not be able to steer the remove_dir_all below via a path-like name.
    validate_name(name)?;
    // A captured frontend forbids prompting outright: `-v` beforehand is
    // a convenience, never a proof, so the privileged calls themselves
    // run `sudo -n` — a password wanted there becomes a loud error in
    // the failure panel instead of a prompt hung invisibly beneath the
    // alternate screen.
    let policy = match frontend {
        Frontend::Terminal | Frontend::Checkpointed { .. } => {
            privileged::Policy::for_prefix(prefix)
        }
        #[cfg(feature = "tui")]
        Frontend::Captured { .. } => privileged::Policy::for_prefix(prefix).screen_owned(),
        #[cfg(not(feature = "tui"))]
        Frontend::Never(_) => unreachable!(),
    };
    // UX-only early form of the policy check: fail before a multi-minute
    // build, not after. Enforcement proper lives at every privileged call
    // site via the sudo axis of `Policy`; this merely surfaces the same refusal sooner.
    // The same reasoning moves the password prompt here: when placement
    // will need sudo, validate credentials now, so the initial prompt
    // comes before the build instead of ambushing an unattended terminal
    // after it (sudo may still re-prompt if its timestamp expires).
    let initial_escalate = install_needs_privilege(policy, prefix)?;
    if frontend.wants_preauthorize() {
        privileged::preauthorize(prefix, initial_escalate)?;
    }
    // Per-PID stage: the state lock serializes instances per *prefix*, so
    // two cargo-lbin runs against different prefixes may legitimately build the
    // same crate at the same time — and one wiping the other's stage
    // mid-build must be structurally impossible, not merely unlikely.
    // Stale PID directories after a crash are plain cache debris; a reused
    // PID wipes its own directory before building anyway.
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
    // Only for names this crate did not provide before: on a first
    // install that is every binary; on an update it is the ones the new
    // version adds (`foo` 2.0 shipping a `fooctl` that 1.0 did not),
    // which were never checked and may well exist in `/usr/bin`. Names
    // carried over were reported when they were new.
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
    // Resolved here, against the entry as it still is: under Infer an
    // exact version pins (or the next `update --all` would undo the
    // choice), and a pin already present is carried over — `install` and
    // `update` refuse pinned crates unless a version is named, so when
    // neither term holds the carried value can only be false too, but a
    // pin is not something a rewrite of the entry gets to drop by
    // omission. Under Exactly the caller has already made the promise.
    let pinned = match pin {
        PinPolicy::Infer => {
            version.is_some() || manifest.crates.get(name).is_some_and(|e| e.pinned)
        }
        PinPolicy::Exactly(pinned) => pinned,
    };
    // The checkpoint sits between the last unprivileged step and the
    // first privileged one: everything before it needed no sudo, and a
    // build can outlive sudo's credential timestamp. At a user-writable
    // prefix nothing ahead will run sudo, so there is nothing to
    // revalidate — and a frontend must not be made to poke sudo on a
    // system that may not even have it. Re-probed here rather than
    // reused from before the build: minutes have passed, the privileged
    // call sites re-check writability themselves, and a stale answer
    // could skip the checkpoint right before sudo -n discovers a new
    // need — failing the install with no chance to reauth. "The
    // checkpoint precedes the first privileged write" is a claim about
    // now, not about the pre-build world.
    // The two pre-placement checkpoints, fallible as one unit — because
    // the sudo revalidation can itself answer with a cancellation (a
    // `c` pressed while NeedAuth was waiting on the run loop), and the
    // invariant is "BuildCancelled from anywhere before placement means
    // no stage left", not "cancelled at the atomic door means no stage
    // left". A cancel leaves nothing worth keeping: the stage is
    // evidence of nothing but the person's own decision. Every other
    // refusal — a migrate checkpoint abort, a real sudo failure — keeps
    // its stage like any other pipeline failure, and the downcast does
    // not match it. With the tui feature off no cancel exists and the
    // result propagates bare.
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
    // Stage removal is deliberately the very last step: if placement,
    // obsolete cleanup or the manifest write fails above, the stage that
    // produced the partial state survives as forensic evidence — its
    // .crates2.json and binaries describe exactly the build that caused the
    // problem (and, after a rollback, exactly what was removed again).
    let _ = fs::remove_dir_all(&stage_dir);
    if let Some(pid_dir) = stage_dir.parent() {
        // Best effort, non-recursive: succeeds only once our PID directory
        // is empty, i.e. after the last crate of this run.
        let _ = fs::remove_dir(pid_dir);
    }
    Ok(())
}

/// Everything between the first privileged placement and the manifest
/// commit, fallible as one unit. The single caller runs `rollback_new_bins`
/// on any `Err`, so placement, obsolete cleanup, manifest serialization,
/// the sealed memfd and the atomic manifest placement are all covered by
/// the same rollback — without cleanup code at every `?`.
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
    // Open and verify every staged source as the user before any privileged
    // placement; root then copies our vetted descriptors via /proc, never a
    // pathname the (user-controlled) stage could swap underneath us.
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
    // `pinned` arrives resolved (see `install_and_commit`): the policy
    // and the carry-over were read against the pre-commit entry, before
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
    // Announced only after the manifest commit: with a rollback path in
    // play, an "installed" printed before `store` could be followed by that
    // very installation being undone.
    // Keyed off the committed state, not the request: the note describes
    // what the manifest now says, which is what `unpin` would change.
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

/// One warning line per binary that a `PATH` entry outside the prefix
/// already provides — usually a distribution package — naming the file,
/// its owner if the package manager will say, and which of the two
/// directories comes first in `PATH`. A warning only; see `shadow` for why it is not a
/// refusal. Given only for names new to this crate: a distro package
/// that appears *after* ours took the name is a collision that arose
/// outside cargo-lbin, and repeating the warning on every update would
/// be the price of catching it.
fn shadow_warnings(prefix: &Path, bins: &[String]) -> Vec<String> {
    if bins.is_empty() {
        return Vec::new();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    // The scan needs the working directory only to anchor relative
    // `PATH` entries and a relative prefix; if it cannot be read, those
    // entries cannot be judged, and a warning that might be wrong is
    // worse than none.
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let prefix_bin = prefix.join("bin");
    shadow::find_shadows(&path_var, &prefix_bin, bins, &cwd, shadow::is_executable)
        .iter()
        .map(|s| {
            let owner = shadow::owner_of(&s.existing);
            format!(
                "warning: {}",
                shadow::describe(s, &prefix_bin, owner.as_deref())
            )
        })
        .collect()
}

/// Will an install into `prefix` need privileged writes? The union over
/// everything the pipeline touches: binaries under bin, then the
/// manifest under the state directory — either alone can be the one
/// that needs sudo (mixed ownership: a user-writable bin next to a
/// root-owned share). One answer for the pipeline's own checkpoint
/// gating and — composed into `placement_needs_privilege` — the TUI
/// preflight, so the two cannot drift; a reauth
/// decision keyed to bin alone would skip exactly the case the state
/// write is about to hit. A missing state directory probes as writable
/// where the prefix allows creating it — `dir_writable` creates parents
/// as the user first — so a fresh custom prefix answers false here and
/// the lock preparation covers its own privileged case separately.
fn install_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    Ok(policy.probe_destination(&prefix.join("bin"))?
        || policy.probe_destination(&prefix.join("share/cargo-lbin"))?)
}

/// The state half of the escalation union: the manifest under the
/// state directory plus, where escalation is possible at all, the lock
/// file — the worker's first privileged touch. Its own question because
/// it is its own write set: a pin flip writes exactly this and nothing
/// under bin, so a read-only bin must not force a handoff for an
/// operation that never touches it — and on a custom prefix, where sudo
/// is forbidden, a bin probe's error must not refuse an operation the
/// CLI performs. The privilege check answers for what the operation
/// writes, not for the prefix as a whole.
#[cfg(feature = "tui")]
fn state_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    Ok(policy.probe_destination(&prefix.join("share/cargo-lbin"))?
        || (matches!(policy.sudo, privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(prefix)))
}

/// The escalation union for operations that place or remove under bin
/// — named for the write set it probes, after `operation_needs_privilege`
/// proved too broad a name the day pin arrived with a smaller one. The
/// build preflight, the in-place removal and the captured migration's
/// retirement warm-up must never disagree about whether a prefix asks a
/// password; private copies of this `||` would drift apart the day one
/// of them learns something. The terminal flavor never consults it —
/// there, sudo may simply ask.
#[cfg(feature = "tui")]
fn placement_needs_privilege(policy: privileged::Policy, prefix: &Path) -> Result<bool> {
    // Composed from the canonical bin + state probe, not re-spelled: a
    // change to install's write set reaches the TUI preflight through
    // this line, without anyone remembering to mirror it.
    Ok(install_needs_privilege(policy, prefix)?
        || (matches!(policy.sudo, privileged::Sudo::Allowed)
            && StateLock::preparation_needs_privilege(prefix)))
}

/// Insert `entry` and persist the manifest as one unit: on a failed store the
/// in-memory manifest is restored to what is on disk.
///
/// This invariant — the in-memory manifest always mirrors the last successful
/// commit — is what makes continuing a batch after a failure sound. Without
/// it, a store failure for crate A would leave A's new entry in memory, and
/// the next successful commit (for crate B) would persist A's entry for
/// binaries that were rolled back or never fully placed.
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

/// `pin`/`unpin` for a frontend that owns the screen: one crate, no
/// stdout, the outcome as data. The same nonblocking screen-owned lock
/// as `tui_remove_one`, for the same UI-thread reason; the same
/// semantics as `cmd_set_pinned` for one crate — an entry already in
/// the requested state is an answer, not a write.
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

/// `remove` for a frontend that owns the screen: one crate, no stdout,
/// the outcome as data — the UI owns the words. The exclusive lock is
/// taken nonblocking under the screen-owned policy: a blocking wait
/// here would freeze the interface with no redraw and no explanation,
/// so a held lock is an answer, not a wait. Escalation is the caller's
/// decision — by the time this runs the caller has concluded none is
/// needed, and the screen-owned policy holds everything below to that
/// (`sudo -n` at most, never a prompt beneath the alternate screen).
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

/// `migrate_one` for a frontend that owns the screen. The snapshot
/// arrives *frozen* from the frontend — built from the row at the
/// keypress, confirmed by the person, never re-taken here: a fresh
/// snapshot after the `y` could bless a version the person never saw,
/// and the existing checkpoint already rejects a source that moved on.
/// The migration runs through `MigrateFrontend::Captured` — the same
/// Build panel, cancel door, lock policy and sudo roundtrip as an
/// install. Returns the outcome as data; the UI owns the words.
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

/// One crate for the TUI, end to end: the same locking, pin refusal and
/// pipeline as `cmd_install`, for a single already-parsed spec, with a
/// captured frontend. The exclusive lock spans build and placement, as
/// it does on the CLI: serialization per prefix is a documented
/// invariant, not an implementation accident.
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
    // The same contract as placement: the one sudo this acquisition can
    // reach runs noninteractively, and its human lines — initializing
    // state, waiting on another instance — go through the frontend's
    // stream, not to a terminal the TUI currently owns.
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
    )
}

fn cmd_install(prefix: &Path, crates: &[String], locked: bool) -> Result<()> {
    // Parsed and de-duplicated by crate before anything else: the pin
    // check below runs once, against the manifest as it is now, so the
    // same crate must not appear twice in one command (see `parse_all`).
    let specs = InstallSpec::parse_all(crates)?;
    let cache = cache_dir()?;
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // A bare reinstall builds the newest version, which is exactly what a
    // pin forbids; refuse before the first build, naming every pinned
    // crate. Naming a version is different: `install foo@1.2.3` on a
    // pinned `foo` is the user re-pinning to that version, and is allowed.
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

/// Error if any of `crates` is pinned in `manifest`. Both `install` and
/// `update NAME` are explicit requests, but a pin is the more deliberate
/// and the more durable of the two statements, so it wins; the message
/// says how to change that.
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

/// `pin` / `unpin`: one manifest write for the whole selection. Already
/// in the requested state is reported, not an error — the user's wish
/// and the manifest agree, which is the point. "Already" is a statement
/// about what is on disk and can be said at once; "pinned X" is a
/// statement about what the store did and is said only after it did.
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
        // Statuses come from one source, never a blend: the recorded
        // `checkupdate` report (default), or a fresh query of just the
        // pinned crates (`--check`). The fresh result is deliberately not
        // persisted — the recorded snapshot belongs to `checkupdate`, and
        // a partial one covering only pins would misinform `list` about
        // everything else.
        let report = if check {
            Some(Report::new(
                prefix,
                check_versions(manifest.crates.iter().filter(|(_, e)| e.pinned))?,
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
    // The exit code answers one question — does any pinned crate have a
    // known newer version? A crate the report does not cover contributes
    // nothing: absence of knowledge is not an update, and the stderr
    // freshness line below is what points at the remedy.
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
            // The same three states as `list`, silent on the third: a
            // newer version known, known current, or not covered by the
            // report — printing nothing rather than guessing.
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
        // Three states, and the last must stay silent rather than
        // masquerade as either of the others: a newer version known, known
        // current, or not covered by the last check (installed or updated
        // since) — for which nothing is printed, because nothing is known.
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

/// Query the index for the given manifest entries and record the answer
/// for every one of them, current or not; network errors abort rather than
/// silently under-reporting. A crate the index offers nothing relevant for
/// (a stable install with only pre-releases published) counts as current:
/// there is nothing `update` would do for it.
fn check_versions<'a>(
    entries: impl IntoIterator<Item = (&'a String, &'a Entry)>,
) -> Result<Vec<Checked>> {
    let mut checked = Vec::new();
    for (name, entry) in entries {
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
    Ok(checked)
}

/// A release as `info` prints it: the version, flagged if yanked.
fn release_label(release: &index::Release) -> String {
    if release.yanked {
        format!("{} [yanked]", release.version)
    } else {
        release.version.to_string()
    }
}

/// Render one crate's `info` block. Two independent questions, two
/// sources: the `latest`/`pre-release` lines are published history and
/// may name a yanked release (flagged); the `installed` verdict is update
/// eligibility, computed from the non-yanked subset with the same
/// `latest_relevant` rules as `checkupdate`. Where `checkupdate` would
/// refuse the crate outright (nothing non-yanked left), the verdict says
/// so instead of claiming "up to date" — `info` must never assert
/// something `checkupdate` would contradict.
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

/// Read-only and explicitly network-bound, like `checkupdate`: the manifest
/// is snapshotted under a shared lock for the "installed" line, then every
/// query runs unlocked. Each name is independent — an unknown crate is
/// reported and the rest are still looked up; the exit code says whether
/// everything was found.
fn cmd_info(prefix: &Path, crates: &[String]) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let manifest = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    // Input order, first occurrence wins: the user asked in the order they
    // think about these crates and reads the answers in the same order.
    // (`update` sorts deliberately — there the order is a build sequence,
    // which should not depend on how the arguments were typed.)
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

/// Keyword search over crates.io, for choosing a name; `info` is where a
/// chosen name gets looked at properly. One API request, then the
/// manifest is read (shared lock, briefly) so hits already installed
/// under the prefix are marked — the one thing `cargo search` cannot
/// tell you. No results is an answer, not an error.
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

/// Generate a static completion script from the Clap command definition.
fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "cargo-lbin", &mut std::io::stdout());
}

/// How many older versions `downgrade` lists. Beyond that, the user
/// knows the number they want and `install NAME@VERSION` takes it.
const DOWNGRADE_CHOICES: usize = 10;

/// Interpret the answer to the version prompt: a 1-based number within
/// `count`, or nothing (Enter / `q`) to abort. Anything else is an error,
/// not a re-prompt — one question, one answer, and the command can be
/// run again.
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

/// Offer the older versions of an installed crate and install the one
/// chosen, pinned. The list comes from the index, filtered by the same
/// release-relevance policy `update` applies, here to versions older
/// than the installed one. Interactive by design — the point is not
/// knowing the number — so there is no `--yes`; a script that knows the
/// version has `install NAME@VERSION`.
fn cmd_downgrade(prefix: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    // Snapshot under a shared lock; the index query and the prompt run
    // unlocked, as in `update`. The choice is made against this
    // snapshot, and the install below re-checks it under the exclusive
    // lock: `install_and_commit` guarantees the chosen version lands,
    // but not that landing it is still a downgrade of anything.
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
    // The user chose relative to `current`; the operation is only a
    // downgrade if that is still what is installed. Removed meanwhile:
    // installing would resurrect the crate. Changed meanwhile: from
    // 1.0.0, installing the "older" 1.1.0 would be an upgrade under a
    // command called downgrade. Same rule as `update`'s per-crate check
    // after confirmation — a newer statement about the prefix wins over
    // an older plan. `--locked` is taken fresh for the same reason; the
    // pin need not match, since the result is pinned either way.
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
    )
}

fn cmd_checkupdate(prefix: &Path, json: bool) -> ExitCode {
    // Shared lock covers only the manifest snapshot; the index queries run
    // unlocked, so a slow crates.io cannot starve writers on the prefix.
    // Building the report is inside the fallible part: its only failure
    // is not being able to anchor a relative prefix, and a check whose
    // prefix cannot be named has nothing to persist or report.
    let outcome = (|| {
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        Report::new(prefix, check_versions(&manifest.crates)?)
    })();
    match outcome {
        Ok(report) => {
            // Persist the full snapshot for `list` (and any later reader)
            // before reporting. A failed write is a warning: the check
            // itself succeeded and its exit code must say so.
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
    // Phase 1: read-only snapshot under a shared lock, released before any
    // network-independent interaction. The confirmation prompt must not
    // hold any lock: an unanswered "proceed?" abandoned for a coffee break
    // would otherwise block every reader and writer on the prefix.
    //
    // Shared lock only for the snapshot; network runs unlocked. Phase 2
    // reloads and re-verifies anyway, so state changing during the
    // unlocked window is already handled.
    let snapshot = {
        let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
        Manifest::load(prefix)?
    };
    // Selection is validated against the snapshot before any network
    // traffic: a typo in a crate name must fail in milliseconds.
    // With `--all`, pinned crates are not part of the plan and are not
    // asked about: `check_versions` is all-or-error, and a pinned crate
    // whose lookup fails (yanked from the index, say) must not stop
    // every unpinned crate from updating. What a pin holds back is
    // `checkupdate`'s job to show; `update` plans mutation, and a pin
    // says this crate is not being mutated. Skipped ones are named, so
    // the hold is visible without a query.
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
    )?
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

/// The mutating half of `update`: exclusive lock, fresh manifest, and
/// each planned update verified against it before it is applied. The
/// world may have changed while the plan was being confirmed, so
/// anything that no longer matches the snapshot is skipped with a note
/// rather than acted on blindly.
fn apply_updates(prefix: &Path, cache: &Path, outdated: &[Checked]) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    // Each crate is its own unit of work: a failed build or placement is
    // reported, rolled back by `install_and_commit`, and the batch moves on.
    // The crates are independent (cargo install tracks no relation between
    // them), so aborting the rest on one failure would only leave more
    // binaries stale than necessary — while undoing successful ones would
    // throw away good work for no consistency gain.
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
                // The stage may end up building something newer than
                // `latest` if a release lands mid-update; the manifest
                // records what was built.
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
                    Ok(()) => updated += 1,
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
    // The command was asked for `total` updates; anything short of that is
    // an incomplete execution and exits non-zero, whether the shortfall was
    // a failed build or a crate the reload no longer recognized. The user
    // reads the exit code, not the reason, and "not done" is the fact.
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

/// The pin bit a committed entry ends up with. `install`'s inference is
/// a contract with `update --all` — an exact version that arrived
/// unpinned would be undone by the next run — but it is `install`'s
/// contract, not every caller's: migrate rebuilds an exact version
/// *because that is what the source has*, and its pin promise is "the
/// bit travels unchanged". The policy makes the final bit part of the
/// one manifest commit; correcting it with a second write afterwards
/// would open a window in which the entry is right and the intent is
/// wrong — and a failure of that second write would strand exactly the
/// state (destination present, wrongly pinned, source still standing)
/// the snapshot machinery exists to prevent.
#[derive(Clone, Copy)]
enum PinPolicy {
    /// `install`'s inference: an exact-version request pins; otherwise a
    /// pin already present is carried over — never dropped by a rewrite.
    Infer,
    /// The caller states the final bit outright.
    Exactly(bool),
}

/// Everything `migrate` promises to preserve about a source entry,
/// captured under the snapshot lock and revalidated twice on the way.
/// Built by exhaustively destructuring `manifest::Entry` on purpose: a
/// new manifest field fails this compilation and forces a decision
/// whether it belongs to the protected semantics of a migration —
/// silently ignoring it would be exactly the kind of guess this command
/// must never make about state it is about to delete.
pub(crate) struct MigrationSnapshot {
    version: Version,
    bins: Vec<String>,
    locked: bool,
    pinned: bool,
}

impl MigrationSnapshot {
    /// A snapshot from parts a frontend already holds — the TUI's row.
    /// This is how the plan a person confirms is *frozen*: built at the
    /// keypress, carried through the confirmation, and handed to the
    /// worker unchanged, so what was approved is what is revalidated —
    /// never a fresh snapshot taken after the `y`, which could bless a
    /// version the person never saw. Exhaustive by listing every field
    /// for the same reason `capture` destructures exhaustively.
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

/// How one migration ended, short of an error — where "error" ends at
/// the destination commit. Everything past that point is a partial
/// success, never a plain failure: the rebuilt install exists, and an
/// error message that hides it invites exactly the wrong reaction (a
/// blind re-run, which the already-installed refusal would then bounce).
#[derive(Debug)]
pub(crate) enum MigrateOutcome {
    /// Rebuilt at the destination; `already_retired` says whether the
    /// source entry was found already gone (someone retired it during
    /// the build — the goal state, reached by other hands).
    Moved { already_retired: bool },
    /// The destination committed, but the source was not retired —
    /// changed under the plan, or the retirement itself failed. Both
    /// installs (or the source's remainder) stand; the reason says what
    /// happened and the caller's message says what to do about it.
    Incomplete(String),
}
/// Phase 0 of `migrate`: the read-only plan. A snapshot under a shared
/// source lock, released before the prompt and the builds — an
/// unanswered "proceed?" must not block every reader and writer on the
/// prefix (`update`'s rule, for `update`'s reason). Everything decided
/// here is revalidated under real locks later, so state changing during
/// the unlocked window is already handled. Returns `None` when the
/// person aborts at the prompt.
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
        println!(
            "{name} {}: {} -> {}{}",
            snap.version,
            prefix.display(),
            to.display(),
            if snap.pinned { " [pinned]" } else { "" }
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
    // `Path` equality is component-wise, so trailing-slash spellings
    // collapse; symlinked spellings of the same place are the person's
    // to know, the same lexical stance the prefixes module documents.
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
    // Each crate is its own unit of work, `update --all`'s batch rule:
    // a failure is reported and the batch moves on — the crates are
    // independent, and undoing a finished migration would throw away
    // good work for no consistency gain.
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
            Ok(MigrateOutcome::Moved { already_retired }) => {
                // The words live with the caller: migrate_one reports
                // data, the CLI speaks CLI.
                if already_retired {
                    println!(
                        "migrated {name} {}: already retired from {}",
                        snap.version,
                        prefix.display()
                    );
                } else {
                    println!(
                        "migrated {name} {}: retired from {}",
                        snap.version,
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
) -> Result<()> {
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
            "`{name}` is already installed under {} at {}; migrate refuses to choose \
             between {} and {} — remove one side first (no --force by design)",
            dest.display(),
            existing.version,
            existing.version,
            snap.version
        );
    }
    // The early revalidation, run by the pipeline at the placement
    // door — after the build, before the destination commits
    // anything. Nonblocking and advisory: a busy source or a changed
    // entry aborts while aborting is still free (the stage is the
    // only casualty). The authoritative pass runs in phase B under
    // the real exclusive lock; this one only exists to not commit a
    // destination the source has already contradicted.
    // The probe's policy follows the flavor too: under a screen the
    // lock preparation may only run `sudo -n`. Its notices stay
    // silent in both shapes — an advisory read that would rather
    // say nothing, exactly as `try_acquire_with` documents.
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
    // `Exactly(snap.pinned)`: the pin bit travels unchanged, inside
    // the same manifest commit as the entry itself. Under `install`'s
    // inference an exact version would arrive pinned, and correcting
    // that with a second store would open a window — and a failure
    // mode — in which the destination is right and the intent is
    // wrong, with the source still standing and a re-run refused.
    // The same pipeline through the caller's shape: the CLI keeps
    // Checkpointed, the TUI gets its Build panel and cancel door by
    // composing the checkpoint behind Captured's placement CAS.
    match frontend {
        MigrateFrontend::Terminal => install_and_commit(
            dest,
            cache,
            &mut dest_manifest,
            name,
            Some(&snap.version),
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
            Some(&snap.version),
            snap.locked,
            PinPolicy::Exactly(snap.pinned),
            &mut Frontend::Captured {
                on_line: &mut **on_line,
                before_placement: &mut **before_placement,
                control,
                checkpoint: Some(&mut checkpoint),
            },
        )?,
    }
    Ok(())
}

/// One migration, sequential by design: destination first, source
/// second. The precise lock property — because "never two locks" would
/// be a lie by one probe: migrate never *waits* on one prefix while
/// holding a lock on the other, and never holds two exclusive locks;
/// the only overlap is the early revalidation's nonblocking shared
/// probe of the source, taken under the destination lock and refused
/// (`try_acquire`) rather than waited for. Holding two exclusive locks
/// for the length of a build would park every other instance on either
/// prefix behind a compilation, and two migrations in opposite
/// directions would need a lock order to not deadlock — a discipline
/// nothing enforces. Sequential locks trade all of that for one honest
/// window: between the destination commit and the source retirement the
/// crate exists in both prefixes — a state one `remove` fixes, and one
/// the listing annotates as `[also in …]` when both sides are the known
/// pair (`/usr/local`, `~/.local`). For custom prefixes the closed set
/// in the `prefixes` module cannot see the other side, so the durable
/// record of an incomplete migration is the command's own message,
/// which names both paths; the annotation is a bonus where it exists,
/// never the contract. The order is load-bearing —
/// the destination commits fully before the source loses anything, so
/// no failure or crash ever leaves the person without one complete,
/// working installation (a crash mid-retirement can leave the source
/// partial — entry still recorded, some binaries already gone — and
/// `remove`, built on `rm -f`, cleans up such a remainder).
fn migrate_one(
    source: &Path,
    dest: &Path,
    cache: &Path,
    name: &str,
    snap: &MigrationSnapshot,
    frontend: &mut MigrateFrontend<'_>,
) -> Result<MigrateOutcome> {
    rebuild_at_destination(source, dest, cache, name, snap, frontend)?;

    // Phase B: the source, under its exclusive lock — the authoritative
    // revalidation and the retirement. Nothing here is allowed to
    // surface as a plain error anymore: the destination has committed,
    // and from this line on every failure is an *incomplete migration*
    // whose message must lead with that fact — a bare "migrating foo
    // failed" would read as "nothing happened, run it again", and the
    // re-run would bounce off the already-installed refusal. Outcomes
    // carry data, not prose: the caller owns the words (the CLI prints,
    // the TUI composes), and the Incomplete reason is a payload like
    // Failed's error, not a success string baked two layers down.
    // Everything from here — the privilege probes included — is one
    // fallible unit, and every error in it maps to Incomplete: the
    // contract is "after the destination commit, every failure is an
    // incomplete migration", and a probe that fails a moment after that
    // commit is no exception just because it failed while *asking*
    // rather than *doing*.
    match retire_with_frontend(source, name, snap, frontend) {
        Ok(Retirement::Retired) => Ok(MigrateOutcome::Moved {
            already_retired: false,
        }),
        // Someone retired it during the build. The destination install
        // was explicitly asked for and stands; there is simply nothing
        // left to retire, which is the goal state.
        Ok(Retirement::AlreadyGone) => Ok(MigrateOutcome::Moved {
            already_retired: true,
        }),
        Ok(Retirement::Mismatch) => Ok(MigrateOutcome::Incomplete(format!(
            "`{name}` is installed at {} and stays: the entry under {} changed during the \
             migration, so the source is deliberately not retired — `remove` retires \
             whichever side is wrong",
            dest.display(),
            source.display()
        ))),
        Err(e) => Ok(MigrateOutcome::Incomplete(format!(
            "`{name}` is installed at {} and stays; retiring it from {} did not complete: \
             {e:#} — resolve that and `remove` the source installation, do not re-run the \
             migration blindly (it will refuse: the destination already has the crate)",
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
            // A retirement that will escalate — for the removal itself
            // or for preparing the lock file — revalidates credentials
            // first, through the same roundtrip an install's placement
            // uses: the build was long, sudo's timestamp may be stale,
            // and a screen-owned policy cannot prompt. One edge is
            // accepted deliberately: the revalidation runs *before* the
            // blocking source lock below, so a wait on someone else's
            // ten-minute build can outlive the freshly warmed timestamp
            // and the `sudo -n` removal ends as Incomplete — safe, and
            // exactly what Incomplete's message covers; guaranteeing
            // freshness after an arbitrarily long wait would need the
            // NeedAuth roundtrip *under* the lock, a hostage-taking not
            // worth the edge.
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
/// against a manifest freshly loaded under it, then the removal. All
/// errors bubble; the caller owns the "destination already committed"
/// framing, because only it knows that context.
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
        // Cancel first: the placement door refuses, and the refusal is
        // the typed cancellation, not an anonymous error. A second
        // cancel with no live group (nothing spawned here) reports
        // "already stopping" rather than claiming a SIGKILL nobody sent
        // — the Killed escalation needs a live group and is exercised
        // end to end by `a_second_cancel_kills_a_term_ignoring_build`.
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

        // A cancel accepted before the spawn is not lost: `spawned`
        // still reports Cancelling afterwards, which is what routes the
        // deferred SIGTERM (exercised here without a live process — the
        // pgid slot merely records; `spawned` on a cancelled control
        // signals the group, and signalling is best-effort by design).
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

        // The reason the sweep exists: the leader dies politely on
        // SIGTERM, but a group member ignoring TERM survives it — and
        // the group id stays alive with any member, so "the leader was
        // reaped" must not be read as "the group is gone". The stray
        // records its own pid so the test can watch it die.
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

        // The nastiest stray: it writes a *fragment* — no newline — to
        // the inherited stderr and then just holds it. The poll reports
        // readable, read_until consumes the fragment and walks straight
        // into a blocking read hunting for the newline, past the loop's
        // cancel check; the first cancel then kills only the leader and
        // the worker stays wedged. The escalation — driven manually
        // here; the run loop's grace timer fires the very same call —
        // SIGKILLs the group, the stray dies, the pipe closes, and the
        // worker unwedges into the typed cancellation.
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
        // Give SIGTERM time to take the leader while the worker sits
        // wedged in read_until — the exact state the escalation exists
        // for. The leader is an unreaped zombie, so the group id is
        // certainly alive and the escalation must find it.
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
        // Give the worker time to spawn the fake; a cancel landing even
        // earlier is also correct (`spawned` delivers it), it just would
        // not exercise the running-build path this test is about.
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
            outcome,
            MigrateOutcome::Moved {
                already_retired: false
            }
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
        // `install_and_commit` pins every exact-version request — its
        // contract, corrected by migrate under the same lock. This is
        // the test that keeps that correction honest.
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
            "an unpinned crate arrives unpinned, not auto-pinned by the exact version"
        );
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

        // A fake cargo that stages fine — and mutates the source
        // manifest mid-build, exactly the race the early revalidation
        // exists to catch before the destination commits anything.
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
            outcome,
            MigrateOutcome::Moved {
                already_retired: false
            }
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
        // The review scenario: the person confirms 1.2.0, another
        // process updates the crate before the worker runs. The frozen
        // snapshot travels; the checkpoint rejects; nothing migrates —
        // never "confirmed one version, migrated another".
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
        // The same RAII guard as the stage tests: the fake is cleared on
        // drop, panics included, so this test cannot leave it behind to
        // answer an unrelated build elsewhere in the run.
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
