//! Minimal crates.io sparse index client.
//!
//! The sparse index is plain HTTPS: one file per crate, JSON-lines, one line
//! per published version. No authentication, no API key. crates.io asks for a
//! meaningful User-Agent, which we provide.

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;

const INDEX_BASE: &str = "https://index.crates.io";
const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (cargo-install wrapper)"
);

#[derive(Deserialize)]
struct IndexLine {
    vers: String,
    #[serde(default)]
    yanked: bool,
}

/// Relative path of a crate's file in the sparse index.
///
/// Scheme (per the crates.io index RFC): 1- and 2-character names live under
/// `1/` and `2/`, 3-character names under `3/<first char>/`, everything else
/// under `<first two>/<next two>/`. Paths are lowercase.
///
/// Callers pass names validated to be ASCII, so the slices below always hit
/// character boundaries. Belt and suspenders: `get()` instead of indexing,
/// so even a layering violation with a multibyte name degrades to a path
/// that 404s cleanly rather than panicking mid-slice.
pub fn index_path(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.len() {
        0 => String::new(), // caller validates; unreachable in practice
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => match lower.get(..1) {
            Some(first) => format!("3/{first}/{lower}"),
            None => lower,
        },
        _ => match (lower.get(..2), lower.get(2..4)) {
            (Some(a), Some(b)) => format!("{a}/{b}/{lower}"),
            _ => lower,
        },
    }
}

/// Fetch all published, non-yanked versions of `name` from the sparse index.
pub fn published_versions(name: &str) -> Result<Vec<Version>> {
    let url = format!("{INDEX_BASE}/{}", index_path(name));
    let response = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => {
                anyhow::anyhow!("crate `{name}` not found on crates.io")
            }
            other => anyhow::anyhow!("index request for `{name}` failed: {other}"),
        })?;
    let body = response
        .into_string()
        .with_context(|| format!("reading index response for `{name}`"))?;
    parse_index_body(name, &body)
}

fn parse_index_body(name: &str, body: &str) -> Result<Vec<Version>> {
    let mut versions = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let entry: IndexLine = serde_json::from_str(line)
            .with_context(|| format!("malformed index line for `{name}`"))?;
        if entry.yanked {
            continue;
        }
        let version = Version::parse(&entry.vers)
            .with_context(|| format!("unparsable version `{}` for `{name}`", entry.vers))?;
        versions.push(version);
    }
    if versions.is_empty() {
        bail!("crate `{name}` has no non-yanked versions");
    }
    Ok(versions)
}

/// Pick the newest version relevant to someone currently on `current`.
///
/// Pre-releases are only considered if the installed version is itself a
/// pre-release; otherwise the newest stable version wins.
pub fn latest_relevant(versions: &[Version], current: &Version) -> Option<Version> {
    let allow_pre = !current.pre.is_empty();
    versions
        .iter()
        .filter(|v| allow_pre || v.pre.is_empty())
        .max()
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_path_matches_rfc_scheme() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("scxtui"), "sc/xt/scxtui");
        assert_eq!(index_path("ripgrep"), "ri/pg/ripgrep");
        // The index is lowercase even for crates published with capitals.
        assert_eq!(index_path("Inflector"), "in/fl/inflector");
    }

    #[test]
    fn multibyte_names_never_panic() {
        // Validation upstream guarantees ASCII, but a layering violation
        // must degrade to a clean 404 path, not a byte-boundary panic.
        // 'ż' is 2 bytes (boundary at 2 happens to hold), '€' is 3 bytes
        // (boundary at 2 does not).
        let _ = index_path("żółć");
        let _ = index_path("€uro");
        let _ = index_path("€ab");
    }

    #[test]
    fn yanked_versions_are_skipped() {
        let body = r#"{"vers":"0.1.0","yanked":false}
{"vers":"0.2.0","yanked":true}
{"vers":"0.1.5","yanked":false}"#;
        let versions = parse_index_body("demo", body).unwrap();
        let current = Version::parse("0.1.0").unwrap();
        let latest = latest_relevant(&versions, &current).unwrap();
        assert_eq!(latest, Version::parse("0.1.5").unwrap());
    }

    #[test]
    fn semver_not_lexicographic() {
        let body = r#"{"vers":"0.9.0","yanked":false}
{"vers":"0.10.0","yanked":false}"#;
        let versions = parse_index_body("demo", body).unwrap();
        let current = Version::parse("0.9.0").unwrap();
        let latest = latest_relevant(&versions, &current).unwrap();
        assert_eq!(latest, Version::parse("0.10.0").unwrap());
    }

    #[test]
    fn prereleases_hidden_from_stable_users() {
        let body = r#"{"vers":"1.0.0","yanked":false}
{"vers":"1.1.0-rc.1","yanked":false}"#;
        let versions = parse_index_body("demo", body).unwrap();
        let stable = Version::parse("1.0.0").unwrap();
        assert_eq!(latest_relevant(&versions, &stable).unwrap(), stable);

        let pre = Version::parse("1.0.0-beta.2").unwrap();
        assert_eq!(
            latest_relevant(&versions, &pre).unwrap(),
            Version::parse("1.1.0-rc.1").unwrap()
        );
    }

    #[test]
    fn all_versions_yanked_is_an_error() {
        let body = r#"{"vers":"0.1.0","yanked":true}"#;
        assert!(parse_index_body("demo", body).is_err());
    }
}
