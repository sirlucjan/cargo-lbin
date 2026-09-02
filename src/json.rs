//! Machine-readable output for `list --json` and `checkupdate --json`.
//!
//! This is a contract. A script written against it today must keep
//! working. Every document carries `schema`. Existing fields are never
//! renamed, removed, retyped or given new semantics without a schema
//! bump. New fields may be added within the same schema version;
//! consumers must ignore fields they do not know. JSON object member
//! order is not part of the schema — JSON gives a consumer no semantics
//! for it, and a script that greps instead of parsing is not owed
//! compatibility.
//!
//! Golden tests pin the currently emitted representation so that any
//! change to it is deliberate: adding a field edits the golden and keeps
//! the schema number; changing an existing field edits the golden and
//! bumps it.
//!
//! On stdout: the JSON document and nothing else. Warnings stay on
//! stderr, exit codes are the text mode's.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use semver::Version;
use serde::Serialize;

use crate::manifest::Manifest;
use crate::report::Report;

/// Bumped only when an existing field changes meaning, type or name.
pub const SCHEMA: u32 = 1;

#[derive(Serialize)]
pub struct ListOutput {
    pub schema: u32,
    /// The prefix as an absolute, normalized path (see `report::identity`).
    pub prefix: PathBuf,
    /// Unix seconds of the last `checkupdate`; `null` if none is recorded.
    pub checked_at: Option<u64>,
    pub crates: Vec<ListCrate>,
}

#[derive(Serialize)]
pub struct ListCrate {
    pub name: String,
    pub version: String,
    pub bins: Vec<String>,
    pub locked: bool,
    pub pinned: bool,
    pub status: ListStatus,
    /// The newest version the last check found; `null` when `status` is
    /// `unknown` — absent knowledge, not an empty version.
    pub latest: Option<Version>,
}

/// The three states of `report::Status`, plus the one it expresses as
/// `None`, named explicitly so a script never has to infer "unknown"
/// from a missing field.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ListStatus {
    UpToDate,
    Outdated,
    Unknown,
}

#[derive(Serialize)]
pub struct CheckOutput {
    pub schema: u32,
    pub prefix: PathBuf,
    pub checked_at: u64,
    pub crates: Vec<CheckCrate>,
}

#[derive(Serialize)]
pub struct CheckCrate {
    pub name: String,
    pub current: Version,
    pub latest: Version,
    /// Derived from `latest > current`; carried so every consumer does
    /// not have to compare versions itself.
    pub outdated: bool,
}

impl ListOutput {
    pub fn build(prefix: PathBuf, manifest: &Manifest, report: Option<&Report>) -> Self {
        let crates = manifest
            .crates
            .iter()
            .map(|(name, entry)| {
                // `latest` is what the check found, taken from the record
                // itself — not `current` echoed back for an up-to-date
                // crate, which would be wrong whenever the installed
                // version has been yanked since and the newest live one
                // is older.
                let (status, latest) = Version::parse(&entry.version)
                    .ok()
                    .and_then(|current| report?.checked_for(name, &current))
                    .map_or((ListStatus::Unknown, None), |c| {
                        let status = if c.is_outdated() {
                            ListStatus::Outdated
                        } else {
                            ListStatus::UpToDate
                        };
                        (status, Some(c.latest.clone()))
                    });
                ListCrate {
                    name: name.clone(),
                    version: entry.version.clone(),
                    bins: entry.bins.clone(),
                    locked: entry.locked,
                    pinned: entry.pinned,
                    status,
                    latest,
                }
            })
            .collect();
        Self {
            schema: SCHEMA,
            prefix,
            checked_at: report.map(|r| r.checked_at),
            crates,
        }
    }
}

impl CheckOutput {
    pub fn from_report(report: &Report) -> Self {
        Self {
            schema: SCHEMA,
            prefix: report.prefix.clone(),
            checked_at: report.checked_at,
            crates: report
                .crates
                .iter()
                .map(|c| CheckCrate {
                    name: c.name.clone(),
                    current: c.current.clone(),
                    latest: c.latest.clone(),
                    outdated: c.is_outdated(),
                })
                .collect(),
        }
    }
}

/// Pretty-printed, one document, trailing newline.
pub fn print<T: Serialize>(value: &T) -> Result<()> {
    let mut out = serde_json::to_string_pretty(value).context("serializing JSON output")?;
    out.push('\n');
    std::io::stdout()
        .write_all(out.as_bytes())
        .context("writing JSON output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Entry;
    use crate::report::Checked;
    use std::path::Path;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn manifest() -> Manifest {
        let mut m = Manifest::default();
        for (name, version, bins, locked, pinned) in [
            ("bat", "0.26.0", vec!["bat"], false, true),
            ("fd", "10.3.0", vec!["fd"], true, false),
            ("ripgrep", "14.1.1", vec!["rg"], false, false),
        ] {
            m.crates.insert(
                name.to_owned(),
                Entry {
                    version: version.to_owned(),
                    bins: bins.into_iter().map(str::to_owned).collect(),
                    locked,
                    pinned,
                },
            );
        }
        m
    }

    fn report() -> Report {
        Report {
            checked_at: 1_756_761_600,
            prefix: PathBuf::from("/usr/local"),
            crates: vec![
                Checked {
                    name: "bat".to_owned(),
                    current: v("0.26.0"),
                    latest: v("0.26.1"),
                },
                // Checked against an older fd: updated since → unknown.
                Checked {
                    name: "fd".to_owned(),
                    current: v("10.2.0"),
                    latest: v("10.3.0"),
                },
                // Up to date, yet `latest` below `current`: 14.1.1 was
                // yanked after install and 14.1.0 is the newest live one.
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    latest: v("14.1.0"),
                },
            ],
        }
    }

    /// The representation as currently emitted, byte for byte, so that a
    /// change to it is made on purpose. Editing this test is expected
    /// when a field is added; it is a schema bump only when an existing
    /// field changes (see the module doc).
    #[test]
    fn list_output_golden() {
        let out = ListOutput::build(PathBuf::from("/usr/local"), &manifest(), Some(&report()));
        let json = serde_json::to_string_pretty(&out).unwrap();
        let expected = r#"{
  "schema": 1,
  "prefix": "/usr/local",
  "checked_at": 1756761600,
  "crates": [
    {
      "name": "bat",
      "version": "0.26.0",
      "bins": [
        "bat"
      ],
      "locked": false,
      "pinned": true,
      "status": "outdated",
      "latest": "0.26.1"
    },
    {
      "name": "fd",
      "version": "10.3.0",
      "bins": [
        "fd"
      ],
      "locked": true,
      "pinned": false,
      "status": "unknown",
      "latest": null
    },
    {
      "name": "ripgrep",
      "version": "14.1.1",
      "bins": [
        "rg"
      ],
      "locked": false,
      "pinned": false,
      "status": "up_to_date",
      "latest": "14.1.0"
    }
  ]
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn list_output_without_report_is_all_unknown() {
        let out = ListOutput::build(PathBuf::from("/p"), &manifest(), None);
        let value = serde_json::to_value(&out).unwrap();
        assert_eq!(value["checked_at"], serde_json::Value::Null);
        for c in value["crates"].as_array().unwrap() {
            assert_eq!(c["status"], "unknown");
            assert_eq!(c["latest"], serde_json::Value::Null);
        }
        // An empty prefix is still a complete document, not a message.
        let out = ListOutput::build(PathBuf::from("/p"), &Manifest::default(), None);
        let value = serde_json::to_value(&out).unwrap();
        assert_eq!(value["crates"], serde_json::json!([]));
        assert_eq!(value["schema"], SCHEMA);
    }

    #[test]
    fn check_output_golden() {
        let out = CheckOutput::from_report(&report());
        let json = serde_json::to_string_pretty(&out).unwrap();
        let expected = r#"{
  "schema": 1,
  "prefix": "/usr/local",
  "checked_at": 1756761600,
  "crates": [
    {
      "name": "bat",
      "current": "0.26.0",
      "latest": "0.26.1",
      "outdated": true
    },
    {
      "name": "fd",
      "current": "10.2.0",
      "latest": "10.3.0",
      "outdated": true
    },
    {
      "name": "ripgrep",
      "current": "14.1.1",
      "latest": "14.1.0",
      "outdated": false
    }
  ]
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn identity_is_what_prefix_reports() {
        // `list --json` reports the anchored prefix, so two spellings of
        // one tree produce one `prefix` value.
        let a = crate::report::identity(Path::new("/usr/local/")).unwrap();
        let b = crate::report::identity(Path::new("/usr/./local")).unwrap();
        assert_eq!(a, b);
    }
}
