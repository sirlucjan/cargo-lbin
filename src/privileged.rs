//! Privilege handling: build as the user, escalate only for protected
//! filesystem mutations under the canonical prefix — placement,
//! retirement, a mutation of what is installed, and state-lock
//! initialization. Sudo is prepended only when the filesystem
//! operation actually being performed requires it, decided per
//! destination, not per prefix — and never around Cargo or the build.
//!
//! Two hardening rules throughout, because build scripts run as the
//! user first and must not steer this:
//!
//! 1. Nothing privileged resolves through `$PATH` — a build script can
//!    drop a fake `sudo` into `~/.local/bin`; `/usr/bin/sudo` it
//!    cannot replace. Absolute paths only; restorecon from trusted
//!    directories.
//!
//! 2. Staged binaries are never handed to root by pathname: the stage
//!    is user-controlled, and GNU install dereferences symlinks. Each
//!    source is opened with `O_NOFOLLOW`, verified via fstat, and root
//!    receives `/proc/<pid>/fd/<n>` — the vetted inode, not whatever
//!    the pathname resolves to later. A malicious crate can still ship
//!    a malicious binary, but not use cargo-lbin as a confused deputy
//!    to read root-only files.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// The one prefix escalation is allowed for. Under `/usr/local` every
/// component is root-owned, so a build script cannot swap a parent for
/// a symlink between our check and root's `install`; any other prefix
/// needing sudo is refused — its parents may be user-controlled.
const CANONICAL_PREFIX: &str = "/usr/local";

/// May this operation use sudo at all? One axis of `Policy`, derived
/// once from the prefix and threaded through — a capability a call
/// site is handed, never a decision it re-derives.
#[derive(Clone, Copy)]
pub enum Sudo {
    /// Permitted where the destination is not user-writable.
    Allowed,
    /// Never used; a non-writable destination is a hard error.
    Forbidden,
}

/// Who owns the terminal — deliberately independent of `Sudo`: an
/// unprivileged `mv` failing under a TUI corrupts the screen exactly
/// as thoroughly as a privileged one.
#[derive(Clone, Copy)]
pub enum Screen {
    /// The caller's terminal: children inherit stdio, sudo may prompt.
    Inherited,
    /// A frontend owns the screen: every child is captured, its words
    /// travel in errors, and sudo runs `-n` — needing a password is a loud
    /// diagnosis, not a prompt hung invisibly. `-v` beforehand is a
    /// convenience, never a proof.
    Owned,
}

/// What a privileged call site is handed: both decisions, made once at
/// the operation's edge — neither axis can silently erase the other.
#[derive(Clone, Copy)]
pub struct Policy {
    pub sudo: Sudo,
    pub screen: Screen,
}

impl Policy {
    /// The policy for a prefix, on the caller's own terminal. Only the
    /// canonical prefix may escalate.
    pub fn for_prefix(prefix: &Path) -> Self {
        let sudo = if prefix == Path::new(CANONICAL_PREFIX) {
            Sudo::Allowed
        } else {
            Sudo::Forbidden
        };
        Self {
            sudo,
            screen: Screen::Inherited,
        }
    }

    /// The same sudo decision under a screen-owning frontend; a CLI-only
    /// build never calls this.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn screen_owned(self) -> Self {
        Self {
            screen: Screen::Owned,
            ..self
        }
    }

    /// Public probe with the internal decision's semantics: a forbidden
    /// destination fails in milliseconds, not after a build.
    pub fn probe_destination(self, dir: &Path) -> Result<bool> {
        self.escalate_for(dir)
    }

    /// Prepend sudo for a write into `dir`, or fail: under `Forbidden` a
    /// non-writable directory is refused — this is what actually stops a
    /// privileged custom prefix, at every call site.
    fn escalate_for(self, dir: &Path) -> Result<bool> {
        let needs = needs_privilege(dir);
        match (self.sudo, needs) {
            (_, false) => Ok(false),
            (Sudo::Allowed, true) => Ok(true),
            (Sudo::Forbidden, true) => bail!(
                "refusing to write to {} with elevated privileges: only {CANONICAL_PREFIX} \
                 is supported as a privileged prefix (its parents are root-owned and cannot \
                 be swapped mid-operation). Use a writable --prefix such as ~/.local instead.",
                dir.display()
            ),
        }
    }
}

/// Probe names tried before giving up; the failure direction is
/// conservative ("not writable"), never destructive.
const PROBE_ATTEMPTS: u32 = 8;

/// Can the user create files under `dir` (creating it if missing)?
/// Create-if-missing matches `install -D`: if the user can create the
/// directory, the write should also happen as the user.
fn dir_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    // Exclusive create: `fs::write` would truncate an existing file or
    // follow a planted symlink; `create_new` (O_CREAT|O_EXCL) refuses
    // both. PID + retry suffix keeps a stale probe from a crashed run
    // from proving anything: AlreadyExists says nothing about
    // writability, so try the next name.
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

/// Is sudo's credential timestamp warm right now?
///
/// Asked where a password is about to be spent — the placement door —
/// and never warmed in advance for a build that may fail. In a batch
/// a timestamp may still be warm from an earlier member's placement;
/// every later build is a plain user's build regardless, because a
/// warm timestamp is permission to ask sudo, not an identity cargo
/// runs under. `sudo -n -v` asks about the
/// timestamp itself, not any command; only when sudo would prompt is
/// the reason announced, and `sudo -v` then owns the prompt —
/// cargo-lbin never reads, buffers or forwards the password, a design
/// rule. Sudo may still re-ask at the privileged calls themselves: its
/// policy, deliberately not worked around. Preauthorization is UX, not
/// proof: the later calls authorize on their own terms.
pub fn credentials_fresh() -> Result<bool> {
    Ok(Command::new(SUDO)
        .arg("-n")
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to spawn {SUDO}"))?
        .success())
}

/// Why cargo-lbin is about to ask for a password.
///
/// Sudo prompts for a user and never for a reason, and the prefix
/// alone does not supply one: a migration escalates for its
/// destination while placing and for its source while retiring. The
/// retirement is the case that needs saying most — it arrives after
/// the build, when nothing on screen looks like it is about
/// privileges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthPurpose {
    Placement,
    Retirement,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    /// A removal or a pin flip: the manifest, and for a removal the
    /// binaries it names. Not an installation, and the sentence above
    /// sudo's prompt must not say it is — the least a person is owed
    /// for a password is an accurate reason.
    Mutation,
}

/// The sentence printed before sudo's own prompt, wherever the prompt
/// happens — the CLI's terminal or the bare screen the TUI steps off.
/// One source, so the two surfaces cannot word the same moment
/// differently. Named by prefix, not bin: the writes may be binaries,
/// state or the lock — "into .../bin" would state a false reason for
/// the latter two.
#[must_use]
pub fn requirement_line(purpose: AuthPurpose, prefix: &Path) -> String {
    let what = match purpose {
        AuthPurpose::Placement => "install under",
        AuthPurpose::Retirement => "retire the source installation from",
        AuthPurpose::Mutation => "change what is installed under",
    };
    crate::text::sanitize(&format!(
        "administrative privileges are required to {what} {}",
        prefix.display()
    ))
}

pub fn preauthorize(prefix: &Path, escalate: bool, purpose: AuthPurpose) -> Result<()> {
    if !escalate {
        return Ok(());
    }
    if credentials_fresh()? {
        return Ok(());
    }
    eprintln!("{}", requirement_line(purpose, prefix));
    let status = Command::new(SUDO)
        .arg("-v")
        .status()
        .with_context(|| format!("failed to spawn {SUDO}"))?;
    if !status.success() {
        bail!("sudo authentication failed");
    }
    Ok(())
}

/// Sudo decision for a set of existing paths under a policy: escalate if
/// any parent directory is not writable and escalation is allowed; a
/// non-writable parent under `Forbidden` is an error, not a silent escalate.
fn escalate_for_paths(policy: Policy, paths: &[&Path]) -> Result<bool> {
    let mut escalate = false;
    for parent in paths.iter().filter_map(|p| p.parent()) {
        escalate |= policy.escalate_for(parent)?;
    }
    Ok(escalate)
}

/// Run `program args...` by absolute path, prepending `/usr/bin/sudo`
/// when decided. Inherited terminal: child gets stdio, sudo may
/// prompt. Owned screen: every child captured, output folded into the
/// error, sudo runs `-n`.
fn run(policy: Policy, escalate: bool, program: &str, args: &[&OsStr]) -> Result<()> {
    let spawned = if escalate {
        format!("{SUDO} {program}")
    } else {
        program.to_owned()
    };
    let mut cmd = if escalate {
        let mut c = Command::new(SUDO);
        if matches!(policy.screen, Screen::Owned) {
            c.arg("-n");
        }
        c.arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.args(args);
    if matches!(policy.screen, Screen::Owned) {
        let output = cmd
            .output()
            .with_context(|| format!("failed to spawn {spawned}"))?;
        if !output.status.success() {
            // The same rule as every external string headed for a Span: this text
            // ends in a BuildReport.
            let words = crate::text::sanitize(&String::from_utf8_lossy(&output.stderr));
            let words = words.trim();
            if words.is_empty() {
                bail!("{spawned} exited with {}", output.status);
            }
            bail!("{spawned} exited with {}: {words}", output.status);
        }
        return Ok(());
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {spawned}"))?;
    if !status.success() {
        bail!("{spawned} exited with {status}");
    }
    Ok(())
}

/// A staged source opened and verified by the user, presented to
/// privileged `install` as a `/proc` fd path — root copies the vetted
/// inode, not a swappable pathname. The handle must outlive the copy.
#[derive(Debug)]
pub struct VerifiedSource {
    file: File,
}

impl VerifiedSource {
    /// Open with `O_NOFOLLOW` and verify on the descriptor: regular file,
    /// owned by us. Parents resolve as the invoking user, so a symlinked
    /// parent grants nothing the user lacks.
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

/// Trusted generated data (the manifest) handed to root as a sealed
/// memfd. A different threat model from `VerifiedSource`: staged
/// binaries only need the *inode* pinned (the crate controls the
/// content anyway), while the manifest is our own trusted state and a
/// cached inode could be rewritten in place by any same-UID process.
/// The seal pins the *bytes*: after `F_SEAL_WRITE` nobody can alter
/// what root will copy, and there is no pathname to find.
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
        // The memfd is reachable via /proc from creation (MFD_CLOEXEC governs
        // exec, not procfs), so a same-UID write could race ours. Read the
        // now-frozen content back and compare: a pass inspects the final
        // sealed state — bytes serialized == bytes root copies.
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

/// Shared placement: privileged `install`, one invocation per file —
/// with a `/proc` fd path, `install -t` would name the destination
/// after the fd number.
fn install_from_proc(policy: Policy, proc_path: &Path, dest: &Path, mode: &str) -> Result<()> {
    let parent = dest
        .parent()
        .context("destination has no parent directory")?;
    let escalate = policy.escalate_for(parent)?;
    let mode_flag = format!("-Dm{mode}");
    run(
        policy,
        escalate,
        INSTALL,
        &[mode_flag.as_ref(), proc_path.as_os_str(), dest.as_os_str()],
    )
}

/// Place a verified source at `dest` atomically: hidden same-dir temp,
/// then `mv -fT` (same-fs rename) — a crash mid-update leaves the
/// previous binary intact. The directory is root-owned for the one
/// escalating prefix, so the temp cannot be tampered with.
pub fn install_verified(
    policy: Policy,
    src: &VerifiedSource,
    dest: &Path,
    mode: &str,
) -> Result<()> {
    install_atomic(policy, &src.proc_path(), dest, mode)
}

/// Shared atomic placement: install to a same-dir temp, rename over
/// `dest`; on rename failure the temp is best-effort removed.
/// `place_and_commit`'s rollback leans on this: a failed placement
/// leaves the destination untouched.
fn install_atomic(policy: Policy, proc_path: &Path, dest: &Path, mode: &str) -> Result<()> {
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
        policy,
        escalate,
        MV,
        &[
            "-fT".as_ref(),
            "--".as_ref(),
            tmp.as_os_str(),
            dest.as_os_str(),
        ],
    );
    if moved.is_err() {
        let _ = run(
            policy,
            escalate,
            RM,
            &["-f".as_ref(), "--".as_ref(), tmp.as_os_str()],
        );
    }
    moved
}

/// Place sealed data at `dest` atomically: GNU `install` over an
/// existing file truncate-and-copies in place, so a crash would leave
/// half a manifest; temp + `mv -fT` leaves whole old or whole new.
pub fn install_sealed(policy: Policy, src: &SealedSource, dest: &Path, mode: &str) -> Result<()> {
    install_atomic(policy, &src.proc_path(), dest, mode)
}

/// Remove files, escalated if any of their directories require it.
pub fn remove_files(policy: Policy, paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let escalate = escalate_for_paths(policy, paths)?;
    let mut args: Vec<&OsStr> = vec!["-f".as_ref(), "--".as_ref()];
    args.extend(paths.iter().map(|p| p.as_os_str()));
    run(policy, escalate, RM, &args)
}

/// Create the state lock file. `mkdir -p` + `touch`, not `install`:
/// touch preserves the inode — `install` unlinks and recreates, and
/// two processes flocking old and new inodes would both "hold the
/// lock" while excluding nobody.
pub fn ensure_lock_file(policy: Policy, path: &Path) -> Result<()> {
    let parent = path.parent().context("lock path has no parent directory")?;
    let escalate = policy.escalate_for(parent)?;
    run(
        policy,
        escalate,
        MKDIR,
        &["-p".as_ref(), parent.as_os_str()],
    )?;
    run(policy, escalate, TOUCH, &[path.as_os_str()])?;
    // Explicit modes: umask 077 would otherwise mint a state dir other
    // users' `list` cannot open; chmod preserves the inode, so flock
    // correctness is intact.
    run(
        policy,
        escalate,
        CHMOD,
        &["0755".as_ref(), parent.as_os_str()],
    )?;
    run(
        policy,
        escalate,
        CHMOD,
        &["0644".as_ref(), path.as_os_str()],
    )?;
    Ok(())
}

/// Best-effort `SELinux` relabel (Fedora; absent and harmless on Arch).
pub fn restorecon(policy: Policy, paths: &[&Path]) {
    let Some(program) = RESTORECON_CANDIDATES
        .iter()
        .find(|c| Path::new(c).is_file())
    else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    // Best effort throughout: Forbidden-needing-escalation skips rather
    // than errors.
    let Ok(escalate) = escalate_for_paths(policy, paths) else {
        return;
    };
    let mut args: Vec<&OsStr> = Vec::with_capacity(paths.len());
    args.extend(paths.iter().map(|p| p.as_os_str()));
    let _ = run(policy, escalate, program, &args);
}

#[cfg(test)]
mod tests {
    /// The prompt names the phase asking for it. Sudo says who you are;
    /// only cargo-lbin knows what it is about to do, and a migration
    /// asks for two prefixes for two different reasons. One sentence,
    /// used by the CLI and by the screen the TUI steps off, so the two
    /// cannot word the same moment differently.
    #[test]
    fn the_password_requirement_says_what_it_is_for() {
        let prefix = Path::new("/usr/local");
        assert_eq!(
            requirement_line(AuthPurpose::Placement, prefix),
            "administrative privileges are required to install under /usr/local"
        );
        assert_eq!(
            requirement_line(AuthPurpose::Retirement, prefix),
            "administrative privileges are required to retire the source installation from \
             /usr/local"
        );
        // A removal or a pin flip is not an installation, and the one
        // sentence a person gets for a password must not say it is.
        let mutation = requirement_line(AuthPurpose::Mutation, prefix);
        assert_eq!(
            mutation,
            "administrative privileges are required to change what is installed under /usr/local"
        );
        assert!(
            !mutation.contains("install under"),
            "which would name an operation that is not happening: {mutation}"
        );
        // A prefix can come from the environment: sanitized like every
        // other external string that reaches a terminal.
        let hostile = PathBuf::from("/usr/\x1b[31mlocal");
        assert!(
            !requirement_line(AuthPurpose::Placement, &hostile).contains('\x1b'),
            "{}",
            requirement_line(AuthPurpose::Placement, &hostile)
        );
    }

    use super::*;

    #[test]
    fn write_probe_never_destroys_existing_files() {
        let dir = std::env::temp_dir().join("cargo-lbin-test-probe");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();

        // Squat the first candidate with real content: the retry suffix
        // sidesteps the name without truncating it.
        let squatted = dir.join(format!(".cargo-lbin-write-probe.{pid}.0"));
        fs::write(&squatted, b"precious").unwrap();
        // Squat the second with a symlink: the old `fs::write` probe would
        // have followed and truncated the target; `create_new` refuses.
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
        assert!(
            !Policy {
                sudo: Sudo::Forbidden,
                screen: Screen::Inherited
            }
            .escalate_for(&writable)
            .unwrap()
        );
        assert!(
            !Policy {
                sudo: Sudo::Allowed,
                screen: Screen::Inherited
            }
            .escalate_for(&writable)
            .unwrap()
        );

        // Non-writable dir: Allowed escalates, Forbidden errors.
        let hostile = Path::new("/proc/cargo-lbin-nonexistent-esc/dir");
        if needs_privilege(hostile) {
            assert!(
                Policy {
                    sudo: Sudo::Allowed,
                    screen: Screen::Inherited
                }
                .escalate_for(hostile)
                .unwrap()
            );
            let err = Policy {
                sudo: Sudo::Forbidden,
                screen: Screen::Inherited,
            }
            .escalate_for(hostile)
            .unwrap_err()
            .to_string();
            assert!(err.contains("only /usr/local"), "{err}");
        }

        // Policy derivation.
        assert!(matches!(
            Policy::for_prefix(Path::new(CANONICAL_PREFIX)),
            Policy {
                sudo: Sudo::Allowed,
                screen: Screen::Inherited
            }
        ));
        assert!(matches!(
            Policy::for_prefix(Path::new("/tmp/whatever")),
            Policy {
                sudo: Sudo::Forbidden,
                screen: Screen::Inherited
            }
        ));
        let _ = fs::remove_dir_all(&writable);
    }

    #[test]
    fn forbidden_policy_blocks_lock_escalation() {
        // The exact hole from review: a custom prefix whose share/ is not
        // writable must error preparing the lock, before any sudo spawns.
        let hostile = Path::new("/proc/cargo-lbin-nonexistent-lock/share/cargo-lbin/lock");
        if needs_privilege(hostile.parent().unwrap()) {
            let err = ensure_lock_file(
                Policy {
                    sudo: Sudo::Forbidden,
                    screen: Screen::Inherited,
                },
                hostile,
            )
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
        // Pre-create at hostile modes without touching the process-global
        // umask — tests run in parallel and that mutation would bleed.
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = state.join("lock");
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();

        ensure_lock_file(
            Policy {
                sudo: Sudo::Allowed,
                screen: Screen::Inherited,
            },
            &lock,
        )
        .unwrap();

        let dir_mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        let lock_mode = fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o755, "state dir must be world-traversable");
        assert_eq!(
            lock_mode, 0o644,
            "lock must be world-readable for shared flock"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sealed_source_is_immutable_even_for_owner() {
        use std::io::Write;
        let sealed = SealedSource::from_bytes(b"TRUSTED").unwrap();
        // The exact attack: a same-UID process opens the fd path and rewrites
        // in place; the write seal must stop it.
        let reopened = OpenOptions::new().write(true).open(sealed.proc_path());
        let mutated = match reopened {
            // Kernel may refuse at open or at write; either way no byte changes.
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
