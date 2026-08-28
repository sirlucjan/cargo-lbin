# cargo-lbin

`cargo-lbin` is a small application manager for binaries published on [crates.io](https://crates.io/).

It uses `cargo install` for the build, installs the resulting binaries into `/usr/local/bin` by default, keeps track of what it owns, and adds the small lifecycle layer that plain `cargo install` does not try to provide:

```text
install -> list -> check updates -> update -> remove
```

The executable is a Cargo subcommand, so the normal interface is:

```bash
cargo lbin ...
```

Direct invocation as `cargo-lbin ...` works too.

## Why?

Sometimes a Rust CLI is useful enough to install, but not useful enough to justify writing and maintaining a distro package for it.

`cargo install` already solves the hard part: fetching the crate, resolving dependencies and building it. `cargo-lbin` deliberately leaves all of that to Cargo and only manages the installed application afterwards.

By default it keeps locally managed binaries separate from the distribution package manager:

```text
/usr/bin        -> distro packages
/usr/local/bin  -> cargo-lbin
```

It is **not** a replacement for pacman, rpm, apt, Cargo, or a proper distro package when one is warranted.

## Installation

From crates.io:

```bash
cargo install cargo-lbin
```

From a checkout:

```bash
cargo install --path .
```

Do **not** run `cargo-lbin` itself with `sudo`. Build scripts and proc macros must run as your normal user; `cargo-lbin` requests `sudo` itself only when it needs to place files under `/usr/local`.

## Usage

Install one or more crates:

```bash
cargo lbin install ripgrep
cargo lbin install ripgrep hexyl
```

Build a crate using its committed `Cargo.lock`:

```bash
cargo lbin install some-tool --locked
```

The `--locked` choice is remembered for that crate and reused on future updates.

Installing an already managed crate is a full reinstall: the crate is rebuilt in a fresh staging directory and the binaries are replaced. Changing `--locked` on a reinstall therefore takes effect rather than being skipped as "already installed".

List managed applications:

```bash
cargo lbin list
```

Example:

```text
hexyl 0.14.0 (hexyl)
some-tool 1.2.3 [locked] (some-tool, some-toolctl)
```

Check crates.io for updates without changing anything:

```bash
cargo lbin checkupdate
```

Example:

```text
hexyl 0.14.0 -> 0.16.0
some-tool 1.2.3 -> 1.3.0
```

`checkupdate` uses script-friendly exit codes, following the `pacman-contrib` `checkupdates` convention:

| Exit code | Meaning |
| ---: | --- |
| `0` | updates are available |
| `1` | an error occurred |
| `2` | everything is up to date |

Update all managed crates with available updates:

```bash
cargo lbin update
```

Skip the confirmation prompt:

```bash
cargo lbin update --yes
# or
cargo lbin update -y
```

Remove one or more crates:

```bash
cargo lbin remove hexyl
cargo lbin remove foo bar
```

## Prefixes

The default prefix is `/usr/local`:

```text
/usr/local/bin
/usr/local/share/cargo-lbin/manifest.json
/usr/local/share/cargo-lbin/lock
```

A custom prefix can be selected globally:

```bash
cargo lbin --prefix ~/.local install hexyl
cargo lbin --prefix ~/.local list
```

Custom prefixes must be writable by the invoking user. `cargo-lbin` deliberately permits privilege escalation only for the canonical `/usr/local` prefix; it will not use `sudo` to write into an arbitrary custom path.

Build staging lives under:

```text
$XDG_CACHE_HOME/cargo-lbin/
```

or, when `XDG_CACHE_HOME` is unset:

```text
~/.cache/cargo-lbin/
```

Each process gets its own staging directory, so independent installs targeting different prefixes cannot wipe each other's builds.

## Update behavior

`cargo-lbin` queries the crates.io index directly when checking for updates.

- Yanked releases are ignored.
- Stable installations are offered stable releases only.
- An installed pre-release may be updated to whatever is newest — a later pre-release or a stable release. Once it lands on a stable version, only stable releases are offered again.
- Versions are compared using SemVer.
- The version recorded in the manifest is the version Cargo actually built, read from Cargo's staging metadata rather than assumed from an earlier update check.
- If a new version stops shipping one of its previous binaries, the obsolete binary is removed during the update.

Network errors are reported as errors rather than silently producing an incomplete update list.

## Safety model

`cargo-lbin` is intentionally conservative because it builds third-party crates and may later perform a small number of filesystem operations with elevated privileges.

### Builds never run through sudo

The crate is built first, as the invoking user, in an isolated staging root. Only placement into a protected `/usr/local` destination may trigger `sudo`.

Running the entire tool as root is rejected:

```bash
sudo cargo lbin install foo
# error: cargo-lbin must not be run as root
```

For root-only containers or CI environments, this guard can be explicitly overridden with:

```bash
CARGO_LBIN_ALLOW_ROOT=1 cargo lbin ...
```

### Existing files are not silently overwritten

Before installing anything, `cargo-lbin` checks every destination name.

A binary may be replaced only when the manifest already records it as belonging to the same crate. A binary owned by another managed crate, or an unmanaged file already present in the destination, causes the operation to fail without clobbering it.

The manifest also enforces that every binary name has exactly one owning crate.

### Privileged commands use trusted paths

Operations that may run through `sudo` invoke trusted system tools by absolute path rather than resolving them through the user's `$PATH`. This prevents a build script from placing a fake `sudo`, `install`, or similar executable earlier in the path before placement begins.

### Staged binaries are pinned before privileged placement

A staged binary is opened by `cargo-lbin` with symlink following disabled and verified as a regular file owned by the invoking user. Privileged placement copies the already-open file descriptor through `/proc/<pid>/fd/...`, not a stage pathname that could be swapped after validation.

Binaries are installed atomically: a complete temporary file is written in the destination directory and renamed into place. An interrupted update therefore does not leave a half-written executable in place of the previous version.

### The manifest is validated, sealed and committed atomically

The manifest lives at:

```text
<prefix>/share/cargo-lbin/manifest.json
```

It is treated as untrusted input whenever it is loaded. Crate names, binary names, versions, duplicate ownership and path safety are validated before the data can steer file operations.

When writing state, `cargo-lbin` serializes the manifest into an anonymous Linux `memfd`, seals it against further modification, verifies the sealed contents, and only then hands it to the placement code. The final manifest is installed through a same-directory temporary file and atomic rename.

In short: `cargo-lbin` does not knowingly write manifest state that it would refuse to read back later.

### Concurrent instances are serialized per prefix

Each prefix has a state lock:

```text
<prefix>/share/cargo-lbin/lock
```

Mutating operations (`install`, `update`, `remove`) take an exclusive lock. `list` holds a shared lock for the duration of the listing; update checks hold one only long enough to snapshot the manifest — neither the crates.io queries nor the interactive update prompt run under the lock.

`update` re-reads the manifest after confirmation before changing anything, so state modified while the prompt was open is not acted on blindly.

### Partial failures are recoverable

Crates in a batch (`cargo lbin install foo bar`) are built, installed and committed to the manifest one at a time. A failure on a later crate leaves the earlier ones fully installed and recorded.

Multi-binary crates are handled carefully. If an install or update fails before its manifest commit, newly introduced binary names that were already placed are removed on a best-effort basis. Existing names that were being updated remain owned by the old manifest entry and can be replaced safely on retry.

A hard process kill or power loss in the narrow window between placing a new binary name and committing the manifest can still leave an unmanaged orphan. In that case `cargo-lbin` refuses to overwrite it and tells you to remove the leftover manually before retrying. A persistent transaction journal is intentionally out of scope.

## SELinux

After placement, `cargo-lbin` runs `restorecon` on installed binaries when it is available in a trusted system location. This is best-effort: systems without SELinux tooling simply skip the step.

## Requirements

`cargo-lbin` currently targets Linux.

- Rust/Cargo **1.91 or newer** to build the tool.
- A working Cargo setup and network access to crates.io for installs and update checks.
- `/proc` mounted and available for descriptor-based placement.
- `sudo` when the default `/usr/local` destination is not writable by the user.
- Standard GNU/Linux userland tools in their conventional `/usr/bin` locations (`sudo`, `install`, `rm`, `mkdir`, `touch`, `mv`, `chmod`).

The current implementation is developed with conventional Arch Linux and Fedora-style layouts in mind.

## Non-goals

Keeping the scope small is deliberate. `cargo-lbin` does not try to become another Cargo or a distro package manager.

It currently does **not** provide:

- Git or local path sources (`--git`, `--path`).
- Arbitrary version selection such as `foo@1.2.3`.
- Privileged installation into arbitrary custom prefixes.
- Management of libraries, headers, systemd units, configuration files or other distro integration.
- A dependency resolver of its own — Cargo remains responsible for builds and dependencies.
- Automatic background updates or a daemon.

For a normal per-user `~/.cargo/bin` workflow, plain `cargo install` remains the simpler tool.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
