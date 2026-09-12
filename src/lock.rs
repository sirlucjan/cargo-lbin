//! Prefix-scoped state lock.
//!
//! Serializes cargo-lbin instances operating on the same prefix: mutations
//! (install/update/remove/downgrade, pin/unpin, migrate) take an exclusive
//! lock for their whole duration, builds included; readers
//! (list/checkupdate) take a shared lock. The lock lives next to the state
//! (`<prefix>/share/cargo-lbin/lock`), so it protects the prefix rather than
//! a particular user — two different users driving `/usr/local` contend on
//! the same file.
//!
//! The lock must be acquired before `Manifest::load()`, otherwise the
//! load-mutate-store sequence is a textbook lost update.
//!
//! For a root-owned prefix the lock file cannot be created by an
//! unprivileged user, so exclusive acquisition prepares it via the
//! privileged path (`mkdir -p` + `touch` — idempotent and inode-preserving,
//! see `privileged::ensure_lock_file`). This is a filesystem bookkeeping
//! operation only; cargo and the build still always run as the user.
//! Readers on a prefix that has never seen a mutation degrade with a
//! warning instead of demanding sudo.
//!
//! Locking uses `std::fs::File`'s lock API (`flock(2)` on Linux); the lock
//! is advisory and scoped to the open file description, so it releases
//! when the guard drops and the descriptor closes — including on crash.

use crate::privileged;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

pub enum Mode {
    Shared,
    Exclusive,
}

/// Held for the guard's lifetime; the lock releases on drop (fd close).
pub struct StateLock {
    _file: Option<File>,
}

impl StateLock {
    /// A shared lock over state that already exists, or nothing — the
    /// read-only auditor's form. `acquire`'s preparation path writes
    /// twice on any writable prefix that has never held a lock
    /// (`create_dir_all`, then `O_CREAT`), which a command whose
    /// contract is "never writes" cannot use. This one opens the lock
    /// file read-only and never creates it or its parents; `Ok(None)`
    /// means there is no lock to take — the file is absent (nothing has
    /// ever prepared this prefix) or unopenable (a foreign prefix whose
    /// lock the user cannot read) — and the caller reads without one,
    /// which the atomic manifest placement keeps safe from torn files if
    /// not from a racing removal; an auditor's findings are advisory
    /// either way, and a spurious one costs a re-run, never state.
    /// Blocks like `acquire` when the lock exists — waiting out someone
    /// else's build beats auditing a prefix mid-placement — and says so
    /// through `notice`, the same channel split `acquire_with` uses: the
    /// CLI prints it to stderr, because a silent multi-minute wait on
    /// someone else's build is indistinguishable from a hang, while the
    /// TUI passes a noop — its spinner already says a job is alive, and
    /// a worker's eprintln beneath the alternate screen is exactly what
    /// this path's quiet design forbids.
    pub fn acquire_shared_existing(
        prefix: &Path,
        notice: &mut dyn FnMut(&str),
    ) -> Result<Option<Self>> {
        let path = prefix.join("share/cargo-lbin/lock");
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => file,
            // Absent: nothing ever prepared this prefix. Denied: a
            // foreign prefix whose lock the user cannot read — both mean
            // "read locklessly". Anything else (EIO, EMFILE) is a real
            // failure of *this* process or disk and deserves to surface,
            // not to be quietly rounded down to "no lock".
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("opening {}", path.display()));
            }
        };
        // The same probe-then-block shape as `acquire_impl`, for the
        // same reason: a non-blocking probe tells an actual wait apart
        // from an instant acquisition, so the notice fires only when
        // there is genuinely someone to wait for.
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                notice("another cargo-lbin instance holds the state lock; waiting...");
                file.lock_shared()
                    .with_context(|| format!("locking {} (shared)", path.display()))?;
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {} (shared)", path.display()));
            }
        }
        Ok(Some(Self { _file: Some(file) }))
    }

    /// Acquire the prefix lock, blocking if another instance holds it (with
    /// a notice, so a wait during someone else's 10-minute build is not
    /// mistaken for a hang). CLI form: lock-file preparation may prompt
    /// via sudo, notices go to stderr.
    pub fn acquire(prefix: &Path, mode: &Mode) -> Result<Self> {
        Self::acquire_with(
            prefix,
            mode,
            privileged::Policy::for_prefix(prefix),
            &mut |s| eprintln!("{s}"),
        )
    }

    /// `acquire` with the two decisions a screen-owning frontend must
    /// own decided by the caller: `policy` says whether preparing a
    /// missing lock file may prompt (a captured frontend passes a
    /// noninteractive policy, so the one path in this module that can
    /// reach sudo runs it as `sudo -n` — a prompt beneath an alternate
    /// screen would hang invisibly), and `notice` is where the human
    /// lines go — stderr on the CLI, the frontend's line stream under a
    /// TUI, nowhere for an advisory read that would rather stay silent.
    pub fn acquire_with(
        prefix: &Path,
        mode: &Mode,
        policy: privileged::Policy,
        notice: &mut dyn FnMut(&str),
    ) -> Result<Self> {
        Self::acquire_impl(prefix, mode, policy, notice, true)
            .map(|lock| lock.expect("blocking acquisition always returns a lock"))
    }

    /// `acquire_with` that does not wait: `Ok(None)` when another
    /// instance holds the lock. For advisory reads on a live UI thread —
    /// an optimization that would freeze the screen for the length of
    /// someone else's build is no optimization, and the authoritative
    /// check under the real lock happens elsewhere anyway.
    /// Only the TUI's advisory path calls this, so a CLI-only build
    /// never does.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn try_acquire_with(
        prefix: &Path,
        mode: &Mode,
        policy: privileged::Policy,
        notice: &mut dyn FnMut(&str),
    ) -> Result<Option<Self>> {
        Self::acquire_impl(prefix, mode, policy, notice, false)
    }

    /// Will acquiring on this prefix need the privileged preparation
    /// path? Mirrors `acquire`'s own first steps — create the parents
    /// best-effort, then try to open or create the file — so a frontend
    /// deciding whether to validate sudo up front sees exactly what the
    /// worker will see. A user-writable prefix answers `false` by
    /// creating the file here, which is the same file `acquire` would
    /// have created a moment later.
    /// Only the TUI's advisory path calls this, so a CLI-only build
    /// never does.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn preparation_needs_privilege(prefix: &Path) -> bool {
        let path = prefix.join("share/cargo-lbin/lock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .or_else(|_| OpenOptions::new().read(true).open(&path))
            .is_err()
    }

    fn acquire_impl(
        prefix: &Path,
        mode: &Mode,
        policy: privileged::Policy,
        notice: &mut dyn FnMut(&str),
        block: bool,
    ) -> Result<Option<Self>> {
        let path = prefix.join("share/cargo-lbin/lock");
        // OpenOptions::create makes the file, not its parents.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Plain attempt first: create it (writable prefixes, root), else
        // open the existing file read-only — flock needs no write access.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Explicitly no truncation: the file is opened purely to be
            // flocked, its (empty) content is never touched.
            .truncate(false)
            .open(&path)
            .or_else(|_| OpenOptions::new().read(true).open(&path));
        let file = match (file, mode) {
            (Ok(f), _) => f,
            (Err(_), Mode::Exclusive) => {
                // Mutations must not proceed unsynchronized: prepare the
                // lock file with escalation, then it must open. Announce
                // what is about to happen — this is the one code path that
                // can put a sudo prompt on the screen before any other
                // output, and a bare password prompt with no context looks
                // exactly like what this tool exists to prevent.
                notice(&format!(
                    "initializing state for {}: creating {}",
                    prefix.display(),
                    path.display()
                ));
                privileged::ensure_lock_file(policy, &path)
                    .with_context(|| format!("preparing state lock {}", path.display()))?;
                OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .with_context(|| format!("opening state lock {}", path.display()))?
            }
            (Err(_), Mode::Shared) => {
                // A reader on a prefix that never saw a mutation; nothing
                // to protect yet and no reason to demand sudo for a `list`.
                notice(&format!(
                    "warning: cannot open {} — proceeding without a state lock \
                     (the file is created by the first mutation)",
                    path.display()
                ));
                return Ok(Some(Self { _file: None }));
            }
        };
        // Non-blocking probe first, to tell an actual wait apart from an
        // instant acquisition — and `WouldBlock` apart from real I/O
        // errors, which must propagate rather than masquerade as
        // contention.
        let probe = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        match probe {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) if !block => return Ok(None),
            Err(TryLockError::WouldBlock) => {
                notice("another cargo-lbin instance holds the state lock; waiting...");
                match mode {
                    Mode::Shared => file.lock_shared(),
                    Mode::Exclusive => file.lock(),
                }
                .context("acquiring state lock")?;
            }
            Err(TryLockError::Error(e)) => return Err(e).context("acquiring state lock"),
        }
        Ok(Some(Self { _file: Some(file) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contended_shared_existing_wait_is_announced_and_a_free_one_is_not() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = std::env::temp_dir().join("cargo-lbin-test-lock-notice");
        let _ = std::fs::remove_dir_all(&root);
        let prefix = root.join("prefix");
        std::fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
        let lock_path = prefix.join("share/cargo-lbin/lock");

        // Free lock: instant acquisition, and the probe keeps the
        // notice quiet — nobody to wait for means nothing to announce.
        std::fs::File::create(&lock_path).unwrap();
        let mut fired = false;
        let got = StateLock::acquire_shared_existing(&prefix, &mut |_| fired = true)
            .unwrap()
            .expect("the lock file exists");
        assert!(!fired, "an instant acquisition says nothing");
        drop(got);

        // Contended: an exclusive holder on a separate description, the
        // waiter on a thread, the notice through a channel — it must
        // arrive while the holder still holds, or the wait was silent.
        let holder = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let p2 = prefix.clone();
        let waiter = std::thread::spawn(move || {
            let mut notice = |m: &str| {
                let _ = tx.send(m.to_owned());
            };
            StateLock::acquire_shared_existing(&p2, &mut notice)
        });
        let msg = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait announces itself while the holder holds");
        assert!(msg.contains("waiting"), "{msg}");
        drop(holder);
        let got = waiter.join().unwrap().unwrap();
        assert!(got.is_some(), "the lock arrives once the holder lets go");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn try_variant_yields_instead_of_waiting() {
        let prefix = std::env::temp_dir().join("cargo-lbin-test-trylock");
        let _ = std::fs::remove_dir_all(&prefix);
        let quiet = |_: &str| {};

        // A writable prefix never needs the privileged preparation path;
        // answering the question creates the same lock file `acquire`
        // would have created a moment later.
        assert!(!StateLock::preparation_needs_privilege(&prefix));

        let held = StateLock::acquire_with(
            &prefix,
            &Mode::Exclusive,
            privileged::Policy {
                sudo: privileged::Sudo::Forbidden,
                screen: privileged::Screen::Inherited,
            },
            &mut { quiet },
        )
        .unwrap();
        // Advisory read while a mutation holds the prefix: the answer is
        // "not now", never a wait — a UI thread is on the other end.
        let advisory = StateLock::try_acquire_with(
            &prefix,
            &Mode::Shared,
            privileged::Policy {
                sudo: privileged::Sudo::Forbidden,
                screen: privileged::Screen::Inherited,
            },
            &mut { quiet },
        )
        .unwrap();
        assert!(advisory.is_none(), "shared try must yield to exclusive");
        drop(held);
        let advisory = StateLock::try_acquire_with(
            &prefix,
            &Mode::Shared,
            privileged::Policy {
                sudo: privileged::Sudo::Forbidden,
                screen: privileged::Screen::Inherited,
            },
            &mut { quiet },
        )
        .unwrap();
        assert!(advisory.is_some(), "free lock acquires without waiting");
        let _ = std::fs::remove_dir_all(&prefix);
    }
}
