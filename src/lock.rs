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
                     (the file is created by the first install/update/remove)",
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
