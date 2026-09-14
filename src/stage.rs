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

/// The directory under the cache where leased runs will live: a
/// namespace a 0.12 binary does not know and therefore cannot sweep.
/// The old binary's `clean --stages` classifies any non-PID name under
/// `stage/` as debris — a leased run placed there could be removed,
/// live lock and all, by a concurrently installed 0.12. One cache must
/// be shareable by both binaries during an upgrade window, so the
/// formats do not share a directory.
pub const RUN_NAMESPACE: &str = "stage-v2";

/// A directory name in a stage namespace under the cache, as the
/// scanners read it.
///
/// Two layouts coexist. `<pid>` is the pre-lease layout: the PID is
/// the only identity there is, and liveness is the `/proc` heuristic —
/// with both of its known lies (a reused PID resurrects a dead stage,
/// an orphaned cargo outlives the PID that named it). `<pid>-<nonce>`
/// is the lease-aware layout: the nonce makes the name unique across
/// PID reuse, so the PID degrades to what it always should have been —
/// a diagnostic — and ownership is whatever holds the run's `.lease`.
///
/// A name that parses as neither is not a run at all; the scanners
/// keep their existing verdict on such debris.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageRun {
    /// `<pid>` — liveness by `/proc`, the heuristic this type exists
    /// to retire.
    LegacyPid(u32),
    /// `<pid>-<nonce>` — ownership by lease; the PID is diagnostics
    /// only and never consulted for liveness.
    LeasedRun { pid: u32 },
}

/// Nonce width in hex digits: 8 random bytes, enough that a collision
/// under one machine's stage namespaces is not a case worth code.
const NONCE_HEX_LEN: usize = 16;

/// Classify a stage-namespace entry name. Strict on purpose: a legacy
/// name is
/// ASCII digits and nothing else, a leased name is digits, one dash,
/// and exactly [`NONCE_HEX_LEN`] lowercase hex digits — the alphabet
/// [`new_run_dir_name`] writes. Anything looser would promote debris
/// into a run and buy it protection it never earned.
#[must_use]
pub fn parse_run_dir(name: &str) -> Option<StageRun> {
    fn pid(s: &str) -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    if let Some(p) = pid(name) {
        return Some(StageRun::LegacyPid(p));
    }
    let (p, nonce) = name.split_once('-')?;
    let p = pid(p)?;
    if nonce.len() == NONCE_HEX_LEN
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Some(StageRun::LeasedRun { pid: p })
    } else {
        None
    }
}

/// Name of the lease file inside a leased run directory.
const LEASE_FILE: &str = ".lease";

/// The lease's name while it is being prepared: created, locked and
/// stripped of close-on-exec under this name, renamed to [`LEASE_FILE`]
/// only then. The probe does not know this name, so an in-preparation
/// lease reads as absent — unknown, spared — never as released.
const LEASE_PENDING: &str = ".lease.pending";

/// Best-effort demolition of a run this process created but never
/// published. Armed right after `create_dir` succeeds, disarmed only
/// once the lease is renamed into place: any handled error on the way
/// removes the half-made run, because a run without a published lease
/// is Unknown to every scanner — spared forever — and a *handled*
/// error is not the crash the conservative skip was priced for. No
/// child exists yet at any point this can fire, so the removal cannot
/// take a stage from under anyone.
struct UnpublishedRun<'a> {
    run_dir: &'a Path,
    armed: bool,
}

impl Drop for UnpublishedRun<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(self.run_dir);
        }
    }
}

/// Ownership of one leased run, held as `flock(2) LOCK_EX` on the
/// run's `.lease` for as long as anything may still write to the
/// stage.
///
/// The lock belongs to the open file *description*, not to this
/// process: the descriptor is stripped of `FD_CLOEXEC` on acquisition,
/// so every child spawned while the lease is held — cargo, rustc,
/// build scripts — inherits it, and the lease stands until the last
/// inheritor exits. That is the whole design: a cargo orphaned by its
/// cargo-lbin keeps the stage visibly owned, where the PID heuristic
/// would have called it debris. The cost is a descriptor leaked into
/// short-lived helpers too (sudo among them); the error is on the
/// conservative side — a stage can only look alive longer, never
/// deletable sooner.
pub struct Lease {
    // Held for the descriptor it keeps open; its Drop is the release.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the field's work is keeping the descriptor open")
    )]
    file: fs::File,
}

impl Lease {
    /// Create `run_dir` and take its lease.
    ///
    /// `LOCK_NB` even though contention is impossible by construction —
    /// the nonce made the directory ours alone — because *if* the lock
    /// is somehow held, blocking on it would hide a bug behind a hang,
    /// and this error names it instead.
    pub fn acquire(run_dir: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        if let Some(parent) = run_dir.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        // create_dir, not create_dir_all, and create_new below: the
        // nonce promised a fresh name, and the filesystem is where
        // that promise is enforced — an EEXIST here is a bug named,
        // not a state tolerated.
        fs::create_dir(run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
        // From here to the rename, every error path demolishes the
        // half-made run: handled failure cleans up after itself, only
        // a crash leaves the conservative-Unknown residue behind.
        let mut guard = UnpublishedRun {
            run_dir,
            armed: true,
        };
        // Locked before visible. Publishing `.lease` first would open
        // a window — created but not yet locked — where a probe reads
        // "released" off a lease nobody owns, and a clean could take
        // it and remove the run under its creator's feet, stranding
        // the creator's eventual lock on an unlinked inode. So the
        // lease is prepared under a name the probe does not know and
        // renamed into place only once it is already locked: from the
        // first instant `.lease` exists, it is held.
        let pending = run_dir.join(LEASE_PENDING);
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&pending)
            .with_context(|| format!("creating {}", pending.display()))?;
        let fd = file.as_raw_fd();
        // SAFETY: flock(2) on an owned, open descriptor; no memory is
        // passed.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", pending.display()));
        }
        // Rust opens everything O_CLOEXEC; without undoing that here
        // the lease would die with this process and the mechanism
        // would degrade to the PID heuristic, only costlier. Read the
        // flags and clear exactly the one bit — F_SETFD with a bare 0
        // would erase flags this code never claimed to own.
        // SAFETY: fcntl(2) F_GETFD/F_SETFD on an owned, open
        // descriptor; no memory is passed.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("reading descriptor flags of {}", pending.display()));
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("clearing close-on-exec on {}", pending.display()));
        }
        let published = run_dir.join(LEASE_FILE);
        fs::rename(&pending, &published)
            .with_context(|| format!("publishing {}", published.display()))?;
        guard.armed = false;
        Ok(Self { file })
    }
}

/// The run path itself must be a real directory — the same rule the
/// payload walk already enforces one level down, applied at the top:
/// a symlink planted under the namespace with a valid run name must
/// not let a probe read someone else's lease as this run's, the taker
/// lock it, or the remover walk (and delete) whatever the link points
/// at.
fn require_real_run_dir(run_dir: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(run_dir)?;
    if meta.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "run path is not a real directory",
        ))
    }
}

/// Take an existing run's lease exclusively, for removal.
///
/// The opposite discipline from [`Lease::acquire`]: nothing is
/// created — a missing `.lease` here is a run someone else already
/// removed or never finished publishing, and manufacturing a lease to
/// then "own" it would convert absence of evidence into a removal
/// license. No `FD_CLOEXEC` clearing either: this lease is held across
/// a removal, not an exec, and it must die with its holder.
///
/// `Ok(Some)` — the caller owns the run until the returned lease
/// drops, so hold it through the whole removal: that ordering is what
/// closes the check-then-delete race. `Ok(None)` — held this instant;
/// the caller leaves the run alone and a later pass answers. `Err` —
/// the lease exists but would not open or lock; the caller decides
/// what its own contract owes for that.
pub fn take_lease_for_removal(run_dir: &Path) -> std::io::Result<Option<Lease>> {
    use std::os::fd::AsRawFd;
    require_real_run_dir(run_dir)?;
    // Read-write, not read-only: on filesystems that emulate flock via
    // fcntl (NFS among them) an exclusive lock wants a writable
    // descriptor. The creator's own lease is writable already; the
    // taker should not be the one descriptor in the design that only
    // works where flock is native.
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(run_dir.join(LEASE_FILE))?;
    // SAFETY: flock(2) on an owned, open descriptor; no memory is
    // passed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(Lease { file }));
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Remove a leased run whose lease the caller holds — payload first,
/// the lease last.
///
/// `remove_dir_all` makes no ordering promise, so it can unlink
/// `.lease` first and then fail on the payload — leaving a lease-less
/// new-format run, which every scanner reads as unknown and spares
/// forever: a handled cleanup error manufacturing permanently
/// invisible debris. So the proof of ownership goes last. Any failure
/// in the payload walk leaves `.lease` in place; the caller's lock
/// eventually drops, the next probe reads released, and the next pass
/// may try again. What remains is the microscopic window between the
/// lease's unlink and the final `remove_dir` — and a run caught there
/// is already empty. Symlinked entries are unlinked, never followed:
/// a payload symlink must not turn cleanup into a walk of someone
/// else's tree.
pub fn remove_leased_run(run_dir: &Path) -> std::io::Result<()> {
    require_real_run_dir(run_dir)?;
    for entry in fs::read_dir(run_dir)? {
        let entry = entry?;
        if entry.file_name() == LEASE_FILE {
            continue;
        }
        // DirEntry::file_type does not follow symlinks, so a link to a
        // directory takes the remove_file branch — unlinked, not
        // traversed.
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_file(run_dir.join(LEASE_FILE))?;
    fs::remove_dir(run_dir)
}

/// The creator's own cleanup, with the veto the lease's doc promised:
/// relinquish `lease`, then remove the run only if no inheritor
/// survives.
///
/// Dropping the creator's descriptor does not release the lock while
/// any child still holds the inherited one — flock lives on the open
/// file description — so a fresh-descriptor exclusive take is the
/// question "did anything survive me?", asked of the kernel itself.
/// Refused: an inheritor is alive, the run stays, and the inheritor's
/// eventual exit makes it verify's finding and clean's candidate — the
/// ordinary ownerless path. Granted: nothing can write to the stage
/// anymore, and the removal happens holding the lock, the same
/// discipline clean uses — payload first, the lease last, so even a
/// failed removal leaves the run visible. Best-effort throughout:
/// cleanup failure is not operation failure, and a run left behind is
/// exactly what verify exists to name — which the lease-last order is
/// what keeps true.
pub fn release_and_remove_run(lease: Lease, run_dir: &Path) {
    drop(lease);
    if let Ok(Some(_held)) = take_lease_for_removal(run_dir) {
        let _ = remove_leased_run(run_dir);
    }
    // Ok(None) — an inheritor survives; Err — nothing left to judge
    // with. Either way the run stays, and stays somebody's finding.
}

/// What a shared, non-blocking probe of a run's lease learned.
///
/// Three answers, not two, and the third is the one that keeps the
/// mechanism honest: creation is not atomic — `mkdir` publishes the
/// run name before `.lease` exists — so a missing or unopenable lease
/// proves nothing about liveness, and unknown must be spared exactly
/// like held. Only a lease that was there and takeable convicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    /// The lease is held: a writer, or something it spawned, is alive.
    Held,
    /// The lease was takeable: no writer remains.
    Released,
    /// No `.lease`, or one that would not open or lock: the creation
    /// window, or a filesystem with opinions — not evidence.
    Unknown,
}

/// Probe a run's lease with `LOCK_SH | LOCK_NB` and let go at once.
///
/// Shared on purpose: the live writer holds `LOCK_EX`, so any probe
/// against a real owner blocks either way — but two concurrent probes
/// must not see *each other* as owners, and with `LOCK_EX` probes they
/// would, a false classification manufactured by the probe's own
/// implementation. `LOCK_SH` lets every reader through and stops at
/// exactly the thing that matters: a writer. The probe's own lock ends
/// when `file` drops on return.
#[must_use]
pub fn probe_lease(run_dir: &Path) -> LeaseState {
    use std::os::fd::AsRawFd;
    // A run path that is not a real directory is not a run: probing
    // through a top-level symlink would read someone else's lease as
    // this run's, and a released-through-a-link answer could then hand
    // clean a removal license for the link's target. Not evidence.
    if require_real_run_dir(run_dir).is_err() {
        return LeaseState::Unknown;
    }
    let Ok(file) = fs::File::open(run_dir.join(LEASE_FILE)) else {
        return LeaseState::Unknown;
    };
    // SAFETY: flock(2) on an owned, open descriptor; no memory is
    // passed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        LeaseState::Released
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        LeaseState::Held
    } else {
        // A lock that failed for any reason but contention answered a
        // different question: not evidence.
        LeaseState::Unknown
    }
}

/// A fresh `<pid>-<nonce>` name for this run. The nonce comes from
/// `getrandom(2)`: no seed to manage, no clock to collide on, and no
/// file descriptor to leak — two runs in the same nanosecond are a
/// scheduler fact, two equal nonces are not.
pub fn new_run_dir_name() -> std::io::Result<String> {
    use std::fmt::Write as _;
    let mut nonce = [0u8; NONCE_HEX_LEN / 2];
    let mut filled = 0usize;
    while filled < nonce.len() {
        // SAFETY: getrandom(2) writes at most `len` bytes into the
        // buffer starting at `buf`; the range passed lives on this
        // stack frame for the whole call.
        let n = unsafe {
            libc::getrandom(nonce[filled..].as_mut_ptr().cast(), nonce.len() - filled, 0)
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            // EINTR: nothing written, nothing lost; simply try again.
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        filled += usize::try_from(n).unwrap_or(0);
    }
    let mut name = format!("{}-", std::process::id());
    for b in nonce {
        // Infallible on String; the expect documents that, not a risk.
        write!(name, "{b:02x}").expect("writing to a String cannot fail");
    }
    Ok(name)
}

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
/// nor `CARGO_TERM_COLOR`). stdout discarded: `cargo install` speaks on
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
        // decision is not a diagnosis). Both conditions on purpose: phase
        // alone loses the race where cargo dies just before a late cancel;
        // signal alone would misfile an external OOM kill as a cancellation.
        // No cleanup here, deliberately: this function knows the stage, not
        // the run or its lease, and a group escapee may still hold the
        // inherited lease and be writing — removal is the lease holder's
        // call, made by the caller's handoff, never a classification's
        // side effect.
        use std::os::unix::process::ExitStatusExt;
        if control.cancelled() && status.signal().is_some() {
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

    /// The parser is the classification boundary: everything it
    /// accepts earns a liveness protocol, everything it rejects is
    /// debris. Both sides of that line are pinned here.
    #[test]
    fn run_dir_parsing_is_strict_on_both_layouts() {
        assert_eq!(parse_run_dir("1234"), Some(StageRun::LegacyPid(1234)));
        assert_eq!(
            parse_run_dir("1234-0123456789abcdef"),
            Some(StageRun::LeasedRun { pid: 1234 })
        );
        for junk in [
            "",
            "-",
            "12x",
            "+7", // u32::parse would take it; the scanner must not
            " 7",
            "1234-",
            "-0123456789abcdef",
            "1234-0123456789abcde",   // one hex digit short
            "1234-0123456789abcdef0", // one hex digit long
            "1234-0123456789ABCDEF",  // not the alphabet we write
            "1234-0123456789abcdeg",
            "12x4-0123456789abcdef",
            "1234-0123456789abcdef-0", // trailing garbage
        ] {
            assert_eq!(parse_run_dir(junk), None, "accepted junk: {junk:?}");
        }
    }

    /// A handled acquire error demolishes the half-made run instead of
    /// leaving a forever-Unknown directory no scanner may touch. The
    /// forced failure: a run path short enough for `create_dir` but
    /// whose `.lease.pending` sibling exceeds `PATH_MAX`, so the very
    /// next step fails after the guard is armed. Only a crash — not a
    /// handled error — is allowed to leave conservative-Unknown
    /// residue.
    #[test]
    fn a_handled_acquire_error_leaves_no_unknown_run_behind() {
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-guard");
        let _ = fs::remove_dir_all(&root);
        // Build a path a few bytes under PATH_MAX (4096 on Linux):
        // components of 200 'x's keep every name under NAME_MAX.
        let mut run = root.clone();
        while run.as_os_str().len() + 201 < 4090 {
            run = run.join("x".repeat(200));
        }
        let pad = 4090_usize.saturating_sub(run.as_os_str().len() + 1);
        run = run.join("x".repeat(pad.clamp(1, 200)));

        let Err(err) = Lease::acquire(&run) else {
            panic!("a path past PATH_MAX must not acquire")
        };
        assert!(
            format!("{err:#}").contains(".lease.pending"),
            "the failure is the pending file's, past create_dir: {err:#}"
        );
        assert!(
            !run.exists(),
            "a handled error demolishes the half-made run"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The lease-last removal, exercised over a populated run: nested
    /// payload, a plain file, and a symlink pointing outside — the
    /// link must be unlinked, never followed, and its target left
    /// untouched. The ordering itself (payload before `.lease`) is
    /// enforced by construction in `remove_leased_run`; what is
    /// observable is that the run and its lease are wholly gone and
    /// the outside world is not.
    #[test]
    fn remove_leased_run_clears_payload_and_spares_symlink_targets() {
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-remove");
        let _ = fs::remove_dir_all(&root);
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("keep")).unwrap();
        let run = root.join("42-00000000000000ff");
        let lease = Lease::acquire(&run).unwrap();
        fs::create_dir_all(run.join("somecrate").join("bin")).unwrap();
        fs::write(run.join("somecrate").join("bin").join("tool"), b"x").unwrap();
        fs::write(run.join("stray-file"), b"y").unwrap();
        std::os::unix::fs::symlink(&outside, run.join("link-out")).unwrap();

        drop(lease);
        let held = take_lease_for_removal(&run).unwrap();
        assert!(held.is_some(), "no writer left: the taker owns the run");
        remove_leased_run(&run).unwrap();
        assert!(!run.exists(), "the run is wholly gone, lease included");
        assert!(
            outside.join("keep").exists(),
            "a payload symlink is unlinked, never followed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The same rule one level up: a symlink planted AS the run path,
    /// wearing a valid run name and pointing at a directory that even
    /// contains an unlocked `.lease` — the strongest possible bait.
    /// Neither the taker nor the remover may follow it; the target
    /// stays untouched.
    #[test]
    fn a_symlinked_run_path_is_never_followed() {
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-toplink");
        let _ = fs::remove_dir_all(&root);
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("keep")).unwrap();
        fs::write(outside.join(LEASE_FILE), b"").unwrap();
        let link = root.join("55-00000000000000ab");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let take = take_lease_for_removal(&link);
        assert!(
            take.is_err(),
            "a symlinked run path must refuse the taker outright"
        );
        let removal = remove_leased_run(&link);
        assert!(
            removal.is_err(),
            "a symlinked run path must refuse the remover outright"
        );
        assert!(
            outside.join("keep").exists() && outside.join(LEASE_FILE).exists(),
            "zero traversal: the link's target is untouched"
        );
        assert!(
            link.exists() || fs::symlink_metadata(&link).is_ok(),
            "the link itself is left where it was found"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The creator's cleanup asks before it deletes. A child spawned
    /// while the lease is held inherits the descriptor; the creator's
    /// own `release_and_remove_run` must then be refused — the child's
    /// inherited lock survives the creator's drop — and the run stays
    /// until the child exits, at which point it is the ordinary
    /// ownerless candidate any taker may claim.
    #[test]
    fn creator_cleanup_defers_to_a_surviving_inheritor() {
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-veto");
        let _ = fs::remove_dir_all(&root);
        let run = root.join("777-00000000000000dd");
        let stop = root.join("stop");
        let lease = Lease::acquire(&run).unwrap();
        fs::create_dir_all(run.join("somecrate")).unwrap();

        // The inheritor: descriptor inherited at spawn, held until the
        // stop file appears — with a hard iteration cap so a panicking
        // test cannot strand it on the runner.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "n=0; while [ ! -e \"{stop}\" ] && [ \"$n\" -lt 400 ]; do n=$((n+1)); sleep 0.05; done",
                stop = stop.display()
            ))
            .spawn()
            .unwrap();

        release_and_remove_run(lease, &run);
        assert!(
            run.exists(),
            "a surviving inheritor vetoes the creator's own cleanup"
        );
        assert!(
            take_lease_for_removal(&run).unwrap().is_none(),
            "the veto is the inheritor's lock, nothing softer: a re-ask is refused too"
        );

        fs::write(&stop, b"").unwrap();
        child.wait().unwrap();
        let taken = take_lease_for_removal(&run).unwrap();
        assert!(
            taken.is_some(),
            "the inheritor's exit is the release; the ordinary ownerless path takes over"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The lease's two kernel-side promises, asked of the kernel
    /// directly: a second file description cannot take the lock while
    /// the `Lease` lives, and can the moment it drops. The probe opens
    /// the file anew on purpose — flock is per open file description,
    /// and a re-used descriptor would test nothing.
    #[test]
    fn a_lease_is_held_exactly_as_long_as_its_holder_lives() {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-lifetime");
        let _ = fs::remove_dir_all(&root);
        let run = root.join("12345-0123456789abcdef");
        let lease = Lease::acquire(&run).unwrap();
        assert!(
            !run.join(LEASE_PENDING).exists(),
            "acquire publishes by rename: the pending name must not outlive it"
        );
        assert!(
            run.join(LEASE_FILE).exists(),
            "from the first instant .lease exists, it is held"
        );

        let probe = fs::File::open(run.join(LEASE_FILE)).unwrap();
        // SAFETY: flock(2) on an owned, open descriptor.
        let contended = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(contended, -1, "a held lease must refuse a second owner");
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock,
            "the refusal is contention, not some other failure"
        );

        drop(lease);
        // SAFETY: as above.
        let taken = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(taken, 0, "a dropped lease must be takeable at once");
        let _ = fs::remove_dir_all(&root);
    }

    /// The descriptor must survive exec, or the design collapses to
    /// the PID heuristic with extra steps: `FD_CLOEXEC` is asserted
    /// clear on the held lease itself.
    #[test]
    fn a_lease_descriptor_is_inheritable_across_exec() {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-cloexec");
        let _ = fs::remove_dir_all(&root);
        let run = root.join("12345-fedcba9876543210");
        let lease = Lease::acquire(&run).unwrap();
        // SAFETY: fcntl(2) F_GETFD on an owned, open descriptor.
        let flags = unsafe { libc::fcntl(lease.file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            flags & libc::FD_CLOEXEC,
            0,
            "a close-on-exec lease dies with this process — the one lifetime it must outlive"
        );
        drop(lease);
        let _ = fs::remove_dir_all(&root);
    }

    /// The probe's three answers, plus the property the shared mode
    /// was chosen for: two probes at once both pass — a fellow reader
    /// is not a writer — while an exclusive taker is refused as long
    /// as any reader is inside.
    #[test]
    fn probing_answers_unknown_held_released_and_readers_coexist() {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join("cargo-lbin-test-lease-probe");
        let _ = fs::remove_dir_all(&root);

        // Published name, no lease yet: the creation window. A
        // separate directory — acquire's create_dir enforces the
        // fresh-name promise and would rightly refuse a pre-made one.
        let window = root.join("12345-00000000000000ee");
        fs::create_dir_all(&window).unwrap();
        assert_eq!(probe_lease(&window), LeaseState::Unknown);

        let run = root.join("12345-00000000000000cc");
        let lease = Lease::acquire(&run).unwrap();
        assert_eq!(probe_lease(&run), LeaseState::Held);

        // Two readers at once: probe A holds LOCK_SH while probe B
        // runs; B must still see the writer, not the reader.
        drop(lease);
        let reader = fs::File::open(run.join(LEASE_FILE)).unwrap();
        // SAFETY: flock(2) on an owned, open descriptor.
        assert_eq!(
            unsafe { libc::flock(reader.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0,
            "no writer left: the reader's shared lock goes through"
        );
        assert_eq!(
            probe_lease(&run),
            LeaseState::Released,
            "a fellow reader must never register as an owner"
        );
        // And the exclusive side of the same coin: while a reader is
        // inside, a would-be exclusive taker is refused.
        let taker = fs::File::open(run.join(LEASE_FILE)).unwrap();
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::flock(taker.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "a reader inside refuses the exclusive taker"
        );
        drop(reader);
        assert_eq!(probe_lease(&run), LeaseState::Released);
        let _ = fs::remove_dir_all(&root);
    }

    /// The generator and the parser agree on one alphabet, and two
    /// calls never agree on one name — the whole point of the nonce.
    #[test]
    fn run_dir_names_round_trip_and_differ() {
        let a = new_run_dir_name().unwrap();
        let b = new_run_dir_name().unwrap();
        assert_ne!(a, b, "two runs, one name: the nonce failed its job");
        for name in [&a, &b] {
            assert_eq!(
                parse_run_dir(name),
                Some(StageRun::LeasedRun {
                    pid: std::process::id()
                }),
                "the generator wrote a name the parser rejects: {name}"
            );
        }
    }
}
