//! Pre-build install warnings for cross-prefix duplicates.
//!
//! Double installation is legal, so this is warning-only. A warning is
//! emitted when an install would create another managed copy under the
//! target prefix, before the first build has begun.

use crate::hints::pasteable_path_arg;
use crate::manifest::Manifest;
use crate::{prefixes, text};
use std::path::{Path, PathBuf};

/// Pre-build warnings for an install batch about to create a second
/// cross-prefix copy: one block per foreign managed copy of a
/// requested crate that is absent from this prefix's manifest.
/// Absent-here on purpose — the plan's word is "before creating the
/// second copy": a reinstall of a crate both sides already carry
/// creates nothing, and verify already names the standing duplication.
/// A warning and never an error: double installation is legal, and
/// migrate is named — as a genuinely pasteable command when both
/// prefixes have an honest shell spelling, and not at all otherwise —
/// for the person who meant to move, not copy. Emitted before the
/// first build, so the whole batch can still be abandoned before any
/// minutes are invested.
pub(crate) fn duplicate_install_warnings<'a>(
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    duplicate_install_warnings_from(&prefixes::also_installed(prefix), prefix, manifest, names)
}

/// The builder behind `duplicate_install_warnings`, over any
/// cross-prefix map — split out so tests exercise the real message
/// construction (prefixes' own tests already cover the map's loading).
/// Lines, not blocks: the first carries the severity word, the
/// continuations do not, and every line is sanitized — both prefixes
/// are environment-borne, the 0.7.0 rule applies.
fn duplicate_install_warnings_from<'a>(
    also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    duplicate_install_warning_lines(
        &cross_prefix_duplicates_from(also, prefix, manifest, names),
        prefix,
    )
}

/// One foreign managed copy that an install batch is about to
/// duplicate — the *decision* as facts, shared by every surface. The
/// CLI renders these into warning lines; the TUI feeds the same lines
/// into its panel today and may render the fields natively tomorrow —
/// either way the answer to "will this install create a duplicate?"
/// has exactly one author, and no surface parses a string built for
/// another.
struct CrossPrefixDuplicate {
    name: String,
    other_prefix: PathBuf,
    other_version: String,
    /// The pasteable migrate command, present only when both prefixes
    /// have an honest shell spelling (see `pasteable_path_arg`); the
    /// spelling question is decided here, with the facts, so no
    /// renderer can disagree about it.
    migrate_hint: Option<String>,
}

/// The decision behind `duplicate_install_warnings`, over any
/// cross-prefix map: which requested crates, absent from this prefix's
/// manifest, are managed elsewhere — one entry per foreign copy.
fn cross_prefix_duplicates_from<'a>(
    also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
    prefix: &Path,
    manifest: &Manifest,
    names: impl Iterator<Item = &'a str>,
) -> Vec<CrossPrefixDuplicate> {
    let mut duplicates = Vec::new();
    for name in names {
        if manifest.crates.contains_key(name) {
            continue;
        }
        let Some(entries) = also.get(name) else {
            continue;
        };
        for other in entries {
            let migrate_hint = match (
                pasteable_path_arg("--prefix", &other.prefix),
                pasteable_path_arg("--to", prefix),
            ) {
                (Some(source), Some(dest)) => {
                    Some(format!("cargo lbin migrate {name} {source} {dest}"))
                }
                _ => None,
            };
            duplicates.push(CrossPrefixDuplicate {
                name: name.to_owned(),
                other_prefix: other.prefix.clone(),
                other_version: other.version.clone(),
                migrate_hint,
            });
        }
    }
    duplicates
}

/// The CLI's words over the shared facts: three lines per duplicate,
/// every one sanitized for the terminal. The migrate line is printed
/// exactly when the facts carry an honest command — the sanitize
/// boundary protects the terminal, not the shell; a laundered path
/// would be safe to paste and wrong to run, and quoting is what keeps
/// `/tmp/$(touch owned)` a directory name instead of a command. With
/// no honest spelling the mechanism is still named, worded so nobody
/// mistakes it for a pasteable hint. Same rule as the verify reinstall
/// hint.
fn duplicate_install_warning_lines(
    duplicates: &[CrossPrefixDuplicate],
    prefix: &Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    for dup in duplicates {
        lines.push(text::sanitize(&format!(
            "warning: `{}` is already managed under {} @{}",
            dup.name,
            dup.other_prefix.display(),
            dup.other_version
        )));
        lines.push(text::sanitize(&format!(
            "this will install another copy under {}",
            prefix.display()
        )));
        match &dup.migrate_hint {
            Some(hint) => {
                lines.push(text::sanitize(&format!(
                    "use `{hint}` if you intended to move it"
                )));
            }
            None => lines.push(text::sanitize(
                "use `cargo lbin migrate` with explicit --prefix/--to \
                 if you intended to move it",
            )),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::manifest_with;

    /// The duplicate-install warning fires exactly when this install
    /// would create the second copy: the crate is absent here and
    /// managed over there. Both prefixes are environment-borne, so the
    /// lines are sanitized; the migrate hint is pasteable as printed.
    #[test]
    fn duplicate_install_warns_only_before_the_second_copy() {
        use std::collections::BTreeMap;
        let prefix = PathBuf::from("/home/u/.local");
        let mut also: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        also.insert(
            "foo".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "1.2.3".to_owned(),
            }],
        );

        // Absent here, managed there: the plan's three lines, verbatim
        // in shape, with a genuinely pasteable migrate hint —
        // flag=value form, like every other pasteable hint.
        let empty = Manifest::default();
        let lines = duplicate_install_warnings_from(&also, &prefix, &empty, std::iter::once("foo"));
        assert_eq!(
            lines,
            [
                "warning: `foo` is already managed under /usr/local @1.2.3",
                "this will install another copy under /home/u/.local",
                "use `cargo lbin migrate foo --prefix=/usr/local --to=/home/u/.local` \
                 if you intended to move it",
            ],
            "{lines:?}"
        );

        // Pasteable means shell-safe, not merely terminal-safe: a space
        // stays one argument, an apostrophe survives its own quoting,
        // and $() stays a directory name instead of a command.
        let spaced = PathBuf::from("/tmp/my lbin");
        let lines = duplicate_install_warnings_from(&also, &spaced, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains("--to='/tmp/my lbin'"),
            "a space is quoted into one argument: {lines:?}"
        );
        let hostile_shell = PathBuf::from("/tmp/$(touch owned)");
        let lines =
            duplicate_install_warnings_from(&also, &hostile_shell, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains("--to='/tmp/$(touch owned)'"),
            "command substitution is neutralized by quoting: {lines:?}"
        );
        let quoted = PathBuf::from("/tmp/o'brien");
        let lines = duplicate_install_warnings_from(&also, &quoted, &empty, std::iter::once("foo"));
        assert!(
            lines[2].contains(r"--to='/tmp/o'\''brien'"),
            "an apostrophe survives its own quoting: {lines:?}"
        );

        // Already installed here too: nothing new is created, verify
        // owns the standing duplication — no warning.
        let local = manifest_with(&["foo"]);
        assert!(
            duplicate_install_warnings_from(&also, &prefix, &local, std::iter::once("foo"))
                .is_empty(),
            "a reinstall creates no second copy"
        );

        // No foreign copy: silence.
        assert_eq!(
            duplicate_install_warnings_from(&also, &prefix, &empty, std::iter::once("bar")),
            Vec::<String>::new(),
            "nothing managed elsewhere, nothing to warn about"
        );

        // A control character has no honest shell spelling: the warning
        // stands, the terminal stays protected (the 0.7.0 rule), but no
        // exact command is printed — a sanitize-laundered path would be
        // safe to paste and wrong to run. Same rule as the verify
        // reinstall hint.
        let mut hostile: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        hostile.insert(
            "foo".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/\x1b[31mlocal"),
                version: "1.2.3".to_owned(),
            }],
        );
        let lines =
            duplicate_install_warnings_from(&hostile, &prefix, &empty, std::iter::once("foo"));
        assert_eq!(lines.len(), 3, "the warning itself stands: {lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\x1b')), "{lines:?}");
        assert_eq!(
            lines[2],
            "use `cargo lbin migrate` with explicit --prefix/--to \
             if you intended to move it",
            "no honest spelling: the mechanism is named, worded so nobody \
             mistakes it for a pasteable hint: {lines:?}"
        );
    }

    /// The decision matrix from the Phase V plan, at the facts level —
    /// the single authority both surfaces render from, so parity is by
    /// construction and this matrix is the parity test. The renderer's
    /// own shape (exactly three lines per fact, order preserved) is
    /// pinned alongside: a multi-crate batch must not lose a duplicate
    /// between deciding and wording.
    #[test]
    fn cross_prefix_duplicate_decision_matrix() {
        use std::collections::BTreeMap;
        let here = PathBuf::from("/tmp/custom prefix");
        let mut also: BTreeMap<String, Vec<prefixes::AlsoIn>> = BTreeMap::new();
        // `both` is managed under BOTH known others — legal exactly when
        // the current prefix is a custom one — and `there` under one.
        also.insert(
            "both".to_owned(),
            vec![
                prefixes::AlsoIn {
                    prefix: PathBuf::from("/usr/local"),
                    version: "1.0.0".to_owned(),
                },
                prefixes::AlsoIn {
                    prefix: PathBuf::from("/home/u/.local"),
                    version: "1.1.0".to_owned(),
                },
            ],
        );
        also.insert(
            "there".to_owned(),
            vec![prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "2.0.0".to_owned(),
            }],
        );
        let local = manifest_with(&["here-only", "there"]);

        // Only local: no duplicate to create.
        let d = cross_prefix_duplicates_from(&also, &here, &local, std::iter::once("here-only"));
        assert!(d.is_empty(), "local-only never warns");
        // Local AND foreign: the duplication stands already — silence.
        let d = cross_prefix_duplicates_from(&also, &here, &local, std::iter::once("there"));
        assert!(
            d.is_empty(),
            "a standing duplicate is verify's, not install's"
        );
        // Foreign only: one entry, with an honest quoted hint — the
        // custom prefix needs its space quoted.
        let empty = Manifest::default();
        let d = cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("there"));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].other_version, "2.0.0");
        assert_eq!(
            d[0].migrate_hint.as_deref(),
            Some("cargo lbin migrate there --prefix=/usr/local --to='/tmp/custom prefix'"),
            "{:?}",
            d[0].migrate_hint
        );
        // Managed under both known others: one block per foreign copy —
        // the docs' promise, pinned.
        let d = cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("both"));
        assert_eq!(d.len(), 2, "one entry per foreign managed copy");
        assert_eq!(d[0].other_version, "1.0.0");
        assert_eq!(d[1].other_version, "1.1.0");
        // Unknown everywhere: silence.
        assert!(
            cross_prefix_duplicates_from(&also, &here, &empty, std::iter::once("nowhere"))
                .is_empty()
        );

        // The renderer: three lines per fact, order preserved — the
        // multi-copy batch loses nothing between deciding and wording.
        let lines = duplicate_install_warning_lines(&d_all(&also, &here, &empty), &here);
        assert_eq!(lines.len(), 3 * 3, "{lines:?}");
        assert!(lines[0].contains("`both`") && lines[3].contains("`both`"));
        assert!(lines[6].contains("`there`"));
    }

    /// The matrix's batch, in input order: both requested names.
    fn d_all(
        also: &std::collections::BTreeMap<String, Vec<prefixes::AlsoIn>>,
        here: &Path,
        manifest: &Manifest,
    ) -> Vec<CrossPrefixDuplicate> {
        cross_prefix_duplicates_from(also, here, manifest, ["both", "there"].into_iter())
    }
}
