//! Machine-readable command output (`--json`).
//!
//! A contract: every document carries `schema`; existing fields are
//! never renamed, removed, retyped or re-semanticized without a bump;
//! new fields may be added within a version and consumers must ignore
//! unknown ones. Member order is not part of the schema. Golden tests
//! pin the emitted representation so any change is deliberate.
//!
//! On stdout: the document and nothing else; warnings on stderr, exit
//! codes the text mode's.

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
    /// Unix seconds of the last recorded update check; `null` if none.
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
    /// Other known lbin prefixes carrying this crate; additive, skipped
    /// when empty — schema 1 consumers keep parsing untouched documents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_in: Vec<AlsoInJson>,
}

/// One foreign installation, mirrored from `prefixes::AlsoIn`.
#[derive(Serialize)]
pub struct AlsoInJson {
    pub prefix: PathBuf,
    pub version: String,
}

/// `report::Status`'s three states plus its `None`, named explicitly
/// so a script never infers "unknown" from a missing field.
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
    pub fn build(
        prefix: PathBuf,
        manifest: &Manifest,
        report: Option<&Report>,
        also: &std::collections::BTreeMap<String, Vec<crate::prefixes::AlsoIn>>,
    ) -> Self {
        let crates = manifest
            .crates
            .iter()
            .map(|(name, entry)| ListCrate::annotated_with(name, entry, report, also))
            .collect();
        Self {
            schema: SCHEMA,
            prefix,
            checked_at: report.and_then(|r| r.checked_at),
            crates,
        }
    }
}

impl ListCrate {
    /// `annotated` plus the cross-prefix annotation — the one
    /// constructor both `ListOutput` and `PinnedOutput` use, so the
    /// subset invariant between them cannot silently rot.
    fn annotated_with(
        name: &str,
        entry: &crate::manifest::Entry,
        report: Option<&Report>,
        also: &std::collections::BTreeMap<String, Vec<crate::prefixes::AlsoIn>>,
    ) -> Self {
        let mut c = Self::annotated(name, entry, report);
        if let Some(entries) = also.get(name) {
            c.also_in = entries
                .iter()
                .map(|a| AlsoInJson {
                    prefix: a.prefix.clone(),
                    version: a.version.clone(),
                })
                .collect();
        }
        c
    }

    /// One manifest entry annotated from a report — the single mapping
    /// shared by JSON views of manifest state, so no two views can
    /// drift in how they read the same record.
    fn annotated(name: &str, entry: &crate::manifest::Entry, report: Option<&Report>) -> Self {
        // `latest` is what the check found — not `current` echoed back,
        // which would be wrong when the installed version was yanked since.
        let (status, latest) = Version::parse(&entry.version)
            .ok()
            .and_then(|current| report?.record_for(name, &current))
            .map_or((ListStatus::Unknown, None), |(checked, status)| {
                let status = match status {
                    crate::report::Status::Outdated(_) => ListStatus::Outdated,
                    crate::report::Status::UpToDate => ListStatus::UpToDate,
                };
                (status, Some(checked.latest.clone()))
            });
        Self {
            name: name.to_owned(),
            version: entry.version.clone(),
            bins: entry.bins.clone(),
            locked: entry.locked,
            pinned: entry.pinned,
            status,
            latest,
            also_in: Vec::new(),
        }
    }
}

/// `pinned [--check] --json`: the pinned subset in exactly the
/// `list --json` per-crate shape; `pinned` carried though always true
/// — uniformity is the point.
#[derive(Serialize)]
pub struct PinnedOutput {
    pub schema: u32,
    pub prefix: PathBuf,
    /// Unix seconds of the check the statuses come from — the recorded
    /// one by default, the moment of the query under `--check`; `null`
    /// if no check is recorded.
    pub checked_at: Option<u64>,
    pub crates: Vec<ListCrate>,
}

impl PinnedOutput {
    pub fn build(
        prefix: PathBuf,
        manifest: &Manifest,
        report: Option<&Report>,
        also: &std::collections::BTreeMap<String, Vec<crate::prefixes::AlsoIn>>,
    ) -> Self {
        // The same annotation path as ListOutput: the documented invariant is
        // entry-for-entry equality on the pinned subset.
        let crates = manifest
            .crates
            .iter()
            .filter(|(_, entry)| entry.pinned)
            .map(|(name, entry)| ListCrate::annotated_with(name, entry, report, also))
            .collect();
        Self {
            schema: SCHEMA,
            prefix,
            checked_at: report.and_then(|r| r.checked_at),
            crates,
        }
    }
}

impl CheckOutput {
    pub fn from_report(report: &Report) -> Self {
        Self {
            schema: SCHEMA,
            prefix: report.prefix.clone(),
            checked_at: report
                .checked_at
                .expect("a fresh sweep always carries its baseline"),
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

fn clone_finding(f: &crate::Finding) -> crate::Finding {
    crate::Finding {
        kind: f.kind,
        message: f.message.clone(),
        krate: f.krate.clone(),
        bin: f.bin.clone(),
        path: f.path.clone(),
        hint: f.hint.clone(),
    }
}

/// `verify --json`: the audit's findings as data. Every finding
/// carries `kind` (a stable machine name), `message` (the human
/// finding text, hint embedded; the text renderers add their own
/// framing, e.g. the severity word), the subjects `crate`/`bin`/`path`
/// where the finding has them (else `null`), and `hint` — the bare
/// pasteable repair command where one is unambiguous: a reinstall for
/// a broken binary; stale-stages carries none. `crates` is `null` when
/// the manifest could not be counted — the same Option honesty as the
/// text verdict.
#[derive(Serialize)]
pub(crate) struct VerifyOutput {
    schema: u32,
    prefix: PathBuf,
    crates: Option<usize>,
    errors: Vec<crate::Finding>,
    warnings: Vec<crate::Finding>,
}

impl VerifyOutput {
    pub(crate) fn build(prefix: PathBuf, report: &crate::VerifyReport) -> Self {
        Self {
            schema: SCHEMA,
            prefix,
            crates: report.crates,
            errors: report.errors.iter().map(clone_finding).collect(),
            warnings: report.warnings.iter().map(clone_finding).collect(),
        }
    }
}

/// The whole `verify --json` document, prefix anchored the same way
/// every other document anchors it, printed like every other document.
pub(crate) fn print_verify(prefix: &std::path::Path, report: &crate::VerifyReport) -> Result<()> {
    let identity = crate::report::identity(prefix)?;
    print(&VerifyOutput::build(identity, report))
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
            checked_at: Some(1_756_761_600),
            prefix: PathBuf::from("/usr/local"),
            crates: vec![
                Checked {
                    name: "bat".to_owned(),
                    current: v("0.26.0"),
                    checked_at: None,
                    latest: v("0.26.1"),
                },
                // Checked against an older fd, and updated since to the
                // very version this report named: the report can answer
                // that one, and every surface must answer it the same
                // way — this golden is where a JSON view drifting from
                // the list and the interface would show up.
                Checked {
                    name: "fd".to_owned(),
                    current: v("10.2.0"),
                    checked_at: None,
                    latest: v("10.3.0"),
                },
                // Up to date, yet `latest` below `current`: 14.1.1 was
                // yanked after install and 14.1.0 is the newest live one.
                Checked {
                    name: "ripgrep".to_owned(),
                    current: v("14.1.1"),
                    checked_at: None,
                    latest: v("14.1.0"),
                },
            ],
        }
    }

    #[test]
    fn verify_output_golden() {
        let report = crate::VerifyReport {
            crates: Some(2),
            errors: vec![crate::Finding {
                kind: "binary-missing",
                message: "`foo`: managed binary /usr/local/bin/foo is missing — reinstall: \
                          cargo lbin install --reinstall foo --prefix=/usr/local"
                    .into(),
                krate: Some("foo".into()),
                bin: Some("foo".into()),
                path: Some(PathBuf::from("/usr/local/bin/foo")),
                hint: Some("cargo lbin install --reinstall foo --prefix=/usr/local".into()),
            }],
            warnings: vec![crate::Finding {
                kind: "stale-stages",
                message: "1 stage directory under /home/u/.cache/cargo-lbin with no live \
                          owner — possible leftover build debris; inspect, then \
                          `cargo lbin clean --stages` when safe"
                    .into(),
                krate: None,
                bin: None,
                path: Some(PathBuf::from("/home/u/.cache/cargo-lbin")),
                hint: None,
            }],
        };
        let out = VerifyOutput::build(PathBuf::from("/usr/local"), &report);
        let json = serde_json::to_string_pretty(&out).unwrap();
        let expected = r#"{
  "schema": 1,
  "prefix": "/usr/local",
  "crates": 2,
  "errors": [
    {
      "kind": "binary-missing",
      "message": "`foo`: managed binary /usr/local/bin/foo is missing — reinstall: cargo lbin install --reinstall foo --prefix=/usr/local",
      "crate": "foo",
      "bin": "foo",
      "path": "/usr/local/bin/foo",
      "hint": "cargo lbin install --reinstall foo --prefix=/usr/local"
    }
  ],
  "warnings": [
    {
      "kind": "stale-stages",
      "message": "1 stage directory under /home/u/.cache/cargo-lbin with no live owner — possible leftover build debris; inspect, then `cargo lbin clean --stages` when safe",
      "crate": null,
      "bin": null,
      "path": "/home/u/.cache/cargo-lbin",
      "hint": null
    }
  ]
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn verify_output_uncounted_is_null_not_zero() {
        // The Option honesty crosses into the document: an unparseable
        // manifest is "crates": null, never an invented 0.
        let report = crate::VerifyReport {
            crates: None,
            errors: vec![crate::Finding {
                kind: "manifest-unparseable",
                message: "the manifest cannot be parsed".into(),
                krate: None,
                bin: None,
                path: None,
                hint: None,
            }],
            warnings: Vec::new(),
        };
        let out = VerifyOutput::build(PathBuf::from("/usr/local"), &report);
        let json = serde_json::to_string_pretty(&out).unwrap();
        assert!(json.contains("\"crates\": null"), "{json}");
        assert!(
            json.contains("\"kind\": \"manifest-unparseable\""),
            "{json}"
        );
    }

    /// The three surfaces answer the same question the same way. This
    /// one is easy to forget, because JSON has no screen to notice on:
    /// after `U` installs the update a report named, a script asking
    /// `list --json` must see what the list and the interface see.
    #[test]
    fn json_reads_the_report_the_way_every_other_surface_does() {
        let report = Report::new(
            Path::new("/usr/local"),
            vec![Checked {
                name: "fd".to_owned(),
                current: Version::parse("10.2.0").unwrap(),
                latest: Version::parse("10.3.0").unwrap(),

                checked_at: None,
            }],
        )
        .unwrap();
        let entry = crate::manifest::Entry {
            version: "10.3.0".to_owned(),
            bins: vec!["fd".to_owned()],
            locked: false,
            pinned: false,
        };

        let updated = ListCrate::annotated("fd", &entry, Some(&report));
        assert!(matches!(updated.status, ListStatus::UpToDate));
        assert_eq!(
            updated.latest.map(|v| v.to_string()),
            Some("10.3.0".to_owned()),
            "the version the report named is still what it named"
        );

        // And past what the report saw, JSON says unknown like the rest.
        let past = crate::manifest::Entry {
            version: "10.4.0".to_owned(),
            ..entry
        };
        let beyond = ListCrate::annotated("fd", &past, Some(&report));
        assert!(matches!(beyond.status, ListStatus::Unknown));
        assert!(beyond.latest.is_none(), "absent knowledge, not a guess");
    }

    /// The representation as emitted, byte for byte: editing this test is
    /// expected when a field is added; a schema bump only when an existing
    /// field changes.
    #[test]
    fn list_output_golden() {
        let out = ListOutput::build(
            PathBuf::from("/usr/local"),
            &manifest(),
            Some(&report()),
            &std::collections::BTreeMap::new(),
        );
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
      "status": "up_to_date",
      "latest": "10.3.0"
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
        let out = ListOutput::build(
            PathBuf::from("/p"),
            &manifest(),
            None,
            &std::collections::BTreeMap::new(),
        );
        let value = serde_json::to_value(&out).unwrap();
        assert_eq!(value["checked_at"], serde_json::Value::Null);
        for c in value["crates"].as_array().unwrap() {
            assert_eq!(c["status"], "unknown");
            assert_eq!(c["latest"], serde_json::Value::Null);
        }
        // An empty prefix is still a complete document, not a message.
        let out = ListOutput::build(
            PathBuf::from("/p"),
            &Manifest::default(),
            None,
            &std::collections::BTreeMap::new(),
        );
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
    fn pinned_output_golden() {
        let out = PinnedOutput::build(
            PathBuf::from("/usr/local"),
            &manifest(),
            Some(&report()),
            &std::collections::BTreeMap::new(),
        );
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
    }
  ]
}"#;
        assert_eq!(json, expected);
    }

    #[test]
    fn pinned_output_is_the_pinned_subset_of_list() {
        // The invariant a consumer may rely on, fed a non-empty map touching
        // a pinned crate — an empty one would stay green while the annotation
        // silently vanished.
        let mut also = std::collections::BTreeMap::new();
        also.insert(
            "bat".to_owned(),
            vec![crate::prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "0.25.0".to_owned(),
            }],
        );
        let list = ListOutput::build(PathBuf::from("/p"), &manifest(), Some(&report()), &also);
        let pinned = PinnedOutput::build(PathBuf::from("/p"), &manifest(), Some(&report()), &also);
        let expected: Vec<_> = list
            .crates
            .iter()
            .filter(|c| c.pinned)
            .map(|c| serde_json::to_value(c).unwrap())
            .collect();
        let got: Vec<_> = pinned
            .crates
            .iter()
            .map(|c| serde_json::to_value(c).unwrap())
            .collect();
        assert_eq!(got, expected);
        // No pins is a complete document, not a message.
        let none = PinnedOutput::build(
            PathBuf::from("/p"),
            &Manifest::default(),
            None,
            &std::collections::BTreeMap::new(),
        );
        let value = serde_json::to_value(&none).unwrap();
        assert_eq!(value["crates"], serde_json::json!([]));
        assert_eq!(value["checked_at"], serde_json::Value::Null);
        assert_eq!(value["schema"], SCHEMA);
    }

    #[test]
    fn identity_is_what_prefix_reports() {
        // `list --json` reports the anchored prefix, so two spellings of
        // one tree produce one `prefix` value.
        let a = crate::report::identity(Path::new("/usr/local/")).unwrap();
        let b = crate::report::identity(Path::new("/usr/./local")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn also_in_is_additive_and_absent_when_empty() {
        // Schema stays 1: skipped entirely for a crate installed nowhere
        // else, so untouched documents stay byte-compatible; when present it
        // names prefix and version — skew is what the reader wants to notice.
        let empty = ListOutput::build(
            PathBuf::from("/p"),
            &manifest(),
            None,
            &std::collections::BTreeMap::new(),
        );
        let text = serde_json::to_string(&empty).unwrap();
        assert!(
            !text.contains("also_in"),
            "absent, not an empty array: {text}"
        );

        let mut also = std::collections::BTreeMap::new();
        also.insert(
            "ripgrep".to_owned(),
            vec![crate::prefixes::AlsoIn {
                prefix: PathBuf::from("/usr/local"),
                version: "9.9.9".to_owned(),
            }],
        );
        let full = ListOutput::build(PathBuf::from("/p"), &manifest(), None, &also);
        let text = serde_json::to_string(&full).unwrap();
        assert!(
            text.contains(r#""also_in":[{"prefix":"/usr/local","version":"9.9.9"}]"#),
            "{text}"
        );
    }
}
