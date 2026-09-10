//! Cross-prefix awareness: which *other* lbin-managed prefixes carry a
//! crate that is installed here. The list of prefixes is finite and
//! closed — the canonical `/usr/local` and the user's `~/.local` — and
//! only lbin's own manifests are consulted: this is not a PATH scanner
//! (the shadow module owns that question) but a "you also installed
//! this over there" reminder, and only lbin state can answer it.
//!
//! Foreign manifests are read WITHOUT any lock, deliberately. The
//! manifest is placed by atomic rename (`install_atomic`), so a
//! lockless read yields a complete old document or a complete new one,
//! never a torn one — and taking even a shared flock on a foreign
//! prefix could park this process behind someone's ten-minute exclusive
//! build there. An annotation must never wait; the worst a lockless
//! read can be is one rename out of date, which is exactly as stale as
//! any listing is the moment it is printed.

use crate::manifest::Manifest;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One foreign installation of a crate: where, and at what version —
/// the version matters, because skew between prefixes is precisely what
/// the person wants to notice.
pub struct AlsoIn {
    pub prefix: PathBuf,
    pub version: String,
}

/// The closed set of prefixes lbin knows about, minus the current one.
/// Comparison is lexical on the resolved paths: both candidates are
/// absolute and canonical by construction, and the current prefix is
/// what the person actually addressed — a symlinked spelling of the
/// same place showing up as "also in" is a cosmetic duplicate, not a
/// correctness problem worth a filesystem round-trip per candidate.
pub fn known_others(current: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/usr/local")];
    #[allow(deprecated)] // un-deprecated in std; attribute for older toolchain docs
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".local"));
    }
    candidates.retain(|c| c != current);
    candidates
}

/// For every crate name, the other prefixes that carry it. Prefixes
/// whose manifest does not exist contribute nothing (most systems use
/// one prefix, and silence is the right answer); an unreadable or
/// corrupt foreign manifest is skipped too — a listing must not fail
/// over a prefix it was not even asked about, and the authoritative
/// error will greet the person the moment they address that prefix
/// directly.
pub fn also_installed(current: &Path) -> BTreeMap<String, Vec<AlsoIn>> {
    also_installed_from(known_others(current))
}

/// The loader behind `also_installed`, over any set of prefixes — split
/// out so a test can feed it a temp prefix the closed set will never
/// contain and exercise the real function, not a hand-copied twin of
/// it that stays green while the original rots.
fn also_installed_from(others: impl IntoIterator<Item = PathBuf>) -> BTreeMap<String, Vec<AlsoIn>> {
    let mut map: BTreeMap<String, Vec<AlsoIn>> = BTreeMap::new();
    for other in others {
        let Ok(manifest) = Manifest::load(&other) else {
            continue;
        };
        for (name, entry) in &manifest.crates {
            map.entry(name.clone()).or_default().push(AlsoIn {
                prefix: other.clone(),
                version: entry.version.clone(),
            });
        }
    }
    map
}

/// The annotation both the CLI listing and the TUI append after a
/// crate's flags: ` [also in /usr/local @1.2.3]`. One formatter, so the
/// two surfaces cannot drift.
pub fn describe(entries: &[AlsoIn]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for a in entries {
        let _ = write!(out, " [also in {} @{}]", a.prefix.display(), a.version);
    }
    // The human form goes to the terminal and to a Span, and the prefix
    // comes ultimately from the environment — a pathname may hold ESC as
    // legally as `a`, so the 0.7.0 rule applies here like everywhere.
    // The JSON side stays untouched on purpose: the serializer escapes,
    // and a consumer deserves the true path.
    crate::text::sanitize(&out)
}

/// `describe` looked up by name; the common call shape.
pub fn describe_for(map: &BTreeMap<String, Vec<AlsoIn>>, name: &str) -> String {
    map.get(name).map(|v| describe(v)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(prefix: &Path, name: &str, version: &str) {
        let dir = prefix.join("share/cargo-lbin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"version":1,"crates":{{"{name}":{{"version":"{version}","bins":["{name}"],"locked":false,"pinned":false}}}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn sees_the_other_prefix_and_never_itself() {
        let root = std::env::temp_dir().join("cargo-lbin-test-prefixes");
        let _ = std::fs::remove_dir_all(&root);
        let here = root.join("here");
        let there = root.join("there");
        write_manifest(&here, "local-only", "1.0.0");
        write_manifest(&there, "elsewhere", "2.3.4");

        // The closed set does not know these tmp prefixes; the split
        // loader takes the set as input, so the test exercises the real
        // function.
        let map = also_installed_from([there.clone()]);
        assert!(map.contains_key("elsewhere"));
        assert!(!map.contains_key("local-only"), "the current prefix is not 'also'");
        let s = describe_for(&map, "elsewhere");
        assert!(s.contains("[also in") && s.contains("@2.3.4"), "{s}");
        assert!(describe_for(&map, "local-only").is_empty());

        // The closed set itself: current is excluded, the list is finite.
        let others = known_others(Path::new("/usr/local"));
        assert!(others.iter().all(|p| p != Path::new("/usr/local")));

        let _ = std::fs::remove_dir_all(&root);
    }
}
