//! Cross-prefix awareness: which *other* lbin prefixes carry a crate
//! installed here. The set is finite and closed (`/usr/local`,
//! `~/.local`) and only lbin's own manifests are consulted — a
//! reminder, not a PATH scanner.
//!
//! Foreign manifests are read WITHOUT any lock, deliberately: atomic
//! rename means a lockless read is a complete old or new document,
//! never torn — and a shared flock on a foreign prefix could park this
//! process behind someone's ten-minute build. An annotation must never
//! wait; the worst case is one rename out of date.

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

/// The closed set minus the current prefix. Comparison is lexical:
/// both candidates are canonical by construction, and a symlinked
/// duplicate is cosmetic, not worth a filesystem round-trip.
pub fn known_others(current: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/usr/local")];
    #[allow(deprecated)] // un-deprecated in std; attribute for older toolchain docs
    if let Some(home) = std::env::home_dir() {
        candidates.push(home.join(".local"));
    }
    candidates.retain(|c| c != current);
    candidates
}

/// For every crate, the other prefixes carrying it. Missing manifests
/// contribute nothing; unreadable or corrupt ones are skipped — a
/// listing must not fail over a prefix it was not asked about.
pub fn also_installed(current: &Path) -> BTreeMap<String, Vec<AlsoIn>> {
    also_installed_from(known_others(current))
}

/// The loader behind `also_installed`, over any prefix set — split out
/// so tests exercise the real function on temp prefixes, not a twin
/// that stays green while the original rots.
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
    // The human form goes to a terminal/Span and the prefix comes from
    // the environment — the 0.7.0 rule applies. JSON stays untouched: the
    // serializer escapes, and a consumer deserves the true path.
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
        assert!(
            !map.contains_key("local-only"),
            "the current prefix is not 'also'"
        );
        let s = describe_for(&map, "elsewhere");
        assert!(s.contains("[also in") && s.contains("@2.3.4"), "{s}");
        assert_eq!(describe_for(&map, "local-only"), "");

        // The closed set itself: current is excluded, the list is finite.
        let others = known_others(Path::new("/usr/local"));
        assert!(others.iter().all(|p| p != Path::new("/usr/local")));

        let _ = std::fs::remove_dir_all(&root);
    }
}
