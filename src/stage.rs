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

/// What tests may substitute for `cargo`: an absolute path to a fake,
/// read by `command` under cfg(test). A plain synchronized value instead
/// of mutating `$PATH` — the environment is process-global and other
/// test threads read it concurrently, which is exactly the unsafety
/// `std::env::set_var` was made unsafe to spotlight.
#[cfg(all(test, feature = "tui"))]
static CARGO_PROGRAM: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Serializes tests that install a fake cargo, so one test's fake never
/// answers another test's spawn.
#[cfg(all(test, feature = "tui"))]
static FAKE_CARGO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII around the fake: holds the serialization lock, installs the
/// override, and clears it on drop, panics included. The guarantee is
/// exactly as strong as the mutex's reach — tests that spawn cargo
/// without taking this guard would still see an active override; today
/// no such test exists, and this comment is where that assumption is
/// written down.
#[cfg(all(test, feature = "tui"))]
pub(crate) struct FakeCargo {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, feature = "tui"))]
impl FakeCargo {
    pub(crate) fn install(script: &Path) -> Self {
        let serial = FAKE_CARGO_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *CARGO_PROGRAM.write().unwrap() = Some(script.to_path_buf());
        Self { _serial: serial }
    }
}

#[cfg(all(test, feature = "tui"))]
impl Drop for FakeCargo {
    fn drop(&mut self) {
        *CARGO_PROGRAM.write().unwrap() = None;
    }
}

fn cargo_program() -> std::ffi::OsString {
    #[cfg(all(test, feature = "tui"))]
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
    // Cargo otherwise tells the user to add the temporary stage/bin to PATH.
    // Append it for the child process so Cargo suppresses that misleading
    // warning without changing command resolution.
    if let Some(path) = std::env::var_os("PATH") {
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        dirs.push(stage.join("bin"));
        if let Ok(joined) = std::env::join_paths(dirs) {
            cmd.env("PATH", joined);
        }
    }
    cmd
}

/// Build `name` from crates.io into the stage root — at exactly
/// `version` if one is given, else the newest cargo picks. The exact
/// form is spelled out (`--version =1.2.3`) rather than relying on
/// cargo treating a bare version as exact: the intent should be in the
/// command line, not in a default.
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

/// Wait up to one tick for the kernel side of the pipe to become
/// readable (or hung up). `Ok(true)` means "read now"; `Ok(false)` is a
/// quiet tick for the read loop's cancel check — including EINTR, which
/// interrupted the wait without producing data: nothing read, nothing
/// lost, and the tick's bounded latency is precisely the property the
/// poll exists for, so a signal must not become a license to block. A
/// real poll error is an error: it goes down the same teardown as a
/// read error, not into a blocking read that would wedge on the very
/// pipe poll just failed to ask about.
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

/// `build` for a frontend that owns the screen: cargo's stderr is piped
/// (which makes cargo drop its own progress bar) and forwarded line by
/// line to `on_line`; nothing reaches the terminal. Plain text is
/// enforced, not assumed: cargo's own coloring is disabled outright and
/// every line is control-character sanitized, because a build script or
/// linker answers to neither cargo nor `CARGO_TERM_COLOR`. stdout is discarded — `cargo install` speaks on
/// stderr, and a stray stdout write must not corrupt an alternate
/// screen.
///
/// On failure the full captured output is written to
/// `<log_dir>/build-<name>-<pid>-<nanos>.log` and the error carries the
/// interesting tail — from the first compiler error onward when there is
/// one, the last lines otherwise — plus the log path, so "failed" is
/// never blind even when the frontend showed only a gauge.
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
    // A pipe usually makes cargo drop colors on its own, but `term.color
    // = "always"` or an inherited CARGO_TERM_COLOR=always would still
    // paint ANSI into the capture — and the parser matches on plain
    // prefixes, the failure panel shows the lines verbatim. Captured
    // means captured; the terminal build stays untouched.
    cmd.env("CARGO_TERM_COLOR", "never");
    // Its own process group, so a cancel can address cargo *and* every
    // rustc and build script it is running with one negative-pid kill.
    // Signalling cargo alone would orphan running compilations, which
    // keep writing into the stage the caller is about to discard. The
    // terminal build stays in the session's foreground group on
    // purpose: there, Ctrl-C reaching everything is the terminal's job.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn cargo")?;
    // The child is its own group leader, so its pid is the group id.
    // Announced before the first read: a cancel that arrives while
    // cargo is spawning must find something to signal, not a
    // forever-empty slot — and `spawned` also delivers a cancel that
    // was accepted before there was anything to signal. Kept locally
    // too, for the sweep below.
    let pgid = i32::try_from(child.id()).ok();
    if let Some(pgid) = pgid {
        control.spawned(pgid);
    }
    let stderr = child
        .stderr
        .take()
        .context("cargo spawned without a stderr pipe")?;
    // read_until + lossy conversion instead of `.lines()`: a build
    // script or linker can emit bytes that are not UTF-8, and a reader
    // that errors out here would return before `child.wait()` — leaving
    // the child running and unreaped, since Child is not killed on drop.
    // Whatever happens on the pipe, the process is always collected.
    let mut reader = std::io::BufReader::new(stderr);
    let mut lines: Vec<String> = Vec::new();
    let mut read_error: Option<std::io::Error> = None;
    let mut buf: Vec<u8> = Vec::new();
    // EOF is the loop's exit — and EOF is withheld for as long as *any*
    // group member keeps the inherited stderr open. A TERM-ignoring
    // child would otherwise wedge a cancellation in a perfect circle:
    // the sweep waits for the reap, the reap waits for EOF, EOF waits
    // for the stray, the stray waits for the sweep. So the blocking
    // read is fronted by a bounded poll, and on quiet ticks the loop
    // checks for exactly that circle: a cancel in flight with the
    // leader already gone means the survivors' supervisor is dead, no
    // further grace is owed, and the remainder of the group is
    // SIGKILLed here — the strays die, EOF arrives, the loop ends.
    // `try_wait` reaps the leader when it answers; `Child` caches the
    // status, so the `wait()` below still returns it.
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
        // The poll asks the kernel — but `read_until` serves from the
        // BufReader first, and one kernel read can park several lines in
        // that buffer. Polling an already-drained pipe while buffered
        // lines wait would hold them hostage to cargo's next write; the
        // buffer is consulted first, and only an empty one earns a tick.
        if reader.buffer().is_empty() {
            match poll_readable(std::os::fd::AsRawFd::as_raw_fd(reader.get_ref())) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    read_error = Some(e);
                    break;
                }
            }
        }
        buf.clear();
        match std::io::BufRead::read_until(&mut reader, b'\n', &mut buf) {
            Ok(0) => break,
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
            Err(e) => {
                read_error = Some(e);
                break;
            }
        }
    }
    if read_error.is_some() {
        // The reader abandons the pipe with cargo possibly still
        // writing; a full pipe would park cargo on write while we park
        // on wait — a quiet mutual stall. Kill first, then reap — and
        // kill the whole group: the build runs in its own process group
        // precisely so rustc and build scripts cannot outlive cargo,
        // and a leader-only kill here would abandon them to keep
        // writing into a stage about to be discarded. The build is
        // already lost to the read failure either way.
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
    // The leader's death does not end the group: a child that ignores
    // SIGTERM — a build script, say: precisely the population the group
    // exists to cover — survives it, and the group id stays alive with
    // any surviving member. On a cancellation those survivors are
    // strays: their supervisor is gone, so no further grace is owed,
    // and the stage they may still be writing to is about to be removed
    // — the remainder of the group is therefore SIGKILLed as cleanup,
    // not escalation, before the address is withdrawn. SIGKILL cannot
    // be ignored; this sweep is what makes "a cancel leaves no orphans
    // writing into the stage" true rather than merely usual. Usually it
    // already ran from the read loop (a stray holding stderr is exactly
    // how EOF gets withheld — see there); this one covers the leader
    // dying after the last line without ever wedging the pipe, and a
    // repeat is a no-op.
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
    // The announcement is withdrawn once this side is done signalling:
    // after the leader's reap and, on a cancellation, after the sweep —
    // the post-wait I/O (failure log, stage verification) stays outside
    // the window. Withdrawn on a failed wait too: the child's state is
    // then unknown, and "never signal" is the only safe direction to be
    // wrong in.
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
        // Ended by the cancel, not by cargo: no failure log — an error
        // that says the person's own decision was carried out is not a
        // diagnosis — and the stage is removed rather than kept, since
        // the only thing it is evidence of is that decision. Both
        // conditions, deliberately: the phase alone would lose the race
        // where cargo dies of its own causes an instant before a late
        // cancel is accepted (a normal exit code is cargo's own verdict
        // and must surface as the failure it is, phase notwithstanding);
        // exit-by-signal alone would misfile an external kill — an OOM,
        // say — as a cancellation nobody requested.
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
    // The failure contract — diagnosis plus the full log — holds past the
    // exit code: cargo saying 0 and the stage failing verification (a
    // missing or forged .crates2.json, a version mismatch) is a failure
    // of this build like any other, and its log matters just as much.
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

/// One shape for every captured-build failure: headline, then the log
/// path — right under it, because a shallow panel truncates from the
/// bottom and the pointer to everything else must survive — then the
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
/// The full captured output, written to a fresh, private file:
/// `create_new` turns the PID+nanos naming from "collision absurdly
/// unlikely" into "overwrite impossible" — an existing file is an error,
/// never silently replaced evidence — and 0600 keeps build.rs output,
/// which can quote the environment, out of other users' reach. Exactly
/// 0600, not merely "no wider": open-time mode is an upper bound under
/// umask (0777 would leave the log unreadable to its own owner), so a
/// chmod on the descriptor restores the owner's rw — safe against the
/// window, since the file is born at most tighter, never looser.
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
    // What the stage holds is the truth about what was built; check it
    // against what was asked rather than assume cargo honoured `=`.
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

    // Key format: `name version (source)`. The stage only ever holds one
    // version per crate (cargo replaces on reinstall), but be defensive and
    // take the semver max if we ever see more.
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
            // Stage bookkeeping is also disk input steering placement; hold
            // it to the same standard as the manifest: valid filenames, no
            // duplicates — caught here, before anything touches the prefix.
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

        // Injected, not resolved: mutating $PATH would be process-global
        // unsafety; the guard hands the fake to `command` directly and
        // clears it on drop, panics included.
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
        // The screw is torqued; mark it with paint: 0600 is a guarantee
        // of write_build_log, not a happy accident of the umask.
        let mode = fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the build log is private to the user");
        let full = fs::read_to_string(&log).unwrap();
        assert!(
            full.contains("Compiling one") && full.contains("note: expected u8"),
            "the log holds everything the tail dropped"
        );

        // The same fake succeeding: lines still stream, the stage is
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
