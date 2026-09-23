// SPDX-FileCopyrightText: Piotr Gorski <piotrgorski@cachyos.org>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pasteable command primitives and shared hint construction.
//!
//! Commands lbin invites a human to paste follow one doctrine from
//! 0.15.0: scope is carried explicitly as `--prefix=…`, never
//! reconstructed as a shorthand like `--user`. Paths are shell-quoted
//! without laundering their meaning; when no honest spelling exists,
//! the hint names the operation instead of offering a command that is
//! safe to paste and wrong to run.

use crate::manifest::Entry;
use std::path::Path;

/// POSIX single-quote shell quoting for the one command lbin invites a
/// human to paste: bare when boring, quoted otherwise, `'\''` for the
/// embedded quote. A hint the docs call pasteable must not be the one
/// thing in the output unsafe to paste.
fn shell_quote(s: &str) -> String {
    let boring = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | ',' | '=' | '@' | '%')
        });
    if boring {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The pasteable spelling of one path-valued flag, or `None` when no
/// honest one exists: non-UTF-8 `display()` is lossy, and a control char
/// would be laundered by the sanitize boundary into a command naming a
/// different path — safe to paste, wrong to run. `<flag>=<quoted>`
/// so a dash-leading path cannot lex as an option.
pub(crate) fn pasteable_path_arg(flag: &str, path: &Path) -> Option<String> {
    let s = path.to_str()?;
    if s.chars().any(char::is_control) {
        return None;
    }
    Some(format!("{flag}={}", shell_quote(s)))
}

/// [`pasteable_path_arg`] for the flag every hint so far has needed.
pub(crate) fn pasteable_prefix(prefix: &Path) -> Option<String> {
    pasteable_path_arg("--prefix", prefix)
}

/// The parenthetical an "it is pinned" refusal carries: a pasteable
/// `unpin` command scoped to the prefix the operation runs against —
/// `--user` and the default alike spell out as `--prefix=…`, which is
/// what they resolve to, so the hint stays true wherever it was
/// copied from. When the prefix has no honest spelling, the hint
/// names the verb instead of offering a command that is safe to
/// paste and wrong to run.
pub(crate) fn unpin_hint(prefix: &Path, names: &str) -> String {
    match pasteable_prefix(prefix) {
        Some(arg) => format!("(run `cargo lbin unpin {names} {arg}` first)"),
        None => "(unpin first)".to_owned(),
    }
}

/// The repair a disk finding may name — `None` when the prefix has no
/// safe spelling.
///
/// `--reinstall` is the exact shape a repair wants: rebuild *this*
/// installation, whatever it is. Spelling the entry out instead — a
/// version for a pinned crate, `--locked` when it carries it, nothing
/// for an unpinned one — repaired an unpinned crate by moving it to
/// the newest release, which is a version change wearing a repair's
/// clothes. The flag reads the entry the same way verify just did.
/// Only the prefix needs quoting: the name already passed validation —
/// a shell-inert alphabet.
pub(crate) fn reinstall_hint(prefix: &Path, name: &str, _entry: &Entry) -> Option<String> {
    let prefix_arg = pasteable_prefix(prefix)?;
    Some(format!(
        "cargo lbin install --reinstall {name} {prefix_arg}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasteable_prefix_refuses_what_it_cannot_spell() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            pasteable_prefix(Path::new("/usr/local")).as_deref(),
            Some("--prefix=/usr/local")
        );
        // The = spelling keeps a dash-leading relative prefix from being
        // lexable as another option.
        assert_eq!(
            pasteable_prefix(Path::new("--weird")).as_deref(),
            Some("--prefix=--weird")
        );
        assert_eq!(
            pasteable_prefix(Path::new("/tmp/my lbin")).as_deref(),
            Some("--prefix='/tmp/my lbin'")
        );
        assert_eq!(pasteable_prefix(Path::new("/tmp/a\nb")), None);
        let non_utf8 = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(
            pasteable_prefix(non_utf8),
            None,
            "display() is lossy here; a lossy command is not a true one"
        );
    }

    #[test]
    fn shell_quote_leaves_boring_paths_bare() {
        assert_eq!(shell_quote("/usr/local"), "/usr/local");
        assert_eq!(shell_quote("/tmp/my lbin"), "'/tmp/my lbin'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote(""), "''");
    }
}
