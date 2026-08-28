mod index;
mod lock;
mod manifest;
mod privileged;
mod stage;
mod validate;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lock::{Mode, StateLock};
use manifest::{Entry, Manifest};
use semver::Version;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use validate::validate_name;

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
    /// Build crates from crates.io and install their binaries
    Install {
        #[arg(required = true)]
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
    /// List installed crates and their binaries
    List,
    /// Check crates.io for newer versions (read-only, no sudo).
    /// Exit codes: 0 updates available, 2 none, 1 error
    Checkupdate,
    /// Install all available updates
    Update {
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
        Cmd::List => cmd_list(&cli.prefix),
        Cmd::Checkupdate => return cmd_checkupdate(&cli.prefix),
        Cmd::Update { yes } => cmd_update(&cli.prefix, yes),
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
    let built = stage::build(name, locked, &stage_dir)?;
    check_collisions(manifest, name, &built.bins, &prefix.join("bin"))?;

    // Snapshot before `place_and_commit` inserts the new manifest entry;
    // see `RollbackSet::snapshot` for why the order is load-bearing.
    let mut rollback = RollbackSet::snapshot(manifest, name, &built.bins);
    if let Err(err) = place_and_commit(prefix, policy, manifest, name, built, locked, &mut rollback)
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
fn place_and_commit(
    prefix: &Path,
    policy: privileged::Escalation,
    manifest: &mut Manifest,
    name: &str,
    built: stage::Built,
    locked: bool,
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
    manifest.crates.insert(
        name.to_owned(),
        Entry {
            version: built.version.to_string(),
            bins: built.bins,
            locked,
        },
    );
    manifest.store(prefix)?;
    // Announced only after the manifest commit: with a rollback path in
    // play, an "installed" printed before `store` could be followed by that
    // very installation being undone.
    println!(
        "installed {name} {} -> {} ({bins_list})",
        built.version,
        bin_dir.display(),
    );
    Ok(())
}

fn cmd_install(prefix: &Path, crates: &[String], locked: bool) -> Result<()> {
    for name in crates {
        validate_name(name)?;
    }
    let cache = cache_dir()?;
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    for name in crates {
        install_and_commit(prefix, &cache, &mut manifest, name, locked)?;
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

fn cmd_list(prefix: &Path) -> Result<()> {
    let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
    let manifest = Manifest::load(prefix)?;
    if manifest.crates.is_empty() {
        println!("no crates installed under {}", prefix.display());
        return Ok(());
    }
    for (name, entry) in &manifest.crates {
        let locked = if entry.locked { " [locked]" } else { "" };
        println!(
            "{name} {}{locked} ({})",
            entry.version,
            entry.bins.join(", ")
        );
    }
    Ok(())
}

struct Outdated {
    name: String,
    current: Version,
    latest: Version,
}

/// Query the index for every manifest entry; network errors abort rather
/// than silently under-reporting.
fn find_outdated(manifest: &Manifest) -> Result<Vec<Outdated>> {
    let mut outdated = Vec::new();
    for (name, entry) in &manifest.crates {
        let current = Version::parse(&entry.version)
            .with_context(|| format!("manifest holds unparsable version for `{name}`"))?;
        let versions = index::published_versions(name)?;
        let Some(latest) = index::latest_relevant(&versions, &current) else {
            continue;
        };
        if latest > current {
            outdated.push(Outdated {
                name: name.clone(),
                current,
                latest,
            });
        }
    }
    Ok(outdated)
}

fn cmd_checkupdate(prefix: &Path) -> ExitCode {
    // Shared lock covers only the manifest snapshot; the index queries run
    // unlocked, so a slow crates.io cannot starve writers on the prefix.
    let outcome = (|| {
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        find_outdated(&manifest)
    })();
    match outcome {
        Ok(outdated) if outdated.is_empty() => ExitCode::from(EXIT_NO_UPDATES),
        Ok(outdated) => {
            for o in &outdated {
                println!("{} {} -> {}", o.name, o.current, o.latest);
            }
            ExitCode::from(EXIT_UPDATES)
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

fn cmd_update(prefix: &Path, yes: bool) -> Result<()> {
    let cache = cache_dir()?;
    // Phase 1: read-only snapshot under a shared lock, released before any
    // network-independent interaction. The confirmation prompt must not
    // hold any lock: an unanswered "proceed?" abandoned for a coffee break
    // would otherwise block every reader and writer on the prefix.
    let outdated = {
        // Shared lock only for the snapshot; network runs unlocked. Phase 2
        // reloads and re-verifies anyway, so state changing during the
        // unlocked window is already handled.
        let manifest = {
            let _lock = StateLock::acquire(prefix, &Mode::Shared)?;
            Manifest::load(prefix)?
        };
        find_outdated(&manifest)?
    };
    if outdated.is_empty() {
        println!("everything is up to date");
        return Ok(());
    }
    for o in &outdated {
        println!("{} {} -> {}", o.name, o.current, o.latest);
    }
    if !yes && !confirm("proceed with update?")? {
        println!("aborted");
        return Ok(());
    }
    // Phase 2: exclusive. The world may have changed while we were talking,
    // so reload and verify each planned update against the fresh manifest;
    // anything that no longer matches the snapshot is skipped with a note
    // rather than acted on blindly.
    let _lock = StateLock::acquire(prefix, &Mode::Exclusive)?;
    let mut manifest = Manifest::load(prefix)?;
    for o in &outdated {
        match manifest.crates.get(&o.name) {
            Some(entry) if entry.version == o.current.to_string() => {
                let locked = entry.locked;
                // The stage may end up building something newer than
                // `latest` if a release lands mid-update; the manifest
                // records what was built.
                install_and_commit(prefix, &cache, &mut manifest, &o.name, locked)?;
            }
            _ => eprintln!(
                "skipping `{}`: state changed since the update was confirmed",
                o.name
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
