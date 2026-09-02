//! Warn when a binary about to be installed shares its name with one
//! already on `PATH` outside the prefix — typically a distribution
//! package in `/usr/bin`.
//!
//! This is a warning, never a refusal. The person installing may know
//! exactly what they are doing (a newer version than the distro ships is
//! the usual reason to reach for cargo-lbin at all); what they may not
//! know is where the two copies stand in `PATH`, because that — not
//! freshness — is what decides which one a bare name reaches. So the
//! warning says three things: which existing file has the name, who
//! owns it if a package manager will say, and which of the two
//! directories comes first in `PATH`. Directory order is all it claims.
//! Which file the current user can actually execute — a root-owned
//! `0100`, an ACL, a shell's command hash — is not determined here, and
//! the wording is kept to what is: filesystem says a regular file with
//! some execute bit exists, `PATH` says which location is earlier, the
//! package manager says who owns it. Nothing simulates a shell.
//!
//! Ownership is asked of whichever package manager is present — pacman,
//! rpm, dpkg — by absolute path, never by `PATH` lookup: this tool puts
//! binaries into a directory that usually precedes `/usr/bin`, so a
//! managed crate named `pacman` would otherwise be the thing asked. The
//! file's path is passed as a single argument, no shell involved. Any
//! failure there degrades to "owner unknown"; the shadowing fact itself
//! needs nothing but the filesystem.
//!
//! Everything printed here came from outside — a path from `PATH`, a
//! line from a package manager — and goes to a terminal. The same rule
//! as for crates.io data applies at this boundary: data is data, control
//! characters are replaced before anything reaches stderr.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Where `<prefix>/bin` stands relative to the existing file's
/// directory in `PATH`. Three states, because "the prefix is not
/// earlier" has two distinct causes with two distinct messages: it
/// comes later, or it is not on `PATH` at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// `<prefix>/bin` precedes the existing file's directory.
    PrefixFirst,
    /// The existing file's directory precedes `<prefix>/bin`.
    ExistingFirst,
    /// `<prefix>/bin` is not on `PATH`.
    PrefixAbsent,
}

/// A `PATH` entry, outside the prefix, holding a file named like a
/// binary that is about to be installed.
#[derive(Debug, PartialEq, Eq)]
pub struct Shadow {
    pub bin: String,
    pub existing: PathBuf,
    pub outcome: Outcome,
}

/// A regular file with an execute bit for someone. A plain file of the
/// right name is not a program — `is_file()` alone would stop the scan
/// on a `0644` `rg` in an early `PATH` directory. Whether the *current
/// user* may execute it (a root-owned `0100`, an ACL) is deliberately
/// not checked: that would need `access(2)`, and the warning claims
/// only directory order, which this test is enough to support.
/// Symlinks are followed.
pub fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Anchor a `PATH` entry the way a shell resolves it: an empty entry is
/// the current directory, a relative one is relative to it. Without
/// this, `--prefix local` and a `PATH` containing `$PWD/local/bin` would
/// not recognize each other as the same directory. Normalization is
/// lexical — `.` and `..` folded, separators collapsed — and does not
/// consult the filesystem, so a symlink on the way can still make two
/// spellings of one directory look different; the cost of that is a
/// warning too many or too few, never a wrong action.
fn anchor(entry: PathBuf, cwd: &Path) -> PathBuf {
    let entry = if entry.as_os_str().is_empty() {
        cwd.to_path_buf()
    } else if entry.is_relative() {
        cwd.join(entry)
    } else {
        entry
    };
    let mut out = PathBuf::new();
    for component in entry.components() {
        match component {
            Component::ParentDir => {
                // Above the root there is nothing to pop; `/..` is `/`.
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Scan `PATH` (as given) for files named like `bins`, ignoring
/// `prefix_bin` itself and reporting only the first matching candidate
/// per name in `PATH` order. `cwd` anchors relative entries; `exists`
/// (`is_executable` in production) is injected so the scan is testable
/// without a filesystem.
pub fn find_shadows(
    path_var: &OsStr,
    prefix_bin: &Path,
    bins: &[String],
    cwd: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<Shadow> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path_var)
        .map(|d| anchor(d, cwd))
        .collect();
    let prefix_bin = anchor(prefix_bin.to_path_buf(), cwd);
    let prefix_pos = dirs.iter().position(|d| *d == prefix_bin);
    bins.iter()
        .filter_map(|bin| {
            dirs.iter()
                .enumerate()
                .filter(|(_, d)| **d != prefix_bin)
                .map(|(i, d)| (i, d.join(bin)))
                .find(|(_, candidate)| exists(candidate))
                .map(|(i, existing)| Shadow {
                    bin: bin.clone(),
                    existing,
                    outcome: match prefix_pos {
                        Some(p) if p < i => Outcome::PrefixFirst,
                        Some(_) => Outcome::ExistingFirst,
                        None => Outcome::PrefixAbsent,
                    },
                })
        })
        .collect()
}

/// The package owning `path`, as described by the first package manager
/// present that claims it. First line of the tool's output, trimmed;
/// the exact format is the tool's, quoted rather than parsed.
pub fn owner_of(path: &Path) -> Option<String> {
    // Absolute paths: see the module doc.
    let queries: [(&str, &[&str]); 3] = [
        ("/usr/bin/pacman", &["-Qo"]),
        ("/usr/bin/rpm", &["-qf"]),
        ("/usr/bin/dpkg", &["-S"]),
    ];
    for (tool, args) in queries {
        // Not present (or not runnable): try the next one.
        let Ok(output) = Command::new(tool).args(args).arg(path).output() else {
            continue;
        };
        if !output.status.success() {
            // This tool does not claim the file (or errored). Ask the
            // next one: a stray `/usr/bin/pacman` on a Fedora box must
            // not stop `rpm -qf` from answering. Normal systems still
            // make one query — they have one manager.
            continue;
        }
        let line = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        // Exit 0 with nothing to say is not a claim either; keep asking.
        if !line.is_empty() {
            return Some(line);
        }
    }
    None
}

/// Text from outside the program, made safe for a terminal: control
/// characters replaced with spaces, whitespace collapsed. The same rule
/// `api` applies to crates.io responses; kept local because this module
/// and that one are the two boundaries, and each should be readable on
/// its own.
fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One warning line per shadow, ready for stderr. Every external value
/// — the path from `PATH`, the owner line from the package manager, the
/// binary name from cargo — passes through `terminal_text`.
pub fn describe(shadow: &Shadow, prefix_bin: &Path, owner: Option<&str>) -> String {
    let prefix_bin = terminal_text(&prefix_bin.to_string_lossy());
    let existing_dir = shadow
        .existing
        .parent()
        .map(|d| terminal_text(&d.to_string_lossy()))
        .unwrap_or_default();
    let who = owner.map_or(String::new(), |o| format!(" ({})", terminal_text(o)));
    // A statement about PATH order and nothing else — no verb about
    // running, finding or executing, because none of those is known.
    let verdict = match shadow.outcome {
        Outcome::PrefixFirst => format!("{prefix_bin} precedes {existing_dir} in PATH"),
        Outcome::ExistingFirst => format!("{existing_dir} precedes {prefix_bin} in PATH"),
        Outcome::PrefixAbsent => format!("{prefix_bin} is not on PATH"),
    };
    format!(
        "`{}` already exists as {}{who}; {verdict}",
        terminal_text(&shadow.bin),
        terminal_text(&shadow.existing.to_string_lossy())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bins(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn first_hit_outside_prefix_wins_and_order_decides() {
        let path = OsStr::new("/usr/local/bin:/usr/bin:/bin");
        let prefix_bin = Path::new("/usr/local/bin");
        let exists = |p: &Path| {
            matches!(
                p.to_str().unwrap(),
                "/usr/local/bin/rg" | "/usr/bin/rg" | "/bin/rg" | "/usr/bin/fd"
            )
        };
        let cwd = Path::new("/home/u");
        let found = find_shadows(path, prefix_bin, &bins(&["rg", "fd", "bat"]), cwd, exists);
        assert_eq!(
            found,
            [
                Shadow {
                    bin: "rg".to_owned(),
                    // /usr/local/bin/rg is the prefix's own and ignored;
                    // /usr/bin/rg is the first foreign hit, /bin/rg later.
                    existing: PathBuf::from("/usr/bin/rg"),
                    outcome: Outcome::PrefixFirst,
                },
                Shadow {
                    bin: "fd".to_owned(),
                    existing: PathBuf::from("/usr/bin/fd"),
                    outcome: Outcome::PrefixFirst,
                },
            ]
        );
    }

    #[test]
    fn prefix_later_and_prefix_absent_are_different_outcomes() {
        let exists = |p: &Path| p == Path::new("/usr/bin/rg");
        let cwd = Path::new("/home/u");
        let path = OsStr::new("/usr/bin:/usr/local/bin");
        let found = find_shadows(
            path,
            Path::new("/usr/local/bin"),
            &bins(&["rg"]),
            cwd,
            exists,
        );
        assert_eq!(found[0].outcome, Outcome::ExistingFirst);
        let path = OsStr::new("/usr/bin:/bin");
        let found = find_shadows(
            path,
            Path::new("/opt/tools/bin"),
            &bins(&["rg"]),
            cwd,
            exists,
        );
        assert_eq!(found[0].outcome, Outcome::PrefixAbsent);
    }

    #[test]
    fn path_entries_are_normalized_before_comparison() {
        // A trailing slash or `.` must not make the prefix look foreign
        // to itself, or it would report its own previous install.
        let exists = |p: &Path| p == Path::new("/usr/local/bin/rg");
        let cwd = Path::new("/home/u");
        let path = OsStr::new("/usr/local/bin/:/usr/./local/bin");
        let found = find_shadows(
            path,
            Path::new("/usr/local/bin"),
            &bins(&["rg"]),
            cwd,
            exists,
        );
        assert_eq!(found, Vec::<Shadow>::new());
    }

    #[test]
    fn parent_components_fold_lexically() {
        let cwd = Path::new("/home/u");
        // `/usr/local/../local/bin` is the prefix's own `bin`, not a
        // foreign directory, and certainly not evidence that the prefix
        // is absent from PATH.
        let exists = |p: &Path| p == Path::new("/usr/local/bin/rg");
        let path = OsStr::new("/usr/local/../local/bin:/usr/bin");
        let found = find_shadows(
            path,
            Path::new("/usr/local/bin"),
            &bins(&["rg"]),
            cwd,
            exists,
        );
        assert_eq!(found, Vec::<Shadow>::new());
        // And the fold does not climb above the root.
        assert_eq!(
            anchor(PathBuf::from("/../usr/bin"), cwd),
            PathBuf::from("/usr/bin")
        );
        assert_eq!(
            anchor(PathBuf::from("../x"), Path::new("/a/b")),
            PathBuf::from("/a/x")
        );
    }

    #[test]
    fn relative_and_empty_path_entries_resolve_against_cwd() {
        let cwd = Path::new("/home/u");
        // `--prefix local` with `$PWD/local/bin` on PATH: the same
        // directory, so its own file is not a shadow...
        let exists = |p: &Path| p == Path::new("/home/u/local/bin/rg");
        let path = OsStr::new("/home/u/local/bin:/usr/bin");
        let found = find_shadows(path, Path::new("local/bin"), &bins(&["rg"]), cwd, exists);
        assert_eq!(found, Vec::<Shadow>::new());
        // ...and an empty entry means the current directory, as in a shell.
        let exists = |p: &Path| p == Path::new("/home/u/rg");
        let path = OsStr::new("/usr/bin::/usr/local/bin");
        let found = find_shadows(
            path,
            Path::new("/usr/local/bin"),
            &bins(&["rg"]),
            cwd,
            exists,
        );
        assert_eq!(found[0].existing, PathBuf::from("/home/u/rg"));
        assert_eq!(found[0].outcome, Outcome::ExistingFirst);
    }

    #[test]
    fn description_names_file_owner_and_outcome() {
        let shadow = Shadow {
            bin: "rg".to_owned(),
            existing: PathBuf::from("/usr/bin/rg"),
            outcome: Outcome::PrefixFirst,
        };
        let text = describe(
            &shadow,
            Path::new("/usr/local/bin"),
            Some("/usr/bin/rg is owned by ripgrep 14.1.1-1"),
        );
        assert!(
            text.starts_with(
                "`rg` already exists as /usr/bin/rg (/usr/bin/rg is owned by ripgrep 14.1.1-1);"
            ),
            "{text}"
        );
        assert!(
            text.ends_with("/usr/local/bin precedes /usr/bin in PATH"),
            "{text}"
        );
        let prefix = Path::new("/usr/local/bin");
        let later = Shadow {
            outcome: Outcome::ExistingFirst,
            ..shadow
        };
        let text = describe(&later, prefix, None);
        assert!(!text.contains('('), "{text}");
        assert!(
            text.ends_with("/usr/bin precedes /usr/local/bin in PATH"),
            "{text}"
        );
        let absent = Shadow {
            outcome: Outcome::PrefixAbsent,
            ..later
        };
        let text = describe(&absent, prefix, None);
        assert!(text.ends_with("/usr/local/bin is not on PATH"), "{text}");
        // No claim about execution anywhere in the wording.
        for verb in ["run", "found", "execut"] {
            assert!(!text.contains(verb), "{verb}: {text}");
        }
    }

    #[test]
    fn external_text_never_carries_terminal_controls() {
        // A PATH entry and a package-manager line with an escape
        // sequence, a bell and a carriage return: all become plain
        // spaced text before reaching stderr.
        let shadow = Shadow {
            bin: "rg".to_owned(),
            existing: PathBuf::from("/opt/\u{1b}[2Jevil/bin/rg"),
            outcome: Outcome::PrefixFirst,
        };
        let text = describe(
            &shadow,
            Path::new("/usr/local/bin"),
            Some("owned by\u{7} ripgrep\r\n14.1.1"),
        );
        assert!(!text.chars().any(char::is_control), "{text}");
        assert!(text.contains("/opt/ [2Jevil/bin/rg"), "{text}");
        assert!(text.contains("(owned by ripgrep 14.1.1)"), "{text}");
    }
}
