//! Input validation.
//!
//! Everything that steers privileged file operations — CLI arguments, the
//! manifest, the stage's `.crates2.json` — passes through here. The rest of
//! the code may then trust two invariants: a crate name is a safe crates.io
//! identifier (and a safe path component), and a bin name is exactly one
//! plain filename, never a path.

use anyhow::{Context, Result, bail};
use semver::Version;
use std::ffi::OsStr;
use std::path::{Component, Path};

/// What `install` was asked for: a crate, optionally at one exact
/// version (`name@1.2.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallSpec {
    pub name: String,
    pub version: Option<Version>,
}

impl InstallSpec {
    /// Parse every spec and refuse a crate named more than once — with
    /// or without versions, in any combination. One `install` call
    /// checks pins once, against the manifest as it was before the
    /// first build; a second spec for the same crate would run after
    /// that check and see a pin the first one just set (or set a pin
    /// the first one did not ask for), and `foo@1.2.3 foo` would end
    /// with the newest version pinned. Two builds of one crate in one
    /// command is never what anyone meant, so the whole command is
    /// refused before anything is built.
    pub fn parse_all(specs: &[String]) -> Result<Vec<Self>> {
        let parsed = specs
            .iter()
            .map(|s| Self::parse(s))
            .collect::<Result<Vec<_>>>()?;
        let mut seen = std::collections::HashSet::new();
        for spec in &parsed {
            if !seen.insert(spec.name.as_str()) {
                bail!("crate `{}` is given more than once", spec.name);
            }
        }
        Ok(parsed)
    }

    /// Parse `NAME` or `NAME@VERSION`. The version is an exact semver
    /// version, not a requirement: `foo@^1` is refused, because "any
    /// matching version" is what `install foo` already means, and the
    /// point of naming one is to get that one and keep it (the caller
    /// pins it). The name is validated as every other name is.
    pub fn parse(spec: &str) -> Result<Self> {
        let (name, version) = match spec.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (spec, None),
        };
        validate_name(name)?;
        let version = match version {
            None => None,
            Some("") => bail!("`{spec}`: empty version after `@`"),
            Some(v) => Some(Version::parse(v).with_context(|| {
                format!("`{spec}`: version must be an exact semver version such as 1.2.3")
            })?),
        };
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }
}

/// crates.io package name: ASCII alphanumeric plus `-`/`_`, first character
/// alphabetic (probes of digit-first names against the live index all 404).
/// This also keeps the name safe to use as a path component for the
/// per-crate stage.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if !valid {
        bail!("`{name}` is not a valid crates.io package name");
    }
    Ok(())
}

/// Binary name: exactly one normal path component, equal to the whole
/// string. Anything that could escape the bin directory when joined —
/// absolute paths, `..`, separators, `.` — is rejected. This matters
/// because bin names from the manifest and the stage bookkeeping end up in
/// `rm`/`install` invocations that may run under sudo.
pub fn validate_bin_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == OsStr::new(name) => Ok(()),
        _ => bail!("`{name}` is not a valid binary name"),
    }
}

/// A list of binary names: every element a valid single filename, no
/// duplicates. Used for both the stage's `.crates2.json` (before anything
/// is placed) and manifest entries.
pub fn validate_bin_list(bins: &[String]) -> Result<()> {
    if bins.is_empty() {
        bail!("crate provides no binaries");
    }
    let mut seen = std::collections::BTreeSet::new();
    for bin in bins {
        validate_bin_name(bin)?;
        if !seen.insert(bin.as_str()) {
            bail!("binary `{bin}` is listed twice");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_spec_parses_name_and_exact_version() {
        let spec = InstallSpec::parse("scx_beerland@1.1.2").unwrap();
        assert_eq!(spec.name, "scx_beerland");
        assert_eq!(spec.version, Some(Version::parse("1.1.2").unwrap()));
        let spec = InstallSpec::parse("bat").unwrap();
        assert_eq!(spec.version, None);
        // A pre-release is an exact version too.
        assert!(InstallSpec::parse("foo@2.0.0-rc.1").is_ok());
    }

    #[test]
    fn install_specs_refuse_a_crate_named_twice() {
        let specs = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        for dup in [
            &["foo", "foo"][..],
            &["foo", "foo@1.2.3"],
            &["foo@1.2.3", "foo"],
            &["foo@1.2.3", "foo@1.3.0"],
            &["bar", "foo@1.2.3", "baz", "foo"],
        ] {
            let err = InstallSpec::parse_all(&specs(dup)).unwrap_err().to_string();
            assert!(err.contains("`foo`"), "{dup:?}: {err}");
        }
        let ok = InstallSpec::parse_all(&specs(&["foo@1.2.3", "bar", "baz@0.1.0"])).unwrap();
        assert_eq!(ok.len(), 3);
    }

    #[test]
    fn install_spec_refuses_requirements_and_junk() {
        for bad in [
            "foo@", "foo@^1", "foo@~1.2", "foo@1", "foo@1.2", "foo@=1.2.3", "foo@ 1.2.3",
            "@1.2.3", "../x@1.2.3", "foo@1.2.3@4",
        ] {
            assert!(InstallSpec::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn name_validation_matches_crates_io_rules() {
        assert!(validate_name("ripgrep").is_ok());
        assert!(validate_name("scx-tools").is_ok());
        assert!(validate_name("serde_json").is_ok());
        assert!(validate_name("b2").is_ok());
        // First character must be alphabetic.
        assert!(validate_name("1foo").is_err());
        assert!(validate_name("_foo").is_err());
        assert!(validate_name("-foo").is_err());
        // Path-like and non-ASCII names must never reach path joins or
        // index_path byte slicing.
        assert!(validate_name("../../etc").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("żółć").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn bin_names_are_single_filenames() {
        assert!(validate_bin_name("scxtop").is_ok());
        assert!(validate_bin_name("cargo-watch").is_ok());
        assert!(validate_bin_name("foo_bar").is_ok());
        assert!(validate_bin_name("foo.exe").is_ok());

        assert!(validate_bin_name("../foo").is_err());
        assert!(validate_bin_name("foo/bar").is_err());
        assert!(validate_bin_name("/var/tmp/foo").is_err());
        assert!(validate_bin_name("/etc/something").is_err());
        assert!(validate_bin_name(".").is_err());
        assert!(validate_bin_name("..").is_err());
        assert!(validate_bin_name("").is_err());
    }

    #[test]
    fn bin_lists_reject_duplicates() {
        let ok = vec!["foo".to_owned(), "fooctl".to_owned()];
        assert!(validate_bin_list(&ok).is_ok());
        let dup = vec!["foo".to_owned(), "foo".to_owned()];
        let err = validate_bin_list(&dup).unwrap_err().to_string();
        assert!(err.contains("listed twice"), "{err}");
        let bad = vec!["../foo".to_owned()];
        assert!(validate_bin_list(&bad).is_err());
        // A crate entry without any binary is a model inconsistency, not a
        // valid degenerate case: cargo install fails on bin-less crates.
        assert!(validate_bin_list(&[]).is_err());
    }
}
