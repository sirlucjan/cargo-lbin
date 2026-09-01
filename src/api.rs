//! Minimal crates.io *API* client — the search endpoint only.
//!
//! The sparse index (see `index`) is one file per crate under a
//! deterministic path; it can answer "what versions does X have" but not
//! "what is there like X". That second question is the API's, and this
//! module asks it, for `search` alone. Everything version-related stays
//! with the index: what the API reports here is a preview for choosing a
//! name, and `info` is where a name gets looked at properly.
//!
//! crates.io asks API clients for a meaningful User-Agent (shared with the
//! index client) and at most one request per second. The second rule is
//! enforced here, not left to callers: the CLI makes one request per
//! process, but the TUI can issue several searches in one session as
//! fast as they are typed, and the policy belongs to the client of the
//! service, not to everyone who happens to call it. A request that would
//! come too soon after the previous one waits for the remainder of the
//! second first — on the calling thread, which in the TUI is the worker.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::index::USER_AGENT;
use crate::validate::validate_name;

const API_BASE: &str = "https://crates.io/api/v1/crates";

/// crates.io's published request rate for API clients.
const API_INTERVAL: Duration = Duration::from_secs(1);

/// When the last API request left this process; `None` before the first.
static LAST_REQUEST: Mutex<Option<Instant>> = Mutex::new(None);

/// How long a request starting at `now` must wait to honour the interval
/// after the previous one. Pure: it answers the question and records
/// nothing, because what gets recorded must be when the request actually
/// left, and only the caller knows that.
fn wait_for_slot(last: Option<Instant>, now: Instant) -> Duration {
    last.map_or(Duration::ZERO, |prev| {
        API_INTERVAL.saturating_sub(now.saturating_duration_since(prev))
    })
}

/// Blocks until this process may send its next API request. The lock is
/// held across the sleep so a second caller queues behind the first
/// rather than computing its own wait from a stale timestamp. The
/// timestamp is taken *after* the sleep: recording the planned wake-up
/// instead would let a late wake-up (the scheduler promises nothing)
/// shorten the gap to the request after it.
fn throttle() {
    let mut last = LAST_REQUEST.lock().unwrap_or_else(PoisonError::into_inner);
    let wait = wait_for_slot(*last, Instant::now());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    *last = Some(Instant::now());
}

/// One search hit, reduced to what a person choosing a name needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    /// Newest stable version if there is one, else what crates.io would
    /// display, else newest of any kind — the same preference `info`
    /// shows, so the two agree at a glance. `?` if the response carried
    /// no usable version at all.
    pub version: String,
    pub description: String,
}

#[derive(Deserialize)]
struct SearchBody {
    crates: Vec<CrateEntry>,
}

/// Only `name` is required of the response. crates.io marks
/// `max_version` and `max_stable_version` deprecated and has added
/// `default_version`; any of the three may disappear or arrive `null`,
/// and a display-only field must not be able to take `search` down.
#[derive(Deserialize)]
struct CrateEntry {
    name: String,
    #[serde(default)]
    max_stable_version: Option<String>,
    #[serde(default)]
    default_version: Option<String>,
    #[serde(default)]
    max_version: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

impl CrateEntry {
    /// Newest stable if reported, else the version crates.io itself
    /// would display, else the legacy "newest of any kind", else a
    /// visible placeholder — never a failed parse.
    fn shown_version(&self) -> String {
        [
            &self.max_stable_version,
            &self.default_version,
            &self.max_version,
        ]
        .into_iter()
        .flatten()
        .map(|v| sanitize_text(v))
        .find(|v| !v.is_empty())
        .unwrap_or_else(|| "?".to_owned())
    }
}

/// Ask crates.io for crates matching `query`, at most `limit` of them, in
/// the relevance order crates.io chose. "At most" is enforced locally:
/// `per_page` is a request, and a caller sizing a panel or numbering
/// picks needs a bound the server cannot exceed.
pub fn search(query: &str, limit: usize) -> Result<Vec<Hit>> {
    if query.trim().is_empty() {
        bail!("empty search query");
    }
    throttle();
    let response = ureq::get(API_BASE)
        .set("User-Agent", USER_AGENT)
        .query("q", query)
        .query("per_page", &limit.to_string())
        .call()
        .map_err(|e| anyhow::anyhow!("crates.io search for `{query}` failed: {e}"))?;
    let body = response
        .into_string()
        .context("reading crates.io search response")?;
    let mut hits = parse_search_body(&body)?;
    hits.truncate(limit);
    Ok(hits)
}

/// Text from crates.io is data, never terminal control. Anything the
/// response contains ends up on a terminal — printed by the CLI, or
/// handed to a `Span` by the TUI — so control characters (ESC, BEL,
/// newlines, tabs) are replaced here, at the boundary, and whitespace is
/// collapsed to single spaces. Applied to every string field: a name is
/// later typed into the install line and a version is displayed, and
/// neither should be trusted more than a description just because it is
/// usually well-formed.
fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitizing protects the terminal; it does not make a string a crate
/// name. A hit's name is later typed into the TUI's install line on a
/// digit pick, where `foo --locked` would parse as a name and a flag, so
/// every name is held to the same rule as a name the user typed. The
/// API should never send such a thing — and the manifest should never
/// contain one either, which has not stopped `manifest` from checking.
fn parse_search_body(body: &str) -> Result<Vec<Hit>> {
    let parsed: SearchBody =
        serde_json::from_str(body).context("malformed crates.io search response")?;
    parsed
        .crates
        .into_iter()
        .map(|c| {
            let name = sanitize_text(&c.name);
            validate_name(&name).with_context(|| {
                format!("invalid crate name in crates.io search response: `{name}`")
            })?;
            Ok(Hit {
                name,
                version: c.shown_version(),
                description: sanitize_text(c.description.as_deref().unwrap_or_default()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hits_with_optional_fields() {
        let body = r#"{
            "crates": [
                {"name": "scx_beerland", "max_version": "1.1.3",
                 "max_stable_version": "1.1.3",
                 "description": "A sched_ext scheduler\n  with  odd   spacing"},
                {"name": "beerlang", "max_version": "0.2.0-alpha.1",
                 "max_stable_version": null, "description": null},
                {"name": "bare", "max_version": "0.1.0"}
            ],
            "meta": {"total": 3}
        }"#;
        let hits = parse_search_body(body).unwrap();
        assert_eq!(
            hits,
            [
                Hit {
                    name: "scx_beerland".to_owned(),
                    version: "1.1.3".to_owned(),
                    description: "A sched_ext scheduler with odd spacing".to_owned(),
                },
                Hit {
                    name: "beerlang".to_owned(),
                    // No stable release: the pre-release is what there is.
                    version: "0.2.0-alpha.1".to_owned(),
                    description: String::new(),
                },
                Hit {
                    name: "bare".to_owned(),
                    version: "0.1.0".to_owned(),
                    description: String::new(),
                },
            ]
        );
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        // An escape sequence, a bell and a newline inside a description:
        // all must come out as plain spaced text, with nothing the
        // terminal would interpret.
        let body = r#"{"crates": [
            {"name": "x", "max_version": "1.0.0",
             "description": "foo\u001b[31mbar\u0007\nbaz\ttail"}
        ]}"#;
        let hits = parse_search_body(body).unwrap();
        assert_eq!(hits[0].description, "foo [31mbar baz tail");
        assert!(!hits[0].description.chars().any(char::is_control));

        // The same rule covers the fields that are "always" clean.
        let body = r#"{"crates": [
            {"name": "evil\u001b[2Jname", "max_version": "1.0.0\u0007"}
        ]}"#;
        // A name that is not a crate name after sanitizing is rejected at
        // the boundary, not carried into the install line.
        assert!(parse_search_body(body).is_err());
        let body = r#"{"crates": [{"name": "fine", "max_version": "1.0.0\u0007"}]}"#;
        let hits = parse_search_body(body).unwrap();
        assert_eq!(hits[0].version, "1.0.0");
    }

    #[test]
    fn name_must_be_a_single_crate_name() {
        // Not command injection — nothing runs a shell — but a name that
        // parses as two install tokens would change what the TUI's
        // install line means after a digit pick.
        for name in ["foo --locked", "foo bar", "../foo", ""] {
            let body = format!(r#"{{"crates": [{{"name": "{name}", "max_version": "1.0.0"}}]}}"#);
            assert!(parse_search_body(&body).is_err(), "{name:?}");
        }
    }

    #[test]
    fn throttle_arithmetic_honours_the_interval() {
        let t0 = Instant::now();
        // No previous request: no wait.
        assert_eq!(wait_for_slot(None, t0), Duration::ZERO);
        // 300 ms after the previous one: wait the remaining 700 ms.
        let t1 = t0 + Duration::from_millis(300);
        assert_eq!(wait_for_slot(Some(t0), t1), Duration::from_millis(700));
        // Exactly one interval later, or any time after: no wait.
        assert_eq!(wait_for_slot(Some(t0), t0 + API_INTERVAL), Duration::ZERO);
        assert_eq!(
            wait_for_slot(Some(t0), t0 + Duration::from_secs(5)),
            Duration::ZERO
        );
        // A previous timestamp in the future (clock oddities) is treated
        // as "just now", never as an underflow.
        assert_eq!(wait_for_slot(Some(t1), t0), API_INTERVAL);
    }

    #[test]
    fn version_falls_back_across_api_generations() {
        let body = r#"{"crates": [
            {"name": "stable", "max_stable_version": "1.0.0",
             "default_version": "2.0.0-rc.1", "max_version": "2.0.0-rc.1"},
            {"name": "newapi", "default_version": "3.1.0"},
            {"name": "legacy", "max_version": "0.4.0"},
            {"name": "nulls", "max_stable_version": null,
             "default_version": null, "max_version": null},
            {"name": "bare"}
        ]}"#;
        let versions: Vec<String> = parse_search_body(body)
            .unwrap()
            .into_iter()
            .map(|h| h.version)
            .collect();
        assert_eq!(versions, ["1.0.0", "3.1.0", "0.4.0", "?", "?"]);
    }

    #[test]
    fn empty_result_set_is_not_an_error() {
        let hits = parse_search_body(r#"{"crates": [], "meta": {"total": 0}}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(parse_search_body("{").is_err());
        // `name` is the one field that is not optional.
        assert!(parse_search_body(r#"{"crates": [{"max_version": "1.0.0"}]}"#).is_err());
        assert!(parse_search_body(r#"{"notcrates": []}"#).is_err());
    }
}
