//! Prefix-scoped state lock.
//!
//! Serializes cargo-lbin instances operating on the same prefix: mutations
//! (install/update/remove) take an exclusive lock for their whole duration,
//! builds included; readers (list/checkupdate) take a shared lock. The lock
//! lives next to the state (`<prefix>/share/cargo-lbin/lock`), so it protects the
//! prefix rather than a particular user — two different users driving
//! `/usr/local` contend on the same file.
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
    /// Acquire the prefix lock, blocking if another instance holds it (with
    /// a notice, so a wait during someone else's 10-minute build is not
    /// mistaken for a hang).
    pub fn acquire(prefix: &Path, mode: &Mode) -> Result<Self> {
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
                // lock file with escalation, then it must open.
                privileged::ensure_lock_file(privileged::Escalation::for_prefix(prefix), &path)
                    .with_context(|| format!("preparing state lock {}", path.display()))?;
                OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .with_context(|| format!("opening state lock {}", path.display()))?
            }
            (Err(_), Mode::Shared) => {
                // A reader on a prefix that never saw a mutation; nothing
                // to protect yet and no reason to demand sudo for a `list`.
                eprintln!(
                    "warning: cannot open {} — proceeding without a state lock \
                     (the file is created by the first install/update/remove)",
                    path.display()
                );
                return Ok(Self { _file: None });
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
            Err(TryLockError::WouldBlock) => {
                eprintln!("another cargo-lbin instance holds the state lock; waiting...");
                match mode {
                    Mode::Shared => file.lock_shared(),
                    Mode::Exclusive => file.lock(),
                }
                .context("acquiring state lock")?;
            }
            Err(TryLockError::Error(e)) => return Err(e).context("acquiring state lock"),
        }
        Ok(Self { _file: Some(file) })
    }
}
