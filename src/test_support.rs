//! Shared test fixtures.

use crate::manifest::{Entry, Manifest};
#[cfg(feature = "tui")]
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};

/// Shared scaffolding: a prefix with a manifest entry and a placed
/// binary, as a finished install leaves them.
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

/// A fake cargo staging `name` 0.1.0, the migrate tests' build.
pub(crate) fn staging_fake(root: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake_bin = root.join("fakebin");
    fs::create_dir_all(&fake_bin).unwrap();
    let script = fake_bin.join("cargo");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             mkdir -p \"$4/bin\"\n\
             printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/{name}\"\n\
             chmod 755 \"$4/bin/{name}\"\n\
             printf '%s' '{{\"installs\":{{\"{name} 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)\":{{\"bins\":[\"{name}\"]}}}}}}' > \"$4/.crates2.json\"\n\
             exit 0\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A fake cargo with a registry: `--version =X` stages exactly X,
/// no version request stages 0.2.0 — the fake's "latest". This is
/// the fake for tests about *which* version a pipeline asks for;
/// `staging_fake` above, blind to the request, cannot tell an
/// exact rebuild from a latest install.
pub(crate) fn versioned_fake(root: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake_bin = root.join("fakebin");
    fs::create_dir_all(&fake_bin).unwrap();
    let script = fake_bin.join("cargo");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             ver=0.2.0\n\
             for a in \"$@\"; do\n\
             case \"$a\" in =*) ver=${{a#=}};; esac\n\
             done\n\
             mkdir -p \"$4/bin\"\n\
             printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/{name}\"\n\
             chmod 755 \"$4/bin/{name}\"\n\
             printf '%s' \"{{\\\"installs\\\":{{\\\"{name} $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{{\\\"bins\\\":[\\\"{name}\\\"]}}}}}}\" > \"$4/.crates2.json\"\n\
             exit 0\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A fake cargo that builds whatever crate it is asked for: `$2` is
/// the name, `$4` the stage root. The sweep needs it, because a
/// sweep by definition names more than one crate.
pub(crate) fn any_crate_fake(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake_bin = root.join("fakebin");
    fs::create_dir_all(&fake_bin).unwrap();
    let script = fake_bin.join("cargo");
    fs::write(
        &script,
        "#!/bin/sh\n\
         name=$2\n\
         ver=0.2.0\n\
         for a in \"$@\"; do\n\
         case \"$a\" in =*) ver=${a#=};; esac\n\
         done\n\
         mkdir -p \"$4/bin\"\n\
         printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/$name\"\n\
         chmod 755 \"$4/bin/$name\"\n\
         printf '%s' \"{\\\"installs\\\":{\\\"$name $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{\\\"bins\\\":[\\\"$name\\\"]}}}\" > \"$4/.crates2.json\"\n\
         exit 0\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A fake cargo that refuses one crate by name and builds every
/// other, for testing what a sweep does around a failure.
pub(crate) fn failing_fake(root: &Path, failing: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fake_bin = root.join("fakebin");
    fs::create_dir_all(&fake_bin).unwrap();
    let script = fake_bin.join("cargo");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             name=$2\n\
             [ \"$name\" = \"{failing}\" ] && {{ echo 'error: could not compile' >&2; exit 101; }}\n\
             ver=0.2.0\n\
             for a in \"$@\"; do\n\
             case \"$a\" in =*) ver=${{a#=}};; esac\n\
             done\n\
             mkdir -p \"$4/bin\"\n\
             printf '#!/bin/sh\\ntrue\\n' > \"$4/bin/$name\"\n\
             chmod 755 \"$4/bin/$name\"\n\
             printf '%s' \"{{\\\"installs\\\":{{\\\"$name $ver (registry+https://github.com/rust-lang/crates.io-index)\\\":{{\\\"bins\\\":[\\\"$name\\\"]}}}}}}\" > \"$4/.crates2.json\"\n\
             exit 0\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(feature = "tui")]
pub(crate) fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

#[cfg(feature = "tui")]
pub(crate) fn manifest(entries: &[(&str, &str)]) -> Manifest {
    let mut m = Manifest::default();
    for (name, version) in entries {
        m.crates.insert(
            (*name).to_owned(),
            Entry {
                version: (*version).to_owned(),
                bins: vec![(*name).to_owned()],
                locked: false,
                pinned: false,
                built_with_rustc: None,
            },
        );
    }
    m
}
