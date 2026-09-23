// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only state audit (`verify`) and opt-in cache cleaning (`clean`).
//!
//! The two share the stale-stage scan; cleaning is always opt-in, and
//! leased-stage removal takes the lease again before deletion. Findings
//! are data: the text surfaces read only `message`, a `--json` consumer
//! gets the fields raw.

use crate::hints::reinstall_hint;
use crate::lock::StateLock;
use crate::manifest::{Entry, Manifest};
use crate::validate::validate_name;
use crate::{cache_dir, prefixes, shadow, stage, text, validate};
use anyhow::{Context, Result, bail};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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
/// repair only where lbin has an unambiguous one. It never modifies
/// managed state or filesystem contents; for lease-aware build stages
/// it may briefly acquire and immediately release a *shared* lease,
/// solely to determine liveness — a lock no writer can mistake for
/// ownership and no fellow probe can mistake for a writer.
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
                    "{} stage director{} under {} with no live owner — possible \
                     leftover build debris; inspect, then `cargo lbin clean --stages` \
                     when safe (for pre-lease stages the owner test is a PID \
                     heuristic: a PID can be reused, and an orphaned build may \
                     still hold the directory)",
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

/// The verify-side sibling of `shadow::notes`: the same scan, but each
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
pub(crate) fn verify_entries(prefix: &Path, manifest: &Manifest) -> (Vec<Finding>, Vec<String>) {
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

/// Stage directories with no live owner, and the namespace is part of
/// the format: a name is a run only where its format lives — bare
/// `<pid>` in `stage/`, `<pid>-<nonce>` in `stage-v2/`; cross-namespace
/// names and non-parsing names are debris. Leased runs (kept on
/// failure) answer through their lease, and only a released lease
/// convicts — held is a live writer; a missing or unreadable lease,
/// or a run path that is not a real directory (a symlink is probed as
/// unknown, never followed), is spared. Legacy stages keep the /proc
/// heuristic with both of its known lies: a reused PID resurrects a
/// dead stage, and an orphaned cargo may outlive the cargo-lbin that
/// spawned it and still hold the directory. The one definition of
/// ownerless, shared by `verify` and `clean` (for whom this is
/// candidate selection — its removal license is the exclusive lease
/// take);
/// error *policy* is the caller's: read errors come back unflattened,
/// verify silences them (read-only, a possibly-wrong warning is worse
/// than none), clean propagates them (a mutating command must not
/// report success over a cache it could not read). `NotFound` is an
/// empty cache for both.
pub(crate) fn scan_stale_stages(cache: &Path) -> std::io::Result<Vec<PathBuf>> {
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
            // Liveness for this layout is the lease's to answer, and
            // only "released" convicts: held is a live writer, unknown
            // (no lease, or one that would not open or lock — a
            // symlinked run path among them) is the creation window or
            // worse and proves nothing. A wrong "stale" is a deletable
            // lie; a wrong "leave it" costs disk until the next pass.
            (true, Some(stage::StageRun::LeasedRun { .. })) => {
                stage::probe_lease(&entry.path()) == stage::LeaseState::Released
            }
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

/// One candidate's fate, and the summary is built from exactly these —
/// a deferral is not a failure, but it is not a removal either, and
/// "removed 2" over a directory still standing would be the summary
/// lying about the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveOutcome {
    /// Gone, by this pass's hand (or named, under `--dry-run`).
    Removed,
    /// Left standing on purpose: the lease was held this instant.
    Deferred,
    /// Gone before this pass reached it — a racing clean's hand.
    AlreadyGone,
    /// The one outcome that flips the exit code.
    Failed,
}

/// One candidate's removal. Two disciplines by layout. Legacy runs
/// have nothing to take, so the heuristic's answer is all there is.
/// Leased runs get the real thing: take `LOCK_EX` and hold it through
/// the whole removal — the take, not the scan, is the removal license,
/// which is what closes the check-then-delete race.
fn remove_stale_stage(dir: &Path, dry_run: bool) -> RemoveOutcome {
    let verb = if dry_run { "would remove" } else { "removing" };
    // The lease discipline applies exactly where a lease can legally
    // exist: a leased-format name inside stage-v2. A leased name in
    // stage/ is cross-namespace debris — nothing legally writes one
    // there, so there is no lease to take, and sending it through the
    // take would let a NotFound read as "already removed" and spare
    // the debris forever.
    let leased = dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == stage::RUN_NAMESPACE)
        && dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(stage::parse_run_dir)
            .is_some_and(|r| matches!(r, stage::StageRun::LeasedRun { .. }));
    if dry_run {
        println!("{verb} ownerless stage {}", dir.display());
        return RemoveOutcome::Removed;
    }
    let held = if leased {
        match stage::take_lease_for_removal(dir) {
            Ok(Some(held)) => Some(held),
            // Held this instant — a writer still exiting, or a verify
            // probe passing through. The safe side of the race: skip,
            // say so, and let a later pass answer.
            Ok(None) => {
                println!("deferring {}: its lease is held right now", dir.display());
                return RemoveOutcome::Deferred;
            }
            // Already removed by a racing clean: nothing left to do
            // here counts as done, not as failed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("skipping {}: already removed", dir.display());
                return RemoveOutcome::AlreadyGone;
            }
            // A mutating command must not report success over a lease
            // it could not judge.
            Err(e) => {
                eprintln!("error: taking the lease of {}: {e}", dir.display());
                return RemoveOutcome::Failed;
            }
        }
    } else {
        None
    };
    println!("{verb} ownerless stage {}", dir.display());
    // Leased runs go payload-first, lease-last — see remove_leased_run:
    // a failed removal leaves the run still wearing its lease, so it
    // stays visible to the next pass instead of becoming a lease-less
    // unknown spared forever. Legacy stages have no lease to keep for
    // last.
    let removal = if held.is_some() {
        stage::remove_leased_run(dir)
    } else {
        fs::remove_dir_all(dir)
    };
    if let Err(e) = removal {
        eprintln!("error: removing {}: {e}", dir.display());
        return RemoveOutcome::Failed;
    }
    // `held` drops here, after the removal: the lock outlives the
    // delete, never the other way around.
    drop(held);
    RemoveOutcome::Removed
}

/// The mutating half of the pair `verify` opens: `verify` names the
/// debris read-only, `clean` removes it — through the very same
/// `scan_stale_stages`, so the two can never disagree on what debris
/// is. For leased runs the scan is only candidate selection: the
/// removal license is the exclusive take of the run's lease, held
/// through the whole removal.
/// Old failure logs join in: they are written on every failed build
/// and nothing else ever prunes them. The cache is the user's own —
/// no prefix lock, no sudo; a PID alive on *any* prefix's build is
/// spared by the liveness test itself.
pub(crate) fn cmd_clean(
    dry_run: bool,
    stages: bool,
    logs_older_than_days: Option<u64>,
) -> Result<()> {
    clean_cache(&cache_dir()?, dry_run, stages, logs_older_than_days)
}

pub(crate) fn clean_cache(
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
    // function, so diagnosis and cleanup cannot drift. For leased runs
    // that answer is candidate selection and the exclusive lease take
    // is the removal license; for legacy stages it is still only a
    // liveness heuristic over the owning cargo-lbin — an orphaned cargo
    // may survive its parent and still hold the directory — which is
    // why --stages is opt-in and this loop stays behind it. Unlike
    // verify (read-only, silence over a possibly-wrong warning), a
    // mutating command must not report success over a cache it could
    // not read: NotFound is an empty cache, every other read error is
    // an error.
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
    let (mut removed, mut deferred) = (0usize, 0usize);
    for dir in &stale {
        match remove_stale_stage(dir, dry_run) {
            RemoveOutcome::Removed => removed += 1,
            RemoveOutcome::Deferred => deferred += 1,
            // Gone is the goal either way; whose hand got there first
            // is a per-item line, not a summary category.
            RemoveOutcome::AlreadyGone => {}
            RemoveOutcome::Failed => failures += 1,
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
        // The summary counts what happened, never the candidate list:
        // "removed 2" over a deferred directory still standing would
        // be the summary lying about the filesystem.
        parts.push(format!("{removed} ownerless stage(s)"));
        if deferred > 0 {
            parts.push(format!("{deferred} deferred (lease held)"));
        }
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
pub(crate) fn cmd_verify(prefix: &Path, json: bool) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::seeded_prefix;

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
            errors[0].message.contains("install --reinstall okcrate"),
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
                built_with_rustc: None,
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
                built_with_rustc: None,
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
                built_with_rustc: None,
            },
        );
        manifest.crates.insert(
            "weird".into(),
            Entry {
                version: "0.1.0".into(),
                bins: vec!["../outside".into(), "good".into(), "good".into()],
                locked: false,
                pinned: false,
                built_with_rustc: None,
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
        assert!(
            hint.starts_with("cargo lbin install --reinstall okcrate"),
            "{hint}"
        );
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

    /// The race the exclusive take exists for, in test form. A reader
    /// (a concurrent verify probe, frozen mid-flight) holds `LOCK_SH`:
    /// the scan still classifies the run as released — readers coexist
    /// by design — but clean's `LOCK_EX` is refused, so the pass skips it
    /// without failing, and the pass after the reader leaves removes
    /// it. The safe side of the race, chosen by the lock mode.
    #[test]
    fn clean_takes_the_lease_and_defers_to_a_reader_inside() {
        use std::os::fd::AsRawFd;
        let _serial = crate::stage::no_spawned_children();
        let root = std::env::temp_dir().join("cargo-lbin-test-clean-lease");
        let _ = fs::remove_dir_all(&root);
        let cache = root.join("cache");
        let released = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join("1-00000000000000aa");
        let probed = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join("1-00000000000000bb");
        crate::stage::released_run_fixture(&released);
        crate::stage::released_run_fixture(&probed);

        // The frozen verify probe: a shared lock held across clean.
        let reader = fs::File::open(probed.join(".lease")).unwrap();
        // SAFETY: flock(2) on an owned, open descriptor.
        assert_eq!(
            unsafe { libc::flock(reader.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );

        // Both are scan candidates: the reader is not a writer.
        let named = scan_stale_stages(&cache).unwrap();
        assert!(named.contains(&released) && named.contains(&probed));

        // The fates, asked one candidate at a time — these are the
        // counts the summary is built from, so "removed" and
        // "deferred" must never blur into one number.
        assert_eq!(
            remove_stale_stage(&probed, false),
            RemoveOutcome::Deferred,
            "a reader inside is a deferral, not a removal and not a failure"
        );
        assert!(probed.exists(), "a deferred run is left standing");

        // One removed, one deferred — and deferring is not failing.
        clean_cache(&cache, false, true, None).unwrap();
        assert!(!released.exists(), "a released run is taken and removed");
        assert!(
            probed.exists(),
            "a reader inside defers the removal to a later pass"
        );
        assert_eq!(
            remove_stale_stage(&released, false),
            RemoveOutcome::AlreadyGone,
            "a candidate gone before this pass is done, not failed"
        );

        drop(reader);
        assert!(
            crate::stage::eventually(std::time::Duration::from_secs(10), || {
                clean_cache(&cache, false, true, None).unwrap();
                !probed.exists()
            }),
            "the pass after the reader leaves finishes the job"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The whole pipeline against the top-level symlink bait: a valid
    /// run name in stage-v2 linking to a directory that contains an
    /// unlocked `.lease` — exactly what a followed link would read as
    /// "released" and then remove. The scan must spare it (unknown),
    /// clean must leave both the link and its target untouched, and a
    /// cross-namespace leased name in stage/ must go through the plain
    /// debris path and actually be removed.
    #[test]
    fn clean_never_follows_a_symlinked_run_and_sweeps_cross_namespace_debris() {
        let root = std::env::temp_dir().join("cargo-lbin-test-clean-toplink");
        let _ = fs::remove_dir_all(&root);
        let cache = root.join("cache");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("keep")).unwrap();
        fs::write(outside.join(".lease"), b"").unwrap();
        let linked = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join("77-00000000000000ee");
        fs::create_dir_all(cache.join(crate::stage::RUN_NAMESPACE)).unwrap();
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        // Cross-namespace debris: a leased name in stage/, no lease
        // discipline owed — the plain path must remove it.
        let crosswise = cache.join("stage").join("77-00000000000000dd");
        fs::create_dir_all(&crosswise).unwrap();

        clean_cache(&cache, false, true, None).unwrap();
        assert!(
            fs::symlink_metadata(&linked).is_ok(),
            "the symlinked run is unknown: spared, not a candidate"
        );
        assert!(
            outside.join("keep").exists() && outside.join(".lease").exists(),
            "zero traversal: the link's target is untouched by clean"
        );
        assert!(
            !crosswise.exists(),
            "cross-namespace debris goes through the plain path and is removed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_stale_stages_reports_dead_pids_and_spares_the_living() {
        let _serial = crate::stage::no_spawned_children();
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-stages");
        let _ = fs::remove_dir_all(&root);
        let cache = root.join("cache");

        // No stage directory at all: silence, not an error.
        assert_eq!(scan_stale_stages(&cache).unwrap(), Vec::<PathBuf>::new());

        // A live PID (ours), a PID /proc cannot know, a name that is
        // not a PID at all — and, in the v2 namespace, one leased run
        // per probe answer, junk, both cross-namespace shapes and a
        // symlinked bait. The leased runs' PIDs are all dead on
        // purpose: /proc must have no vote in stage-v2.
        let live = cache.join("stage").join(std::process::id().to_string());
        let dead = cache.join("stage").join(u32::MAX.to_string());
        let junk = cache.join("stage").join("not-a-pid");
        let junk_v2 = cache.join(crate::stage::RUN_NAMESPACE).join("not-a-run");
        // Unknown: the run name is published but .lease is not there —
        // the creation window, frozen.
        let window = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(format!("{}-0123456789abcdef", u32::MAX));
        // Cross-namespace names: a LIVE bare PID in stage-v2 (so /proc
        // could only spare it — proving it gets no vote there), and a
        // leased name in stage/ where nothing legally writes one.
        let legacy_in_v2 = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(std::process::id().to_string());
        let leased_in_legacy = cache
            .join("stage")
            .join(format!("{}-00000000000000cd", u32::MAX));
        // Held: a lease this test keeps alive across the scan.
        let held = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(format!("{}-00000000000000aa", u32::MAX));
        // Released: a lease acquired and dropped — the one answer that
        // convicts.
        let released = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(format!("{}-00000000000000bb", u32::MAX));
        // A symlinked "run" wearing a valid name, pointing at a
        // directory with an unlocked lease: the probe must answer
        // unknown, never follow.
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("keep")).unwrap();
        fs::write(outside.join(".lease"), b"").unwrap();
        let linked = cache
            .join(crate::stage::RUN_NAMESPACE)
            .join(format!("{}-00000000000000ce", u32::MAX));
        fs::create_dir_all(cache.join(crate::stage::RUN_NAMESPACE)).unwrap();
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        for d in [
            &live,
            &dead,
            &junk,
            &window,
            &junk_v2,
            &legacy_in_v2,
            &leased_in_legacy,
        ] {
            fs::create_dir_all(d).unwrap();
        }
        let _holder = crate::stage::Lease::acquire(&held).unwrap();
        crate::stage::released_run_fixture(&released);
        let stale = scan_stale_stages(&cache).unwrap();
        assert!(
            !stale.contains(&live),
            "a running instance's stage is not debris"
        );
        assert!(stale.contains(&dead), "a dead PID's stage is debris");
        assert!(stale.contains(&junk), "a non-PID name is debris");
        assert!(
            !stale.contains(&window),
            "a run without a lease is unknown, and unknown is spared"
        );
        assert!(
            !stale.contains(&held),
            "a held lease is a live writer, dead PID or not"
        );
        assert!(
            stale.contains(&released),
            "a released lease is the one answer that convicts"
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
        assert!(
            !stale.contains(&linked),
            "a symlinked run path probes as unknown: spared, never followed"
        );
        assert!(
            outside.join("keep").exists() && outside.join(".lease").exists(),
            "zero traversal: the link's target is untouched by the scan"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// One remedy for every entry, whatever the entry says. `--reinstall`
    /// reads the pin, the version and `--locked` itself, so the hint
    /// does not restate them — and cannot drift from them.
    #[test]
    fn the_repair_hint_reads_the_entry_instead_of_restating_it() {
        let root = std::env::temp_dir().join("cargo-lbin-test-verify-remedy");
        let _ = fs::remove_dir_all(&root);
        let prefix = seeded_prefix(&root, "prefix", "okcrate", false, false);
        fs::remove_file(prefix.join("bin/okcrate")).unwrap();

        let mut pinned = Manifest::load(&prefix).unwrap();
        pinned.crates.get_mut("okcrate").unwrap().pinned = true;
        let (errors, _) = verify_entries(&prefix, &pinned);
        assert!(
            errors[0].message.contains("install --reinstall okcrate"),
            "the pinned remedy rebuilds the entry: {errors:?}"
        );
        assert!(
            !errors[0].message.contains("okcrate@"),
            "and does not name a version the entry already holds: {errors:?}"
        );

        let mut locked = Manifest::load(&prefix).unwrap();
        locked.crates.get_mut("okcrate").unwrap().locked = true;
        let (errors, _) = verify_entries(&prefix, &locked);
        assert!(
            !errors[0].message.contains("--locked"),
            "nor a build policy it already carries: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
