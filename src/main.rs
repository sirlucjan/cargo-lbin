mod api;
mod index;
mod json;
mod lock;
mod manifest;
mod privileged;
mod report;
mod shadow;
mod stage;
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
    /// Installation prefix; binaries land in <prefix>/bin
    // Precedence: explicit --prefix, then $CARGO_LBIN_PREFIX, then
    // /usr/local — clap's env support handles the ordering and appends
    // the [env: ...] and [default: ...] annotations to --help on its
    // own. The point: a user who never wants sudo exports
    // CARGO_LBIN_PREFIX=~/.local once (expanded by the shell) and stops
    // typing --prefix on every command.
    #[arg(
        long,
        global = true,
        env = "CARGO_LBIN_PREFIX",
        default_value = "/usr/local"
    )]
    prefix: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build crates from crates.io and install their binaries.
    /// `NAME@VERSION` installs exactly that version and pins it
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
    /// Hold crates at their installed version: excluded from `update
    /// --all`, refused by `update NAME` and `install NAME` until unpinned
    Pin {
        #[arg(required = true)]
        crates: Vec<String>,
    },
    /// Release a pin
    Unpin {
        #[arg(required = true)]
        crates: Vec<String>,
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
    let cli = Cli::parse_from(args);
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
        Cmd::List { json } => cmd_list(&cli.prefix, json),
        #[cfg(feature = "tui")]
        Cmd::Tui => tui::run(&cli.prefix),
        Cmd::Info { ref crates } => cmd_info(&cli.prefix, crates),
        Cmd::Search { ref query, limit } => cmd_search(&cli.prefix, query, limit),
        Cmd::Checkupdate { json } => return cmd_checkupdate(&cli.prefix, json),
        Cmd::Downgrade { ref name } => cmd_downgrade(&cli.prefix, name),
        Cmd::Update {
            ref crates,
            all,
            yes,
        } => cmd_update(&cli.prefix, crates, all, yes),
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
fn rollback_new_bins(policy: privileged::Escalation, placed: &[PathBuf]) {
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
fn install_and_commit(
    prefix: &Path,
    cache: &Path,
    manifest: &mut Manifest,
    name: &str,
    version: Option<&Version>,
    locked: bool,
) -> Result<()> {
    // Revalidate even though CLI input was already checked: on the update
    // path `name` comes from the manifest, and a hand-edited manifest must
    // not be able to steer the remove_dir_all below via a path-like name.
    validate_name(name)?;
    let policy = privileged::Escalation::for_prefix(prefix);
    // UX-only early form of the policy check: fail before a multi-minute
    // build, not after. Enforcement proper lives at every privileged call
    // site via `Escalation`; this merely surfaces the same refusal sooner.
    let _ = policy.probe_destination(&prefix.join("bin"))?;
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
    let built = stage::build(name, version, locked, &stage_dir)?;
    check_collisions(manifest, name, &built.bins, &prefix.join("bin"))?;
    // Only for names this crate did not provide before: on a first
    // install that is every binary; on an update it is the ones the new
    // version adds (`foo` 2.0 shipping a `fooctl` that 1.0 did not),
    // which were never checked and may well exist in `/usr/bin`. Names
    // carried over were reported when they were new.
    let new_bins: Vec<String> = built
        .bins
        .iter()
        .filter(|b| !manifest.crates.get(name).is_some_and(|e| e.bins.contains(b)))
        .cloned()
        .collect();
    warn_shadows(prefix, &new_bins);

    // Snapshot before `place_and_commit` inserts the new manifest entry;
    // see `RollbackSet::snapshot` for why the order is load-bearing.
    let mut rollback = RollbackSet::snapshot(manifest, name, &built.bins);
    // An exact version was chosen to be kept: the entry is pinned, or the
    // next `update --all` would undo the choice. Without one, a pin
    // already present is carried over (see below).
    let pin = version.is_some();
    if let Err(err) =
        place_and_commit(prefix, policy, manifest, name, built, locked, pin, &mut rollback)
    {
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
    policy: privileged::Escalation,
    manifest: &mut Manifest,
    name: &str,
    built: stage::Built,
    locked: bool,
    pin: bool,
    rollback: &mut RollbackSet,
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
            println!("removed obsolete binaries: {}", obsolete.join(", "));
        }
    }

    let bins_list = built.bins.join(", ");
    // Pinned if this install chose a version, or if the entry was
    // pinned before. When `pin` is false the carried-over value can only
    // be false too — `install` and `update` refuse pinned crates unless
    // a version is named — but a pin is not something a rewrite of the
    // entry gets to drop by omission. When `pin` is true (a re-pin over
    // an already pinned crate), both agree. Read before the call: the
    // first argument borrows `manifest` mutably, and a plain function
    // call gets no two-phase borrow for a later argument.
    let pinned = pin || manifest.crates.get(name).is_some_and(|e| e.pinned);
    commit_entry(
        manifest,
        prefix,
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
    let pin_note = if pin {
        format!(" [pinned; `cargo lbin unpin {name}` to allow updates]")
    } else {
        String::new()
    };
    println!(
        "installed {name} {} -> {} ({bins_list}){pin_note}",
        built.version,
        bin_dir.display(),
    );
    Ok(())
}

/// One stderr line per binary that a `PATH` entry outside the prefix
/// already provides — usually a distribution package — naming the file,
/// its owner if the package manager will say, and which of the two
/// directories comes first in `PATH`. A warning only; see `shadow` for why it is not a
/// refusal. Given only for names new to this crate: a distro package
/// that appears *after* ours took the name is a collision that arose
/// outside cargo-lbin, and repeating the warning on every update would
/// be the price of catching it.
fn warn_shadows(prefix: &Path, bins: &[String]) {
    if bins.is_empty() {
        return;
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return;
    };
    // The scan needs the working directory only to anchor relative
    // `PATH` entries and a relative prefix; if it cannot be read, those
    // entries cannot be judged, and a warning that might be wrong is
    // worse than none.
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let prefix_bin = prefix.join("bin");
    for s in shadow::find_shadows(&path_var, &prefix_bin, bins, &cwd, shadow::is_executable) {
        let owner = shadow::owner_of(&s.existing);
        eprintln!(
            "warning: {}",
            shadow::describe(&s, &prefix_bin, owner.as_deref())
        );
    }
}

/// Insert `entry` and persist the manifest as one unit: on a failed store the
/// in-memory manifest is restored to what is on disk.
///
/// This invariant — the in-memory manifest always mirrors the last successful
/// commit — is what makes continuing a batch after a failure sound. Without
/// it, a store failure for crate A would leave A's new entry in memory, and
/// the next successful commit (for crate B) would persist A's entry for
/// binaries that were rolled back or never fully placed.
fn commit_entry(manifest: &mut Manifest, prefix: &Path, name: &str, entry: Entry) -> Result<()> {
    let previous = manifest.crates.insert(name.to_owned(), entry);
    if let Err(err) = manifest.store(prefix) {
        if let Some(old) = previous {
            manifest.crates.insert(name.to_owned(), old);
        } else {
            manifest.crates.remove(name);
        }
        return Err(err);
    }
    Ok(())
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

fn cmd_remove(prefix: &Path, crates: &[String]) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let policy = privileged::Escalation::for_prefix(prefix);
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
        let output = json::ListOutput::build(report::identity(prefix)?, &manifest, report.as_ref());
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
        println!(
            "{name} {}{locked}{pinned} ({}){status}",
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
        bail!("downgrade asks which version to install; without a terminal, use `cargo lbin install {name}@VERSION`");
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
    install_and_commit(prefix, &cache, &mut manifest, name, Some(version), locked)
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
        println!("{name} {} [pinned, skipped]", snapshot.crates[*name].version);
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
                match install_and_commit(prefix, cache, &mut manifest, &o.name, None, locked) {
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
        assert!(commit_entry(&mut m, &prefix, "foo", entry("2.0.0")).is_err());
        assert_eq!(m.crates["foo"].version, "1.0.0");
        // Fresh install: the name must disappear again.
        let mut m = Manifest::default();
        assert!(commit_entry(&mut m, &prefix, "foo", entry("2.0.0")).is_err());
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
}
