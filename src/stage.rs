//! Stage builds.
//!
//! `cargo install --root <stage>` runs as the invoking user: registry cache,
//! build scripts and proc macros never execute as root. The stage's
//! `.crates2.json` is then the source of truth for what was actually built —
//! version and binary names — regardless of what the index promised earlier.

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
#[cfg(feature = "tui")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(feature = "tui")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Debug)]
pub struct Built {
    pub version: Version,
    pub bins: Vec<String>,
    pub bin_paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Crates2 {
    installs: BTreeMap<String, InstallInfo>,
}

#[derive(Deserialize)]
struct InstallInfo {
    bins: Vec<String>,
}

/// What tests may substitute for `cargo`: a synchronized value, not a
/// `$PATH` mutation — the environment is process-global, exactly the
/// unsafety `set_var` spotlights. Plain cfg(test): the harness must
/// exist with the tui feature off.
#[cfg(test)]
static CARGO_PROGRAM: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Serializes tests that install a fake cargo, so one test's fake never
/// answers another test's spawn.
#[cfg(test)]
static FAKE_CARGO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII around the fake: lock, install, clear on drop (panics
/// included). Exactly as strong as the mutex's reach — a test spawning
/// cargo without the guard would see an active override; none exists,
/// and this is where that assumption is written down.
#[cfg(test)]
pub(crate) struct FakeCargo {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl FakeCargo {
    pub(crate) fn install(script: &Path) -> Self {
        let serial = FAKE_CARGO_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *CARGO_PROGRAM.write().unwrap() = Some(script.to_path_buf());
        Self { _serial: serial }
    }
}

#[cfg(test)]
impl Drop for FakeCargo {
    fn drop(&mut self) {
        *CARGO_PROGRAM.write().unwrap() = None;
    }
}

fn cargo_program() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(p) = CARGO_PROGRAM.read().unwrap().clone() {
        return p.into_os_string();
    }
    std::ffi::OsString::from("cargo")
}

fn command(name: &str, version: Option<&Version>, locked: bool, stage: &Path) -> Command {
    let mut cmd = Command::new(cargo_program());
    cmd.arg("install").arg(name).arg("--root").arg(stage);
    if let Some(version) = version {
        cmd.arg("--version").arg(format!("={version}"));
    }
    if locked {
        cmd.arg("--locked");
    }
    // Append stage/bin to the child's PATH so cargo suppresses its
    // misleading add-to-PATH warning; resolution is unchanged.
    if let Some(path) = std::env::var_os("PATH") {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        dirs.push(stage.join("bin"));
        if let Ok(joined) = std::env::join_paths(dirs) {
            cmd.env("PATH", joined);
        }
    }
    cmd
}

/// Build `name` into the stage — exactly `version` when given,
/// otherwise the newest version cargo picks. The exact form is spelled
/// (`--version =1.2.3`): the intent belongs in the command line, not a
/// default.
pub fn build(name: &str, version: Option<&Version>, locked: bool, stage: &Path) -> Result<Built> {
    fs::create_dir_all(stage).with_context(|| format!("creating {}", stage.display()))?;
    // Compiler output goes straight to the terminal; the user should see the
    // build exactly as cargo presents it.
    let status = command(name, version, locked, stage)
        .status()
        .context("failed to spawn cargo")?;
    if !status.success() {
        bail!("cargo install {name} failed with {status}");
    }
    verified_info(name, version, stage)
}

/// Wait up to one tick for the pipe: `Ok(true)` read now, `Ok(false)`
/// a quiet tick for the cancel check — EINTR included: nothing read,
/// nothing lost, and a signal must not become a license to block. A
/// real poll error takes the teardown path, never a blocking read.
#[cfg(feature = "tui")]
fn poll_readable(fd: std::os::fd::RawFd) -> std::io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) reads/writes the one pollfd it is given; the
    // struct lives on this stack frame for the whole call.
    let n = unsafe { libc::poll(&raw mut pfd, 1, 100) };
    match n {
        0 => Ok(false),
        1.. => Ok(true),
        _ => {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// The captured read loop, EOF to EOF — and EOF is withheld while
/// *any* group member holds the inherited stderr. A TERM-ignoring
/// child would wedge a cancellation in a circle (sweep waits for reap,
/// reap for EOF, EOF for the stray), so the blocking read is fronted
/// by a bounded poll, and a quiet tick with a cancel in flight and the
/// leader gone sweeps the survivors with SIGKILL. `try_wait` reaps the
/// leader; `Child` caches the status for the caller's `wait()`.
#[cfg(feature = "tui")]
fn drain_stderr(
    reader: &mut std::io::BufReader<std::process::ChildStderr>,
    child: &mut std::process::Child,
    control: &crate::BuildControl,
    pgid: Option<i32>,
    on_line: &mut dyn FnMut(&str),
    lines: &mut Vec<String>,
) -> Option<std::io::Error> {
    let mut buf: Vec<u8> = Vec::new();
    let mut swept = false;
    loop {
        if control.cancelled() && !swept && matches!(child.try_wait(), Ok(Some(_))) {
            swept = true;
            if let Some(pgid) = pgid {
                // SAFETY: kill(2) with a negative pid signals the
                // process group; ESRCH — already empty — is a no-op.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        // `read_until` serves from the BufReader first: polling a drained
        // pipe while buffered lines wait would hold them hostage to cargo's
        // next write — only an empty buffer earns a tick.
        if reader.buffer().is_empty() {
            match poll_readable(std::os::fd::AsRawFd::as_raw_fd(reader.get_ref())) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => return Some(e),
            }
        }
        buf.clear();
        match std::io::BufRead::read_until(reader, b'\n', &mut buf) {
            Ok(0) => return None,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                // Build scripts and linkers answer to neither cargo nor
                // CARGO_TERM_COLOR; sanitize once, for screen and log.
                let line = crate::text::sanitize(&String::from_utf8_lossy(&buf));
                on_line(&line);
                lines.push(line);
            }
            // EINTR: nothing read, nothing lost; simply try again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Some(e),
        }
    }
}

/// `build` for a screen-owning frontend: stderr piped and forwarded
/// line by line; plain text enforced, not assumed (cargo's coloring
/// off, every line sanitized — build scripts answer to neither cargo
/// nor CARGO_TERM_COLOR). stdout discarded: `cargo install` speaks on
/// stderr, and a stray write must not corrupt the screen.
///
/// On failure the full output goes to a log and the error carries the
/// interesting tail plus the path — "failed" is never blind.
#[cfg(feature = "tui")]
pub fn build_captured(
    name: &str,
    version: Option<&Version>,
    locked: bool,
    stage: &Path,
    log_dir: &Path,
    on_line: &mut dyn FnMut(&str),
    control: &crate::BuildControl,
) -> Result<Built> {
    fs::create_dir_all(stage).with_context(|| format!("creating {}", stage.display()))?;
    let mut cmd = command(name, version, locked, stage);
    // A pipe usually drops colors, but term.color=always would still
    // paint ANSI into the capture — and the parser matches plain
    // prefixes. The terminal build stays untouched.
    cmd.env("CARGO_TERM_COLOR", "never");
    // Its own process group, so one negative-pid kill reaches cargo and
    // every rustc it runs; signalling cargo alone would orphan
    // compilations still writing into the stage. The terminal build stays
    // in the session's foreground group: there Ctrl-C is the terminal's
    // job.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn cargo")?;
    // The leader's pid is the group id. Announced before the first read:
    // a cancel arriving mid-spawn must find something to signal, and
    // `spawned` also delivers one accepted earlier.
    let pgid = i32::try_from(child.id()).ok();
    if let Some(pgid) = pgid {
        control.spawned(pgid);
    }
    let stderr = child
        .stderr
        .take()
        .context("cargo spawned without a stderr pipe")?;
    // read_until + lossy conversion, not `.lines()`: non-UTF-8 bytes
    // would error the reader out before `child.wait()`, leaving the child
    // unreaped (Child is not killed on drop).
    let mut reader = std::io::BufReader::new(stderr);
    let mut lines: Vec<String> = Vec::new();
    let read_error = drain_stderr(&mut reader, &mut child, control, pgid, on_line, &mut lines);
    if read_error.is_some() {
        // The reader abandons the pipe with cargo possibly still writing: a
        // full pipe would park cargo on write while we park on wait. Kill the
        // whole group first, then reap — a leader-only kill would abandon
        // rustc to keep writing into a stage about to be discarded.
        if let Ok(pgid) = i32::try_from(child.id()) {
            // SAFETY: kill(2) with a negative pid signals the process
            // group; no memory is touched and an error (ESRCH: already
            // gone) is an acceptable no-op.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    let status = child.wait();
    // The leader's death does not end the group: a TERM-ignoring child
    // survives, and the id stays alive with any member. On a cancellation
    // the survivors' supervisor is gone — SIGKILL the remainder as
    // cleanup before the address is withdrawn; this sweep is what makes
    // "a cancel leaves no orphans writing into the stage" true. Usually
    // the read loop already ran it; this covers a leader dying after the
    // last line, and a repeat is a no-op.
    if control.cancelled()
        && let Some(pgid) = pgid
    {
        // SAFETY: kill(2) with a negative pid signals the process
        // group; no memory is touched, and ESRCH — the group already
        // empty — is the happy case.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    // Withdrawn once this side is done signalling — after the reap and
    // the sweep, before the post-wait I/O. On a failed wait too: the
    // state is unknown, and "never signal" is the safe direction.
    control.reaped();
    let status = status.context("waiting for cargo")?;
    if let Some(e) = read_error {
        return Err(failure_with_log(
            log_dir,
            name,
            &lines,
            format!("reading cargo output failed: {e}"),
            &tail_from(&lines, lines.len().saturating_sub(TAIL_LINES)),
        ));
    }
    if !status.success() {
        // Ended by the cancel, not by cargo: no failure log (the person's own
        // decision is not a diagnosis), stage removed. Both conditions on
        // purpose: phase alone loses the race where cargo dies just before a
        // late cancel; signal alone would misfile an external OOM kill as a
        // cancellation.
        use std::os::unix::process::ExitStatusExt;
        if control.cancelled() && status.signal().is_some() {
            let _ = fs::remove_dir_all(stage);
            return Err(anyhow::Error::new(crate::BuildCancelled));
        }
        // Tail from the first compiler error when there is one — the
        // lines before it are successful units, noise here.
        let start = lines
            .iter()
            .position(|l| {
                matches!(
                    crate::progress::parse_line(l),
                    crate::progress::BuildEvent::Error
                )
            })
            .unwrap_or(lines.len().saturating_sub(TAIL_LINES));
        return Err(failure_with_log(
            log_dir,
            name,
            &lines,
            format!("cargo install {name} failed with {status}"),
            &tail_from(&lines, start),
        ));
    }
    // The failure contract holds past the exit code: cargo saying 0 with
    // a stage failing verification is a failure like any other, log
    // included.
    verified_info(name, version, stage).map_err(|e| {
        failure_with_log(
            log_dir,
            name,
            &lines,
            format!("cargo exited successfully, but: {e:#}"),
            &tail_from(&lines, lines.len().saturating_sub(TAIL_LINES)),
        )
    })
}

#[cfg(feature = "tui")]
fn tail_from(lines: &[String], start: usize) -> Vec<&str> {
    lines[start..]
        .iter()
        .take(TAIL_LINES)
        .map(String::as_str)
        .collect()
}

/// One shape for every captured failure: headline, log path right
/// under it (a shallow panel truncates from the bottom), then the
/// tail.
#[cfg(feature = "tui")]
fn failure_with_log(
    log_dir: &Path,
    name: &str,
    lines: &[String],
    headline: String,
    tail: &[&str],
) -> anyhow::Error {
    let log = write_build_log(log_dir, name, lines);
    let mut msg = headline;
    match log {
        Ok(path) => {
            msg.push_str("\nfull log: ");
            msg.push_str(&path.display().to_string());
        }
        Err(e) => {
            use std::fmt::Write as _;
            let _ = write!(msg, "\n(could not write the full log: {e:#})");
        }
    }
    for l in tail {
        msg.push_str("\n  ");
        msg.push_str(l);
    }
    anyhow::anyhow!(msg)
}

#[cfg(feature = "tui")]
const TAIL_LINES: usize = 12;

#[cfg(feature = "tui")]
/// The full output to a fresh, private file: `create_new` makes
/// overwrite impossible (an existing file is an error, never replaced
/// evidence); 0600 keeps build.rs output — which can quote the
/// environment — from other users. chmod on the descriptor restores
/// owner rw under a hostile umask; the file is born at most tighter.
fn write_build_log(log_dir: &Path, name: &str, lines: &[String]) -> Result<PathBuf> {
    fs::create_dir_all(log_dir)
        .with_context(|| format!("creating log directory {}", log_dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = log_dir.join(format!("build-{name}-{}-{stamp}.log", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating build log {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on build log {}", path.display()))?;
    let mut body = lines.join("\n");
    body.push('\n');
    std::io::Write::write_all(&mut file, body.as_bytes())
        .with_context(|| format!("writing build log {}", path.display()))?;
    Ok(path)
}

/// Read the stage and verify it holds what was asked for — shared tail
/// of both build variants.
fn verified_info(name: &str, version: Option<&Version>, stage: &Path) -> Result<Built> {
    let built = staged_info(name, stage)?;
    // The stage is the truth about what was built; check it against what
    // was asked rather than assume cargo honoured `=`.
    if let Some(version) = version
        && built.version != *version
    {
        bail!(
            "asked for {name} {version} but the stage holds {}",
            built.version
        );
    }
    Ok(built)
}

/// Read what the stage actually contains for `name` from `.crates2.json`.
fn staged_info(name: &str, stage: &Path) -> Result<Built> {
    let path = stage.join(".crates2.json");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: Crates2 =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    // Key format: `name version (source)`. One version per crate in
    // practice; take the semver max defensively.
    let mut best: Option<Built> = None;
    for (key, info) in &parsed.installs {
        let mut parts = key.split_whitespace();
        let (Some(key_name), Some(key_version), Some(key_source)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if key_name != name || !key_source.contains(CRATES_IO_SOURCE) {
            continue;
        }
        let version = Version::parse(key_version)
            .with_context(|| format!("unparsable staged version `{key_version}`"))?;
        let replace = match &best {
            Some(b) => version > b.version,
            None => true,
        };
        if replace {
            // Stage bookkeeping is disk input steering placement; hold it to the
            // manifest's standard — caught here, before the prefix is touched.
            crate::validate::validate_bin_list(&info.bins)
                .with_context(|| format!("stage bookkeeping for `{name}`"))?;
            let bin_dir = stage.join("bin");
            best = Some(Built {
                bin_paths: info.bins.iter().map(|b| bin_dir.join(b)).collect(),
                bins: info.bins.clone(),
                version,
            });
        }
    }
    best.with_context(|| format!("`{name}` missing from stage bookkeeping after build"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_info_parses_crates2() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-stage");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".crates2.json"),
            r#"{"installs":{
                "hexyl 0.14.0 (registry+https://github.com/rust-lang/crates.io-index)":
                    {"bins":["hexyl"]},
                "other 1.0.0 (git+https://example.com/other#abc)":
                    {"bins":["other"]}
            }}"#,
        )
        .unwrap();

        let built = staged_info("hexyl", &dir).unwrap();
        assert_eq!(built.version, Version::parse("0.14.0").unwrap());
        assert_eq!(built.bins, vec!["hexyl"]);
        assert!(
            staged_info("other", &dir).is_err(),
            "git source must not match"
        );
        assert!(staged_info("absent", &dir).is_err());

        // Forged bookkeeping with duplicate bins must fail here, before
        // anything would touch the prefix.
        fs::write(
            dir.join(".crates2.json"),
            r#"{"installs":{
                "dupes 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":
                    {"bins":["foo","foo"]}
            }}"#,
        )
        .unwrap();
        let err = format!("{:#}", staged_info("dupes", &dir).unwrap_err());
        assert!(err.contains("listed twice"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn captured_build_forwards_lines_and_logs_failures() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-captured");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("bin");
        let stage = root.join("stage");
        let logs = root.join("logs");
        fs::create_dir_all(&fake_bin).unwrap();

        // A fake `cargo` first: fails after emitting a compiler error, so
        // the tail must start at the error and the full log must exist.
        let script = fake_bin.join("cargo");
        fs::write(
            &script,
            "#!/bin/sh\n\
             echo '   Compiling one v1.0.0' >&2\n\
             echo '   Compiling two v2.0.0' >&2\n\
             echo 'error[E0308]: mismatched types' >&2\n\
             echo 'note: expected u8' >&2\n\
             exit 101\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        // Injected, not resolved: the guard hands the fake to `command`
        // directly and clears it on drop.
        let _fake = FakeCargo::install(&script);

        let mut seen: Vec<String> = Vec::new();
        let control = crate::BuildControl::new();
        let err = build_captured(
            "boomcrate",
            None,
            false,
            &stage,
            &logs,
            &mut |l| {
                seen.push(l.to_owned());
            },
            &control,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert_eq!(seen.len(), 4, "every stderr line reaches the frontend");
        assert!(
            msg.contains("error[E0308]") && msg.contains("note: expected u8"),
            "tail starts at the first compiler error: {msg}"
        );
        assert!(
            !msg.contains("Compiling one"),
            "successful units stay out of the tail: {msg}"
        );
        assert!(msg.contains("full log:"), "log path travels in the error");
        let log = fs::read_dir(&logs).unwrap().next().unwrap().unwrap().path();
        // Torqued, marked with paint: 0600 is a guarantee of
        // write_build_log, not a happy accident of the umask.
        let mode = fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the build log is private to the user");
        let full = fs::read_to_string(&log).unwrap();
        assert!(
            full.contains("Compiling one") && full.contains("note: expected u8"),
            "the log holds everything the tail dropped"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The other half of the captured contract: success streams through
    /// the same parser path, and an exit-0 build that staged nothing still
    /// fails with the log written.
    #[cfg(feature = "tui")]
    #[test]
    fn captured_build_streams_success_and_verifies_the_stage() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("cargo-lbin-test-captured-ok");
        let _ = fs::remove_dir_all(&root);
        let fake_bin = root.join("bin");
        let stage = root.join("stage");
        let logs = root.join("logs");
        fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("cargo");
        fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _fake = FakeCargo::install(&script);

        // The fake succeeding: lines still stream, the stage is
        // verified through the same path as the terminal build.
        fs::write(
            &script,
            "#!/bin/sh\n\
                 echo '   Compiling okcrate v0.1.0' >&2\n\
                 echo '    Finished release [optimized]' >&2\n\
                 mkdir -p \"$4\"\n\
                 printf '%s' '{\"installs\":{\"okcrate 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{\"bins\":[\"okcrate\"]}}}' > \"$4/.crates2.json\"\n\
                 exit 0\n",
        )
        .unwrap();
        let mut count = 0usize;
        let built = build_captured(
            "okcrate",
            None,
            false,
            &stage,
            &logs,
            &mut |l| {
                if matches!(
                    crate::progress::parse_line(l),
                    crate::progress::BuildEvent::Compiling { .. }
                ) {
                    count += 1;
                }
            },
            &crate::BuildControl::new(),
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(built.bins, vec!["okcrate"]);

        // A fake that exits 0 without staging anything: the failure
        // contract must hold past the exit code, log included.
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let _ = fs::remove_dir_all(&logs);
        let err = build_captured(
            "ghost",
            None,
            false,
            &stage,
            &logs,
            &mut |_| {},
            &crate::BuildControl::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cargo exited successfully, but:"),
            "verification failure names itself: {msg}"
        );
        assert!(
            msg.contains("full log:"),
            "verification failure still writes and names the log: {msg}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
