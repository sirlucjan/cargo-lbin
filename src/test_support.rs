//! Shared test fixtures.

use crate::manifest::{Entry, Manifest};
use std::fs;
use std::path::{Path, PathBuf};

/// Shared scaffolding for the migrate tests: a prefix with a
/// manifest entry and a placed binary, as a finished install leaves
/// them.
pub(crate) fn seeded_prefix(
    root: &Path,
    dir: &str,
    name: &str,
    locked: bool,
    pinned: bool,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let prefix = root.join(dir);
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::create_dir_all(prefix.join("share/cargo-lbin")).unwrap();
    fs::write(prefix.join("bin").join(name), "#!/bin/sh\ntrue\n").unwrap();
    fs::set_permissions(
        prefix.join("bin").join(name),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let mut manifest = Manifest::default();
    manifest.crates.insert(
        name.to_owned(),
        Entry {
            version: "0.1.0".into(),
            bins: vec![name.to_owned()],
            locked,
            pinned,
            built_with_rustc: None,
        },
    );
    manifest.store(&prefix).unwrap();
    prefix
}

pub(crate) fn manifest_with(names: &[&str]) -> Manifest {
    let mut m = Manifest::default();
    for n in names {
        m.crates.insert(
            (*n).to_owned(),
            Entry {
                version: "1.0.0".to_owned(),
                bins: vec![(*n).to_owned()],
                locked: false,
                pinned: false,
                built_with_rustc: None,
            },
        );
    }
    m
}
