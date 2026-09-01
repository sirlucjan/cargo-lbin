//! Minimal crates.io sparse index client.
//!
//! The sparse index is plain HTTPS: one file per crate, JSON-lines, one line
//! per published version. No authentication, no API key. crates.io asks for a
//! meaningful User-Agent, which we provide.

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;

const INDEX_BASE: &str = "https://index.crates.io";
pub(crate) const USER_AGENT: &str = concat!(
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

/// One line of the index: a published version and whether it was yanked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub yanked: bool,
}

/// Fetch all published, non-yanked versions of `name` from the sparse index.
/// An unknown crate is an error here: for `checkupdate` and `update`, a
/// manifest entry the index has never heard of is a problem to report,
/// not a state to describe.
pub fn published_versions(name: &str) -> Result<Vec<Version>> {
    let releases = releases(name)?.ok_or_else(|| not_found(name))?;
    non_yanked(name, releases)
}

/// The error for a name the index does not know, shared so every caller
/// says it the same way.
pub fn not_found(name: &str) -> anyhow::Error {
    anyhow::anyhow!("crate `{name}` not found on crates.io")
}

/// Fetch every release of `name`, yanked ones included. `Ok(None)` means
/// the index has no such crate — an answer, distinct from a failed
/// request — so a caller that asked by name can do something useful
/// with "no", such as suggest what the user might have meant.
pub fn releases(name: &str) -> Result<Option<Vec<Release>>> {
    let url = format!("{INDEX_BASE}/{}", index_path(name));
    let response = match ureq::get(&url).set("User-Agent", USER_AGENT).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(other) => bail!("index request for `{name}` failed: {other}"),
    };
    let body = response
        .into_string()
        .with_context(|| format!("reading index response for `{name}`"))?;
    parse_index_body(name, &body).map(Some)
}

fn parse_index_body(name: &str, body: &str) -> Result<Vec<Release>> {
    let mut releases = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let entry: IndexLine = serde_json::from_str(line)
            .with_context(|| format!("malformed index line for `{name}`"))?;
        let version = Version::parse(&entry.vers)
            .with_context(|| format!("unparsable version `{}` for `{name}`", entry.vers))?;
        releases.push(Release {
            version,
            yanked: entry.yanked,
        });
    }
    Ok(releases)
}

/// The installable subset; a crate with nothing left to install is an error
/// for update purposes even though it exists.
fn non_yanked(name: &str, releases: Vec<Release>) -> Result<Vec<Version>> {
    let versions: Vec<Version> = releases
        .into_iter()
        .filter(|r| !r.yanked)
        .map(|r| r.version)
        .collect();
    if versions.is_empty() {
        bail!("crate `{name}` has no non-yanked versions");
    }
    Ok(versions)
}

/// What `search` shows about a crate's published history.
///
/// This is history, not eligibility: the newest releases are reported
/// whether or not they were yanked, and carry the flag so the reader sees
/// "1.1.0 [yanked]" rather than being told 1.0.0 is the latest — that would
/// hide the yank, which is the single most useful thing to know about it.
/// Whether the *installed* copy can move anywhere is a separate question
/// answered from the non-yanked subset, by the same rules `update` uses.
#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    /// Newest stable release ever published, yanked or not.
    pub latest_stable: Option<Release>,
    /// Newest pre-release, only if it is newer than `latest_stable` —
    /// older pre-releases are history, not news.
    pub latest_pre: Option<Release>,
    /// Every release ever published, yanked ones included; `yanked` is
    /// the subset of these that is no longer installable.
    pub total: usize,
    pub yanked: usize,
}

pub fn summarize(releases: &[Release]) -> Summary {
    let newest = |pre: bool| {
        releases
            .iter()
            .filter(|r| r.version.pre.is_empty() != pre)
            .max_by(|a, b| a.version.cmp(&b.version))
            .cloned()
    };
    let latest_stable = newest(false);
    let latest_pre = newest(true).filter(|pre| {
        latest_stable
            .as_ref()
            .is_none_or(|stable| pre.version > stable.version)
    });
    Summary {
        latest_stable,
        latest_pre,
        total: releases.len(),
        yanked: releases.iter().filter(|r| r.yanked).count(),
    }
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
        let versions = non_yanked("demo", parse_index_body("demo", body).unwrap()).unwrap();
        let current = Version::parse("0.1.0").unwrap();
        let latest = latest_relevant(&versions, &current).unwrap();
        assert_eq!(latest, Version::parse("0.1.5").unwrap());
    }

    #[test]
    fn semver_not_lexicographic() {
        let body = r#"{"vers":"0.9.0","yanked":false}
{"vers":"0.10.0","yanked":false}"#;
        let versions = non_yanked("demo", parse_index_body("demo", body).unwrap()).unwrap();
        let current = Version::parse("0.9.0").unwrap();
        let latest = latest_relevant(&versions, &current).unwrap();
        assert_eq!(latest, Version::parse("0.10.0").unwrap());
    }

    #[test]
    fn prereleases_hidden_from_stable_users() {
        let body = r#"{"vers":"1.0.0","yanked":false}
{"vers":"1.1.0-rc.1","yanked":false}"#;
        let versions = non_yanked("demo", parse_index_body("demo", body).unwrap()).unwrap();
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
        let releases = parse_index_body("demo", body).unwrap();
        assert_eq!(releases.len(), 1);
        assert!(non_yanked("demo", releases).is_err());
    }

    fn rel(v: &str, yanked: bool) -> Release {
        Release {
            version: Version::parse(v).unwrap(),
            yanked,
        }
    }

    #[test]
    fn summary_reports_history_including_yanked() {
        let body = r#"{"vers":"0.9.0","yanked":false}
{"vers":"1.0.0-rc.1","yanked":false}
{"vers":"1.0.0","yanked":false}
{"vers":"1.0.1","yanked":true}
{"vers":"1.1.0-beta.1","yanked":false}
{"vers":"1.1.0-beta.2","yanked":true}"#;
        let summary = summarize(&parse_index_body("demo", body).unwrap());
        // The newest stable is the yanked one, and it is reported as such:
        // hiding it behind 1.0.0 would hide the yank.
        assert_eq!(summary.latest_stable, Some(rel("1.0.1", true)));
        // The rc predates the stable release and is not news; the newest
        // beta is, yanked or not.
        assert_eq!(summary.latest_pre, Some(rel("1.1.0-beta.2", true)));
        assert_eq!((summary.total, summary.yanked), (6, 2));
    }

    #[test]
    fn summary_of_prerelease_only_crate() {
        let body = r#"{"vers":"0.1.0-alpha.1","yanked":false}"#;
        let summary = summarize(&parse_index_body("demo", body).unwrap());
        assert_eq!(summary.latest_stable, None);
        assert_eq!(summary.latest_pre, Some(rel("0.1.0-alpha.1", false)));
    }

    #[test]
    fn summary_of_fully_yanked_crate_still_has_a_latest() {
        // A stable release exists; it is yanked. "(no stable release)"
        // would be false.
        let body = r#"{"vers":"0.1.0","yanked":true}"#;
        let summary = summarize(&parse_index_body("demo", body).unwrap());
        assert_eq!(
            summary,
            Summary {
                latest_stable: Some(rel("0.1.0", true)),
                latest_pre: None,
                total: 1,
                yanked: 1
            }
        );
    }
}
