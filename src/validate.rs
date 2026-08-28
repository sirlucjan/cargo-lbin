//! Input validation.
//!
//! Everything that steers privileged file operations — CLI arguments, the
//! manifest, the stage's `.crates2.json` — passes through here. The rest of
//! the code may then trust two invariants: a crate name is a safe crates.io
//! identifier (and a safe path component), and a bin name is exactly one
//! plain filename, never a path.

use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::path::{Component, Path};

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
