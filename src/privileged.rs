//! Privilege handling.
//!
//! The rule: build as the user, escalate only for file placement. sudo is
//! prepended only when the actual destination directory is not writable, so
//! tests and user-owned prefixes (`--prefix ~/.local`) never touch sudo at
//! all — and a prefix with mixed ownership (writable `bin/`, root-owned
//! `share/`) gets the right answer per destination, not per prefix.
//!
//! Two hardening rules apply throughout, because crate build scripts run as
//! the user before any of this executes and must not be able to steer it:
//!
//! 1. Nothing on the privileged path is resolved through `$PATH`. A build
//!    script can drop a fake `sudo` into `~/.local/bin` that prints a
//!    password prompt; `/usr/bin/sudo` it cannot replace. All tools are
//!    invoked by absolute path, and restorecon is looked up only in trusted
//!    directories.
//!
//! 2. Staged binaries are never handed to root by pathname. The stage is
//!    user-controlled, so between our checks and `sudo install` a leftover
//!    process could swap `stage/bin/foo` for a symlink to `/etc/shadow` —
//!    and GNU install dereferences symlinks, making root read the target
//!    for the attacker. Instead each source is opened by the user with
//!    `O_NOFOLLOW`, verified via fstat on the descriptor (regular file,
//!    owned by us), and root receives `/proc/<our-pid>/fd/<n>`: it copies
//!    the inode we vetted, not whatever the pathname resolves to later. A
//!    malicious crate can still ship a malicious binary — we are installing
//!    its program, after all — but it cannot use cargo-lbin as a confused deputy
//!    to exfiltrate root-only files.

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const SUDO: &str = "/usr/bin/sudo";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/usr/bin/rm";
const MKDIR: &str = "/usr/bin/mkdir";
const TOUCH: &str = "/usr/bin/touch";
const MV: &str = "/usr/bin/mv";
const CHMOD: &str = "/usr/bin/chmod";
/// Trusted locations for restorecon; deliberately not `$PATH`.
const RESTORECON_CANDIDATES: &[&str] = &[
    "/usr/sbin/restorecon",
    "/usr/bin/restorecon",
    "/sbin/restorecon",
];

/// The one prefix for which escalation is allowed. Under `/usr/local` every
/// path component is root-owned, so an unprivileged build script cannot swap
/// a parent directory for a symlink between our writability check and root's
/// `install`. Any other prefix that needs sudo is refused: its parents may
/// be user-controlled, reopening the destination-side TOCTOU that pinning
/// the source fd does not cover. Writable prefixes (`~/.local`, a
/// user-owned `/opt/foo`) never escalate and are always fine.
const CANONICAL_PREFIX: &str = "/usr/local";

/// Whether sudo may be used at all for a given operation. Derived once from
/// the prefix and threaded through every privileged call, so escalation is a
/// capability a call site must be handed rather than a runtime decision it
/// re-derives (and could re-derive differently if a user-controlled prefix
/// component changes between checks). The invariant is auditable at a
/// glance: `/usr/local` may escalate, everything else never does.
#[derive(Clone, Copy)]
pub enum Escalation {
    /// sudo permitted where the destination is not user-writable.
    Allowed,
    /// sudo never used; a non-writable destination is a hard error.
    Forbidden,
}

impl Escalation {
    /// The policy for a prefix. Only the canonical prefix may escalate.
    pub fn for_prefix(prefix: &Path) -> Self {
        if prefix == Path::new(CANONICAL_PREFIX) {
            Self::Allowed
        } else {
            Self::Forbidden
        }
    }

    /// Public probe with the same semantics as the internal decision:
    /// commands call it before starting a long build so a forbidden
    /// destination fails in milliseconds, not minutes.
    pub fn probe_destination(self, dir: &Path) -> Result<bool> {
        self.escalate_for(dir)
    }

    /// Decide whether to prepend sudo for a write into `dir`, or fail.
    /// Under `Forbidden`, a non-writable directory is refused rather than
    /// escalated — this is what actually stops a privileged custom prefix,
    /// at every call site, not just at a one-time pre-check.
    fn escalate_for(self, dir: &Path) -> Result<bool> {
        let needs = needs_privilege(dir);
        match (self, needs) {
            (_, false) => Ok(false),
            (Self::Allowed, true) => Ok(true),
            (Self::Forbidden, true) => bail!(
                "refusing to write to {} with elevated privileges: only {CANONICAL_PREFIX} \
                 is supported as a privileged prefix (its parents are root-owned and cannot \
                 be swapped mid-operation). Use a writable --prefix such as ~/.local instead.",
                dir.display()
            ),
        }
    }
}

/// How many candidate probe names to try before giving up. Exhausting them
/// takes deliberate squatting; the failure direction is conservative
/// ("not writable"), never destructive.
const PROBE_ATTEMPTS: u32 = 8;

/// Can the current user create files under `dir` (creating it if missing)?
///
/// The create-if-missing probe is deliberate: it matches what `install -D`
/// would do, and if the user can create the directory themselves, the
/// subsequent write should also happen as the user.
fn dir_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    // Exclusive create: `fs::write` would truncate an existing file of the
    // probe's name — or follow a symlink planted under it — and a probe
    // must never destroy what it finds. `create_new` is `O_CREAT|O_EXCL`,
    // which refuses anything that already exists, symlinks (even dangling
    // ones) included. The PID plus a retry suffix keeps a stale probe left
    // by a crashed run from turning into a false "destination requires
    // sudo": `AlreadyExists` proves nothing about writability, so try the
    // next name; any other error is the actual answer.
    let pid = std::process::id();
    for attempt in 0..PROBE_ATTEMPTS {
        let probe = dir.join(format!(".cargo-lbin-write-probe.{pid}.{attempt}"));
        match OpenOptions::new().write(true).create_new(true).open(&probe) {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe);
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return false,
        }
    }
    // Every candidate occupied: refuse to guess rather than touch anything.
    false
}

/// Does writing into `dir` require sudo?
pub fn needs_privilege(dir: &Path) -> bool {
    !dir_writable(dir)
}

/// Escalation decision for a set of existing paths under a policy: sudo if
/// any parent directory is not writable and escalation is allowed; a
/// non-writable parent under `Forbidden` is an error, not a silent escalate.
fn escalate_for_paths(policy: Escalation, paths: &[&Path]) -> Result<bool> {
    let mut escalate = false;
    for parent in paths.iter().filter_map(|p| p.parent()) {
        escalate |= policy.escalate_for(parent)?;
    }
    Ok(escalate)
}

/// Run `program args...` by absolute path, prepending `/usr/bin/sudo` when
/// `escalate` is true.
fn run(escalate: bool, program: &str, args: &[&OsStr]) -> Result<()> {
    let spawned = if escalate {
        format!("{SUDO} {program}")
    } else {
        program.to_owned()
    };
    let mut cmd = if escalate {
        let mut c = Command::new(SUDO);
        c.arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.args(args);
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {spawned}"))?;
    if !status.success() {
        bail!("{spawned} exited with {status}");
    }
    Ok(())
}

/// A staged source file opened and verified by the user, presented to
/// privileged `install` as a `/proc` fd path so the vetted inode — not a
/// swappable pathname — is what root copies. The handle must stay alive
/// until the copy is done; dropping it invalidates the proc path.
#[derive(Debug)]
pub struct VerifiedSource {
    file: File,
}

impl VerifiedSource {
    /// Open with `O_NOFOLLOW` (a symlink as the final component fails) and
    /// verify on the descriptor itself that this is a regular file owned by
    /// the current user. Directory components are resolved as the invoking
    /// user, so a symlinked parent cannot grant access the user lacks.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("opening staged binary {}", path.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("fstat on staged binary {}", path.display()))?;
        if !meta.is_file() {
            bail!("staged {} is not a regular file", path.display());
        }
        // SAFETY: geteuid cannot fail and has no preconditions.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            bail!(
                "staged {} is owned by uid {}, not the invoking user ({euid})",
                path.display(),
                meta.uid()
            );
        }
        Ok(Self { file })
    }

    fn proc_path(&self) -> PathBuf {
        // Not /proc/self: for escalated placement the path is resolved by
        // the install process, whose `self` is not us.
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }
}

/// Trusted generated data (the manifest) handed to root as an immutable
/// buffer: an anonymous memfd with write/grow/shrink seals. This is a
/// deliberately different threat model from `VerifiedSource`: for staged
/// binaries pinning the *inode* suffices, because the crate controls the
/// content anyway — swapping EVIL1 for EVIL2 gains it nothing. The
/// manifest is cargo-lbin's own trusted state, and an fd-pinned inode in the
/// cache can still be opened by pathname and rewritten in place by any
/// leftover same-UID build process. A sealed memfd pins the *bytes*:
/// after `F_SEAL_WRITE` no process — same UID included — can alter what
/// root will copy, and there is no pathname in the filesystem to find in
/// the first place. The invariant becomes "bytes serialized by cargo-lbin ==
/// bytes copied by root", not merely "inode opened == inode copied".
#[derive(Debug)]
pub struct SealedSource {
    file: File,
}

impl SealedSource {
    pub fn from_bytes(contents: &[u8]) -> Result<Self> {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        // SAFETY: the c"" literal is a &'static CStr, so NUL termination is
        // guaranteed by the type; memfd_create allocates a new descriptor
        // with no other preconditions.
        let fd = unsafe {
            libc::memfd_create(
                c"cargo-lbin-manifest".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        // SAFETY: fd is fresh and exclusively owned from here on.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(contents)
            .context("writing sealed manifest buffer")?;
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: valid memfd descriptor; F_ADD_SEALS has no memory
        // preconditions.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
            return Err(std::io::Error::last_os_error()).context("sealing manifest buffer");
        }
        // The memfd is reachable via /proc/<pid>/fd from the moment of
        // memfd_create — MFD_CLOEXEC governs fd inheritance across exec,
        // not procfs opens — so a same-UID process could race a write in
        // between our write and the seal. Read the now-frozen content back
        // and compare: whatever happened before F_SEAL_WRITE, this check
        // inspects the final immutable state, so a pass genuinely means
        // "bytes serialized by cargo-lbin == bytes root will copy".
        {
            use std::os::unix::fs::FileExt;
            let len = file
                .metadata()
                .context("fstat on sealed manifest buffer")?
                .len();
            if len != contents.len() as u64 {
                bail!("sealed manifest buffer was tampered with before sealing");
            }
            let mut check = vec![0u8; contents.len()];
            file.read_exact_at(&mut check, 0)
                .context("reading back sealed manifest buffer")?;
            if check != contents {
                bail!("sealed manifest buffer was tampered with before sealing");
            }
        }
        Ok(Self { file })
    }

    fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }
}

/// Shared placement: hand privileged `install` a `/proc` fd path. One
/// invocation per file: with a `/proc` fd path, `install -t` would name the
/// destination after the fd number, so the destination is always explicit.
fn install_from_proc(policy: Escalation, proc_path: &Path, dest: &Path, mode: &str) -> Result<()> {
    let parent = dest.parent().context("destination has no parent directory")?;
    let escalate = policy.escalate_for(parent)?;
    let mode_flag = format!("-Dm{mode}");
    run(
        escalate,
        INSTALL,
        &[mode_flag.as_ref(), proc_path.as_os_str(), dest.as_os_str()],
    )
}

/// Place a verified staged source (inode-pinned) at `dest`, atomically.
/// Like the manifest: install into a hidden temp in the same directory,
/// then `mv -fT` (a same-filesystem rename). A crash mid-update leaves the
/// previous working binary intact rather than a half-written one. The
/// destination directory is root-owned for the canonical prefix (the only
/// one for which `policy` permits escalation), so the temp cannot be
/// tampered with.
pub fn install_verified(
    policy: Escalation,
    src: &VerifiedSource,
    dest: &Path,
    mode: &str,
) -> Result<()> {
    install_atomic(policy, &src.proc_path(), dest, mode)
}

/// Shared atomic placement: install `proc_path` to a same-dir temp, rename
/// over `dest`. On rename failure the temp is best-effort removed.
///
/// `place_and_commit`'s rollback leans on this atomicity: a failed
/// placement leaves the destination untouched, so the set of successfully
/// placed new names equals the set of new names present on disk.
fn install_atomic(policy: Escalation, proc_path: &Path, dest: &Path, mode: &str) -> Result<()> {
    let parent = dest
        .parent()
        .context("destination has no parent directory")?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .context("destination has no file name")?;
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    install_from_proc(policy, proc_path, &tmp, mode)?;
    let escalate = policy.escalate_for(parent)?;
    let moved = run(
        escalate,
        MV,
        &["-fT".as_ref(), "--".as_ref(), tmp.as_os_str(), dest.as_os_str()],
    );
    if moved.is_err() {
        let _ = run(escalate, RM, &["-f".as_ref(), "--".as_ref(), tmp.as_os_str()]);
    }
    moved
}

/// Place sealed generated data (byte-pinned) at `dest`, atomically.
///
/// GNU `install` over an existing destination keeps the inode and
/// truncate-and-copies in place, so a crash mid-copy would leave half a
/// manifest. `install_atomic` installs into a temp in the same directory
/// and `mv -fT`s it into place, a same-filesystem `rename(2)`: crash before
/// the rename leaves the old manifest whole, crash after leaves the new one.
pub fn install_sealed(policy: Escalation, src: &SealedSource, dest: &Path, mode: &str) -> Result<()> {
    install_atomic(policy, &src.proc_path(), dest, mode)
}

/// Remove files, escalated if any of their directories require it.
pub fn remove_files(policy: Escalation, paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let escalate = escalate_for_paths(policy, paths)?;
    let mut args: Vec<&OsStr> = vec!["-f".as_ref(), "--".as_ref()];
    args.extend(paths.iter().map(|p| p.as_os_str()));
    run(escalate, RM, &args)
}

/// Create the state lock file, escalating if needed. Deliberately
/// `mkdir -p` + `touch` rather than `install`: both are idempotent, and
/// touch on an existing file updates timestamps but preserves the inode.
/// That matters — `install` unlinks and recreates, so a process holding a
/// flock on the old inode and one locking the new file would both "hold
/// the lock" while excluding nobody.
pub fn ensure_lock_file(policy: Escalation, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("lock path has no parent directory")?;
    let escalate = policy.escalate_for(parent)?;
    run(escalate, MKDIR, &["-p".as_ref(), parent.as_os_str()])?;
    run(escalate, TOUCH, &[path.as_os_str()])?;
    // Explicit modes: mkdir/touch inherit the caller's umask, and a user
    // with umask 077 would otherwise mint a 0700 state dir and 0600 lock
    // that every other user's `cargo-lbin list` cannot even open. chmod on an
    // existing file preserves the inode, so flock correctness is intact.
    run(escalate, CHMOD, &["0755".as_ref(), parent.as_os_str()])?;
    run(escalate, CHMOD, &["0644".as_ref(), path.as_os_str()])?;
    Ok(())
}

/// Best-effort `SELinux` relabel (matters on Fedora, absent and harmless on
/// Arch). Failures are deliberately ignored: on most systems restorecon
/// either does not exist or is a no-op for `bin_t`.
pub fn restorecon(policy: Escalation, paths: &[&Path]) {
    let Some(program) = RESTORECON_CANDIDATES
        .iter()
        .find(|c| Path::new(c).is_file())
    else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    // Best effort throughout: a Forbidden policy that would need escalation
    // simply skips the relabel rather than erroring.
    let Ok(escalate) = escalate_for_paths(policy, paths) else {
        return;
    };
    let mut args: Vec<&OsStr> = Vec::with_capacity(paths.len());
    args.extend(paths.iter().map(|p| p.as_os_str()));
    let _ = run(escalate, program, &args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_probe_never_destroys_existing_files() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-probe");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();

        // Squat the first candidate with real content: the probe must
        // neither truncate it nor let it flip the verdict — the retry
        // suffix sidesteps the name.
        let squatted = dir.join(format!(".cargo-lbin-write-probe.{pid}.0"));
        fs::write(&squatted, b"precious").unwrap();
        // Squat the second with a symlink to a real file: the old
        // `fs::write` probe would have followed it and truncated the
        // target; `create_new` refuses to open it at all.
        let target = dir.join("symlink-target");
        fs::write(&target, b"target-content").unwrap();
        let link = dir.join(format!(".cargo-lbin-write-probe.{pid}.1"));
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(dir_writable(&dir), "retry suffixes must sidestep squats");
        assert_eq!(fs::read(&squatted).unwrap(), b"precious");
        assert_eq!(fs::read(&target).unwrap(), b"target-content");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

        // Every candidate squatted: conservative refusal, nothing touched.
        for attempt in 0..PROBE_ATTEMPTS {
            let name = dir.join(format!(".cargo-lbin-write-probe.{pid}.{attempt}"));
            if name.symlink_metadata().is_err() {
                fs::write(&name, b"squat").unwrap();
            }
        }
        assert!(!dir_writable(&dir));
        assert_eq!(fs::read(&squatted).unwrap(), b"precious");
        assert_eq!(fs::read(&target).unwrap(), b"target-content");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_source_rejects_symlinks_and_accepts_regular_files() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-verified");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let real = dir.join("real");
        fs::write(&real, b"binary").unwrap();
        assert!(VerifiedSource::open(&real).is_ok());

        let link = dir.join("link");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
        let err = VerifiedSource::open(&link).unwrap_err();
        // O_NOFOLLOW makes the open itself fail with ELOOP.
        assert!(err.to_string().contains("opening staged binary"), "{err:#}");

        assert!(VerifiedSource::open(&dir).is_err(), "directories rejected");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn escalation_policy_only_for_canonical_prefix() {
        // Writable dir: no escalation regardless of policy.
        let writable = std::env::temp_dir().join("cargo-lbin-test-esc-ok");
        let _ = fs::remove_dir_all(&writable);
        fs::create_dir_all(&writable).unwrap();
        assert!(!Escalation::Forbidden.escalate_for(&writable).unwrap());
        assert!(!Escalation::Allowed.escalate_for(&writable).unwrap());

        // Non-writable dir: Allowed escalates, Forbidden errors.
        let hostile = Path::new("/proc/cargo-lbin-nonexistent-esc/dir");
        if needs_privilege(hostile) {
            assert!(Escalation::Allowed.escalate_for(hostile).unwrap());
            let err = Escalation::Forbidden
                .escalate_for(hostile)
                .unwrap_err()
                .to_string();
            assert!(err.contains("only /usr/local"), "{err}");
        }

        // Policy derivation.
        assert!(matches!(
            Escalation::for_prefix(Path::new(CANONICAL_PREFIX)),
            Escalation::Allowed
        ));
        assert!(matches!(
            Escalation::for_prefix(Path::new("/tmp/whatever")),
            Escalation::Forbidden
        ));
        let _ = fs::remove_dir_all(&writable);
    }

    #[test]
    fn forbidden_policy_blocks_lock_escalation() {
        // The exact hole from review: a custom prefix whose share/cargo-lbin is
        // not writable must not escalate when preparing the lock — it must
        // error, before any sudo is spawned.
        let hostile = Path::new("/proc/cargo-lbin-nonexistent-lock/share/cargo-lbin/lock");
        if needs_privilege(hostile.parent().unwrap()) {
            let err = ensure_lock_file(Escalation::Forbidden, hostile)
                .unwrap_err()
                .to_string();
            assert!(err.contains("only /usr/local"), "{err}");
        }
    }

    #[test]
    fn ensure_lock_file_forces_world_readable_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join("cargo-lbin-test-umask");
        let _ = fs::remove_dir_all(&dir);
        let state = dir.join("share/cargo-lbin");
        // Pre-create at hostile modes (as a umask-077 environment would),
        // without touching the process-global umask — cargo runs tests in
        // parallel and that mutation could bleed into a concurrent test.
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = state.join("lock");
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();

        ensure_lock_file(Escalation::Allowed, &lock).unwrap();

        let dir_mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        let lock_mode = fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o755, "state dir must be world-traversable");
        assert_eq!(lock_mode, 0o644, "lock must be world-readable for shared flock");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sealed_source_is_immutable_even_for_owner() {
        use std::io::Write;
        let sealed = SealedSource::from_bytes(b"TRUSTED").unwrap();
        // The exact attack: a same-UID process opens the fd path for
        // writing and tries to rewrite the content in place. The write
        // seal must stop it — ownership is irrelevant at this layer.
        let reopened = OpenOptions::new().write(true).open(sealed.proc_path());
        let mutated = match reopened {
            // Kernel may refuse at open (O_TRUNC-less write open can
            // succeed) or at write; either way no byte may change.
            Ok(mut f) => f.write_all(b"FORGED!").is_ok(),
            Err(_) => false,
        };
        assert!(!mutated, "seal failed: content was mutated");
        assert_eq!(fs::read(sealed.proc_path()).unwrap(), b"TRUSTED");
    }

    #[test]
    fn proc_path_points_at_our_open_descriptor() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-procfd");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src");
        fs::write(&src, b"CONTENT").unwrap();
        let verified = VerifiedSource::open(&src).unwrap();
        // Even after the pathname is swapped for a symlink, the proc path
        // still reads the vetted inode.
        fs::remove_file(&src).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &src).unwrap();
        let read = fs::read(verified.proc_path()).unwrap();
        assert_eq!(read, b"CONTENT");
        let _ = fs::remove_dir_all(&dir);
    }
}
