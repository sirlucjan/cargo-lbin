# cargo-lbin

`cargo-lbin` is a Cargo-powered application manager for command-line binaries published on [crates.io](https://crates.io/).

It leaves fetching, dependency resolution and compilation to `cargo install`, then adds the small lifecycle layer around the resulting applications: discovery, inspection, installation, ownership tracking, update checks, updates and removal.

```text
search -> info -> install -> list -> check updates -> update -> remove
```

By default, managed binaries live in `/usr/local/bin`, separate from distribution packages in `/usr/bin`.

The normal interface is a Cargo subcommand:

```bash
cargo lbin ...
```

Direct invocation as `cargo-lbin ...` works too.

## Why?

Sometimes a Rust CLI is useful enough to install system-wide, but not useful enough to justify writing and maintaining a distro package for it.

`cargo install` already does the hard part. `cargo-lbin` deliberately does **not** replace Cargo's resolver or build process; it manages the installed application afterwards.

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

The terminal UI is enabled by the default `tui` feature. To build only the CLI, without Ratatui/Crossterm dependencies:

```bash
cargo install cargo-lbin --no-default-features
```

Invoked through rustup's `cargo` proxy, `cargo lbin` may print a harmless
`default toolchain implicitly overridden` warning: the proxy exports
`RUSTUP_TOOLCHAIN` and the nested `cargo install` inherits it. This is
expected, `cargo +toolchain lbin` still selects the toolchain it names,
and `cargo-lbin` deliberately does not clear the variable — second-guessing
rustup's environment is not its job.

Do **not** run `cargo-lbin` itself with `sudo`. Build scripts and proc macros must run as your normal user; `cargo-lbin` requests `sudo` itself only when placement under the canonical `/usr/local` prefix requires it.

Where `cargo-lbin` asks for those credentials itself — placement, and a migration's retirement of the source — it names the reason first, because `sudo` prompts for a user and never for a purpose: `administrative privileges are required to install under /usr/local`, or `… to retire the source installation from /usr/local`. One sentence for both surfaces: the CLI prints it, and the TUI prints it on the terminal it steps off before suspending. (Operations that escalate inside a single privileged call, such as a `remove` or a pin flip needing `sudo`, still meet `sudo`'s own prompt directly.)

## Quick start

```bash
# Find a crate when you do not know its exact name
cargo lbin search beerland

# Inspect a crate by exact name
cargo lbin info scx_beerland

# Install it
cargo lbin install scx_beerland

# See everything cargo-lbin manages
cargo lbin list

# Check crates.io for updates
cargo lbin checkupdate

# Update one crate, or all managed crates
cargo lbin update scx_beerland
cargo lbin update --all

# Remove it again
cargo lbin remove scx_beerland

# Or drive the same operations interactively
cargo lbin tui
```

## Commands

| Command | Purpose |
| --- | --- |
| `install <crate[@version]>... [--locked] [--reinstall]` | Build crates with Cargo and install their binaries; `@version` installs exactly that version and pins it; `--reinstall` rebuilds what is already installed, exactly as the manifest records it |
| `remove <crate>...` | Remove managed crates and their binaries |
| `pin <crate>...` / `unpin <crate>...` | Pin crates to their installed version / release the pin |
| `pinned [--check] [--json]` | List pinned crates and whether newer versions exist |
| `downgrade <crate>` | Pick an older version from crates.io, install it and pin it (in the TUI: `D`) |
| `list [--json]` | List managed crates, using the last update report for annotations |
| `checkupdate [--json]` | Query crates.io for updates and save a full local report |
| `update <crate>... [--yes]` | Update explicitly selected managed crates; `--yes` skips confirmation |
| `update --all [--yes]` | Update every managed crate with an available update; `--yes` skips confirmation |
| `migrate <crate>... --to <prefix> [--yes]` | Rebuild installed crates under another prefix, then retire them here |
| `migrate --all --to <prefix> [--yes]` | Migrate every managed crate to another prefix |
| `verify [--json]` | Check the manifest's claims against the disk, read-only; non-zero exit on verification errors |
| `clean [--dry-run] [--stages] [--logs-older-than DAYS]` | Remove build debris from the cache; every removal is opt-in — name at least one of the two |
| `search <terms>... [--limit N]` | Find crates by keyword |
| `info <crate>... [--versions]` | Show exact-name crate information and installed state |
| `tui` | Interactive frontend over the same operations, when the `tui` feature is enabled |
| `completions <shell>` | Print a shell completion script for the commands and flags |
| `man DIR` | Write man pages (roff) for cargo-lbin and every subcommand into DIR |

## Install

Install one or more crates:

```bash
cargo lbin install ripgrep
cargo lbin install ripgrep hexyl
```

Install exactly one version, and keep it:

```bash
cargo lbin install scx_beerland@1.1.2
```

```text
installed scx_beerland 1.1.2 -> /usr/local/bin (scx_beerland) [pinned; `cargo lbin unpin scx_beerland` to allow updates]
```

A version chosen by name is a version meant to stay, so `@version` pins the crate (see [Pin](#pin)); without the pin, the next `update --all` would rebuild the newest release and leave no trace of the choice. The version must be an exact semver version — `foo@^1` is refused, since "any matching version" is what plain `install foo` already means. Cargo refuses yanked versions; `info` shows which ones those are. Installing a named version over an already pinned crate is allowed — it is a re-pin to that version — whereas a bare `install foo` on a pinned crate is refused, because it would build the newest release. A crate may appear only once per `install` command, with or without a version: `install foo@1.2.3 foo` would otherwise end with the newest release pinned, and two builds of one crate in one command is never what was meant.

Build using the crate's committed `Cargo.lock`:

```bash
cargo lbin install some-tool --locked
```

The `--locked` choice is stored per crate and reused on future updates.

A plain `install NAME` of an already managed crate is a full rebuild and replacement: it is built in a fresh staging directory and its managed binaries are replaced, so changing `--locked` this way takes effect instead of being skipped as "already installed". It also resolves the version afresh, which for an unpinned crate means the newest release.

`install --reinstall NAME` rebuilds what is already installed without re-selecting the crate version or its stored policy: the manifest entry is authoritative for the version, the pin and `--locked`, and all three are carried over. Dependencies are still resolved by Cargo as they would be for any build — unless the entry carries `--locked`, which is exactly what that flag records. A pinned crate comes back pinned at the same version, an unpinned one comes back unpinned, and a crate built reproducibly is rebuilt the same way. It is the shape wanted when the environment moves rather than the crate — a new toolchain, a new libc, different compiler flags — and it is what `verify` names when a managed binary needs repairing, because repairing an installation should not also change which version it is. Because the entry already answers both questions, naming a version (`install --reinstall foo@1.2.3`) or passing `--locked` alongside it is a usage error, and `--reinstall` on a crate this prefix does not manage is `not installed`. Cargo still uses the registry and its caches to build that version; what `cargo-lbin` does not ask is *which* version or policy to apply.

Installing a crate that another known prefix already manages — and this one does not — warns before the first build starts, so the batch can still be abandoned before any minutes are invested:

```text
warning: `ripgrep` is already managed under /usr/local @14.1.0
this will install another copy under /home/user/.local
use `cargo lbin migrate ripgrep --prefix=/usr/local --to=/home/user/.local` if you intended to move it
```

It is a warning and never an error — double installation is legal, and `verify` reports the standing duplication afterwards. The migrate line is pasteable as printed — both paths are shell-quoted, so a space or a `$(...)` in a prefix stays a directory name rather than becoming a shell construct; a path with no honest shell spelling (non-UTF-8 or control characters) keeps the warning but drops the exact command — the fallback names the mechanism (`use \`cargo lbin migrate\` with explicit --prefix/--to`) without pretending to be pasteable — the same rule the `verify` reinstall hint follows. A reinstall of a crate both prefixes already carry does not warn: it creates no second copy. In the TUI the row's `[also in …]` annotation shows the state before the key is pressed, and the same warning still lands in the panel.

Installing a binary name the prefix did not have before — every name on a first install, only the added ones when an update introduces a new binary — warns when a file of that name already exists on `PATH` outside the prefix, usually a distribution package:

```text
warning: `rg` already exists as /usr/bin/rg (/usr/bin/rg is owned by ripgrep 14.1.1-1); /usr/local/bin precedes /usr/bin in PATH
```

The owner comes from `/usr/bin/pacman -Qo`, `/usr/bin/rpm -qf` or `/usr/bin/dpkg -S` — the first of these that runs and claims the file; by absolute path, never a `PATH` lookup, since the prefix itself is usually on `PATH` ahead of `/usr/bin`. Without a claim, the file is still reported. The warning reports which directory comes first in `PATH` — the prefix's `bin`, the existing file's directory, or that `<prefix>/bin` is not on `PATH` at all; it does not attempt to determine which file the current user can actually execute. Paths and package-manager output are external data and pass through the same control-character sanitizing as crates.io responses before reaching the terminal. It is a warning, not a refusal: installing a newer version than the distribution ships is a normal reason to use this tool, and the person installing decides.

A `migrate` reports this after it finishes, not while it runs. The rebuild at the destination happens with the source installation still in place, and the retirement that follows may remove it — or, when it refuses or fails, deliberately leave it. So the scan runs once that phase has answered and describes what is actually there: the copy that now comes first on `PATH`, which may be a distribution package the source copy had been hiding; or the source itself, when it survived; or nothing, when nothing is left to shadow. `<destination>/bin is not on PATH` is asked separately, because it is a fact about the destination rather than about any shadowing file: a binary in a directory `PATH` does not list is unreachable by bare name whether or not something else carries that name.

## Search

Find crates by keyword:

```bash
cargo lbin search sched_ext scheduler
cargo lbin search beerland --limit 5
```

Search terms are joined with spaces. `--limit` accepts `1..=100` and defaults to `10`. `cargo-lbin` honours crates.io's one-request-per-second policy, so back-to-back searches wait for the remainder of the second when necessary.

Example:

```text
* scx_beerland  1.1.3  Scheduler designed to prioritize locality and scalability.  [installed 1.1.2]
  scx_lavd      1.1.3  A Latency-criticality Aware Virtual Deadline scheduler ...
* installed under /usr/local
```

`search` uses crates.io's keyword search and preserves its relevance order. Unlike `cargo search`, it can also mark hits already managed under the selected prefix and show their installed version.

The displayed version prefers the newest stable version reported by crates.io. If none is available, `cargo-lbin` falls back to crates.io's default version and then its legacy newest-version field. No matches is a valid answer, not an error.

Search results are only a discovery view. Use `info` for exact crate details and update eligibility, and `info --versions` when you want the full published version history.

## Info

Show one or more crates by exact name:

```bash
cargo lbin info ripgrep bat
```

Example:

```text
ripgrep
  latest:      14.1.1
  releases:    42 (2 yanked)
  installed:   14.1.0 (update available: 14.1.1)
  pinned:      no
  locked:      yes
  binaries:    rg
  also in:     /usr/local @14.0.3

bat
  latest:      0.26.0
  pre-release: 0.27.0-beta.1
  releases:    38
  installed:   no
```

`latest` and `pre-release` describe published history. They may name a yanked release, which is shown explicitly as `[yanked]`. The pre-release line is shown only when that release is newer than the latest stable release.

The `installed` verdict is a separate question. It uses the same non-yanked update rules as `checkupdate`, so `info` does not call something "up to date" when `checkupdate` would disagree. If a crate has no non-yanked releases left, that is reported explicitly. An installed crate also shows its manifest entry in full — `pinned`, `locked`, and the binaries it provides — so `info` is the complete single-crate view of what `list` shows in aggregate. If another known prefix carries the crate too, `info` names each copy with its version (`also in:`), whether or not this prefix has a copy — the foreign manifests are read without any lock, like every cross-prefix annotation, so the answer never waits behind a foreign build.

`--versions` appends the full published version set to each crate's block, in descending SemVer order, with yanked releases marked:

```text
  versions:
    14.1.1
    14.1.0
    14.0.3 [yanked]
```

It lists everything — pre-releases and yanked included — because it answers "install `foo@X`, but which X exists?", and that question is about history, not eligibility; the `installed` verdict above it already applies the eligibility rules. The order is the version axis, not publication time: a `1.9.7` backported after `2.0.0` still sorts below it, which is exactly what the `foo@X` question wants. No separate `versions` command: this is still information about the crate.

Unknown names do not stop the rest of a batch. They are reported after the successful results, with a hint to use `search`; the command exits non-zero if any exact lookup failed.

## List and update reports

List managed applications:

```bash
cargo lbin list
```

Example:

```text
hexyl 0.14.0 [pinned] (hexyl) -> 0.16.0
ripgrep 14.1.1 (rg) (up to date)
some-tool 1.2.3 [locked] (some-tool, some-toolctl)
update check: 3h ago
```

`list` never touches the network. Update annotations come only from the most recent `checkupdate` report:

- `-> VERSION` means a newer version was known at the last check.
- `(up to date)` means that exact installed version was checked and found current.
- No annotation means the crate was not covered by that report, for example because it was installed or updated afterwards.

The report age is printed to stderr so stdout remains suitable for simple parsing. Without a saved report, `list` still lists the manifest and tells you that no update check has been recorded.

Run a fresh check with:

```bash
cargo lbin checkupdate
```

Example:

```text
hexyl 0.14.0 -> 0.16.0
some-tool 1.2.3 -> 1.3.0
```

`checkupdate` is read-only with respect to installed applications and uses script-friendly exit codes following the `pacman-contrib` `checkupdates` convention:

| Exit code | Meaning |
| ---: | --- |
| `0` | updates are available |
| `1` | an error occurred |
| `2` | everything is up to date |

A successful check writes a full per-prefix snapshot, not merely the outdated entries. Failure to write that presentation cache is a warning; it does not change the result of an otherwise successful update check.

### JSON output

`list`, `pinned`, `checkupdate` and `verify` take `--json` and print one JSON document on stdout and nothing else; warnings stay on stderr and exit codes are unchanged. The shape is a contract: every document carries a `schema` number, fields are only ever added within a schema version, and any rename, retype or removal is a schema bump.

```json
{
  "schema": 1,
  "prefix": "/usr/local",
  "checked_at": 1756761600,
  "crates": [
    {
      "name": "hexyl",
      "version": "0.14.0",
      "bins": ["hexyl"],
      "locked": false,
      "pinned": true,
      "status": "outdated",
      "latest": "0.16.0"
    },
    {
      "name": "some-tool",
      "version": "1.3.0",
      "bins": ["some-tool", "some-toolctl"],
      "locked": true,
      "pinned": false,
      "status": "unknown",
      "latest": null
    }
  ]
}
```

`list --json` fields: `prefix` is the absolute, normalized prefix; `checked_at` is the Unix time of the last recorded check, or `null` if there is none; `pinned` mirrors the `pin` state; `status` is one of `up_to_date`, `outdated` or `unknown` (not covered by the last check — installed or updated since); `latest` is the newest version that check found, or `null` when the status is `unknown`. An empty prefix is `"crates": []`, not a message.

`pinned --json` uses the same per-crate shape as `list --json`; without `--check`, it is the pinned subset of the same recorded snapshot, entry for entry — details under [Pinned](#pinned).

`checkupdate --json` prints the snapshot the check just took, with the same `schema`, `prefix` and `checked_at`, and per crate `name`, `current`, `latest` and a derived `outdated` boolean:

```bash
cargo lbin checkupdate --json | jq -r '.crates[] | select(.outdated) | .name'
```

`verify --json` fields: `prefix` and `schema` as above; `crates` is the managed crate count, or `null` when the manifest could not be read or parsed — the count is then unknown, not zero; `errors` and `warnings` are arrays of findings, each with `kind` (a stable machine name such as `binary-missing`, `duplicate-bin-claim`, `manifest-unparseable`, `path-shadow`, `stale-stages`), `message` (the human finding text, repair hint included; the text renderer adds its own `error:`/`warning:` framing), the finding's subjects as data — `crate`, `bin` and `path`, each `null` where the finding has none — and `hint`, the bare pasteable repair command where one is unambiguous (a reinstall for a broken binary), else `null`. A consumer never parses `message`. Only `message` is terminal-sanitized. Data fields retain their original textual values; JSON escaping preserves control characters without laundering the diagnosed value. The document is stdout's only content; on verification errors the exit status is non-zero and the error verdict goes to stderr, as in text mode.


## Update

Update selected crates, or explicitly request all managed crates:

```bash
cargo lbin update hexyl some-tool
cargo lbin update --all
```

A bare `cargo lbin update` is intentionally a usage error. Once individual updates exist, `cargo-lbin` never guesses that "update" means "replace everything".

The update plan is printed and confirmed before anything is built. Skip the confirmation prompt with:

```bash
cargo lbin update --all --yes
cargo lbin update --all -y
```

The same flag works with an explicit crate list.

Each selected crate is an independent unit. A failure is reported and the remaining crates are still processed; successful updates are not undone because a later crate failed. The command exits non-zero whenever fewer updates were applied than were confirmed, including a failed build/placement or a crate skipped because the manifest changed between confirmation and execution.

The last update report is never authoritative for mutation. `update` performs its own fresh checks, and after confirmation it reloads the manifest under the exclusive lock before changing anything.

## Pin

Hold a crate at the version it has:

```bash
cargo lbin pin hexyl
cargo lbin unpin hexyl
```

`install NAME@VERSION` pins as part of installing (see [Install](#install)); `pin` is for a crate already in place.

A pinned crate is left out of `update --all` — listed as `[pinned, skipped]` so the hold is visible, never silent, and not queried at all, so a pinned crate whose lookup fails cannot stop the others from updating — and refused by `update NAME` and by `install NAME` (which would build the newest release, exactly what the pin forbids) until it is unpinned. `install --reinstall NAME` is allowed on a pinned crate and leaves the pin alone: it never leaves the version the pin declares, so there is nothing for the refusal to protect. `migrate` rebuilds a pinned crate at exactly its pinned version, while an unpinned one gets the latest (see [Migrate](#migrate)): the pin is a declaration of version policy, honored wherever a version is chosen, not merely a hold against the next update. `checkupdate` and `list` still check and report a newer version when one exists: the pin is a decision about what to do with that fact, not a reason to hide it. `list` marks pinned crates with `[pinned]`, and a pin set by another process between confirming an update and running it counts as changed state, so that crate is skipped. Removing a pinned crate is allowed; a pin holds a version, not a binary.

Pinning writes the manifest, so it needs the same privilege as installing into the prefix.

The backlog a pin is sitting on has its own read-only view:

```
cargo lbin pinned
```

```
hexyl 0.14.0 -> 0.16.0
update check: 3h ago
```

By default `pinned` reads the last recorded `checkupdate` report, so it is
offline and deterministic; in text output the report age is printed to
stderr, and a crate the report does not cover is listed without an
annotation rather than guessed about. `--json` keeps stdout
machine-clean: one document, no age line; warnings still go to stderr. `--check` asks crates.io about the pinned crates now — and
only about them — without touching the recorded report: the recorded
snapshot belongs to `checkupdate`, and a partial one would misinform
`list`. Exit codes follow `checkupdate`: `0` when a pinned crate is known to have
a newer version, `2` when no pinned crate is known to have one, `1` on
error, so a shell hook or cron job can stay quiet until a held-back
update actually exists. `pinned --json` uses the same per-crate JSON
shape as `list --json`; without `--check`, it is the pinned subset of the
same recorded snapshot, entry for entry.

## Downgrade

Go back to an older version without knowing its number:

```bash
cargo lbin downgrade scx_beerland
```

```text
scx_beerland 1.1.3 is installed; older versions on crates.io:
  1) 1.1.2
  2) 1.1.1
  3) 1.0.9
select a version to install (1-3), or Enter/q to abort: 1
downgrading scx_beerland 1.1.3 -> 1.1.2
...
installed scx_beerland 1.1.2 -> /usr/local/bin (scx_beerland) [pinned; `cargo lbin unpin scx_beerland` to allow updates]
```

The list is crates.io's, filtered by the same release-relevance policy `update` uses — published, not yanked, pre-releases only when the installed version is one — applied to versions older than the installed one. If the crate is removed or its installed version changes while the prompt is open, the command stops rather than applying a choice made against stale state. Newest first, at most ten; if there are more, `install NAME@VERSION` takes any of them. The chosen version is built like any install, with the crate's `--locked` setting carried over, and pinned for the same reason `install NAME@VERSION` pins: a downgrade the next `update --all` would undo is not a downgrade.

The command is interactive on purpose and has no `--yes`; without a terminal it stops and points at `install NAME@VERSION`, which is what a script that knows the version needs. Any answer other than a listed number, an empty line or `q` is an error, and the command can simply be run again.

## Migrate

Move an installed crate to the other prefix — say, promote a personal
install to the system, or the reverse:

```bash
cargo lbin migrate hexyl --user --to /usr/local
cargo lbin migrate hexyl --to ~/.local
cargo lbin migrate --all --to ~/.local --yes
```

The source is the prefix the command addresses, like every other
command (`--prefix`/`--user`/`CARGO_LBIN_PREFIX`); the destination is
always explicit via `--to`. In the TUI, `m` on the selected crate runs
the same operation toward the other prefix of the known pair. The plan is printed and confirmed before
anything is built; `--yes` skips the prompt.

The crate is **rebuilt** at the destination — never copied. The version
follows the pin: a pinned crate is rebuilt at exactly its pinned
version, an unpinned one gets the latest available, because without a
pin the version was never part of the intent — migration preserves
policy, not necessarily version, and a fresh `install` at the
destination would not have resurrected the source's accidental version
either. `--locked` and the pin itself are carried over in both cases.
Copying would be the wrong guarantee: a faithful copy faithfully
promotes whatever the binary has become since it was installed, and
promoting `~/.local` to `/usr/local` is exactly where that matters.
Rebuilding re-establishes provenance through the same pipeline as
`install`. It costs a compilation; that is the price of knowing what
was placed. The printed plan says per crate which contract applies, and
the success line reports the version the destination actually
committed.

The source entry is retired only after the destination has fully
committed. `migrate` never waits on one prefix while holding a lock on
the other, and never holds two exclusive locks; the only overlap is a
nonblocking shared probe of the source right before the destination
commits. If the source changes while the destination is building, the
migration aborts before the destination commits anything; anything that
goes wrong *after* that commit — the source changed, or its retirement
itself failed — is reported as an incomplete migration whose message
says the essential thing up front: the destination installation stands,
do not re-run blindly (it would be refused), resolve the problem and
`remove` the source installation. That message is the durable record.
The `[also in …]` annotation additionally shows a crate present on both
sides, but only for the known pair of prefixes (`/usr/local` and
`~/.local`) — for a custom `--to`, the listing of either prefix cannot
see the other, so keep the command's output; the annotation is a bonus
where it exists, not the guarantee. No failure or crash ever leaves you
without one complete working installation: the destination commits
fully before the source loses anything. A crash mid-retirement can
leave the source partial — its manifest entry still recorded, some
binaries already gone — and a plain `remove` on the source cleans up
such a remainder.

A crate already installed at the destination is refused; there is no
`--force`. `migrate` will not overwrite an existing installation with
the migrating one — remove the wrong side first, then migrate.

Like `update`, a batch reports each failure and moves on, and the
command exits non-zero whenever fewer migrations completed than were
confirmed — a crate left in both prefixes is a *safe* shortfall, but a
shortfall.

The destination follows the same escalation policy as everything else:
`sudo` is offered only for the canonical `/usr/local`; any other
destination must be writable by the invoking user. Note the timing when
migrating *away* from `/usr/local`: the destination is user-writable, so
the one privileged step is retiring the source — the password prompt can
therefore appear only at the end, after the build. A declined or failed
prompt degrades to an incomplete migration with the destination intact.

## Verify

Ask whether the managed state is healthy:

```bash
cargo lbin verify
```

```
error: `foo`: managed binary /home/user/.local/bin/foo is missing — reinstall: cargo lbin install --reinstall foo --prefix=/home/user/.local
warning: 2 stage directories under /home/user/.cache/cargo-lbin with no live owner — possible leftover build debris; inspect, then `cargo lbin clean --stages` when safe (for pre-lease stages the owner test is a PID heuristic: a PID can be reused, and an orphaned build may still hold the directory)
```

The manifest is the source of truth; every command relies on that,
`verify` is the one that checks it. lbin's writes are atomic where it
matters — the manifest and binary placement — but a multi-step
operation can still crash between its steps, nothing stops a hand from
removing a managed binary, and a system package can shadow one — and
the messages that report such states scroll away. `verify` is the
standing query those states were missing.

It is read-only in the strictest sense: it does not even prepare the
lock. A shared lock is taken only where one already exists — never
created, along with nothing else — and a prefix that has no lock yet is
read without one, which the atomic manifest placement keeps safe from
torn files; when someone else's operation holds the lock, the wait
says so on stderr rather than looking like a hang. There is no `--fix`
by design — a finding names the
existing repair command where lbin has an unambiguous one, and
otherwise describes the state and leaves the decision to you; the
repair commands already own the locks, confirmations and privilege
rules a repair needs, and a second mutating path would buy one saved
keystroke for a whole new surface of failure modes. A named command is
spelled to be pasteable and true: it carries the audited prefix as
`--prefix=<path>` (the default is `/usr/local`, and your shell may carry
its own `CARGO_LBIN_PREFIX`), shell-quoted when the path needs it, the
pinned version when there is one, and `--locked` when the entry was
built with it — pasteable means pasteable, including over a prefix with
a space or an apostrophe in its name. A prefix whose name has no honest
shell spelling at all (not UTF-8, or holding a control character) gets
its findings without a command: a command that is safe to paste but
names a different path would be the one lie worse than none. And a
command is named only when
it would actually run: one broken entry anywhere makes every lbin
command refuse the whole manifest, so a manifest with any structural
finding gets its disk findings without commands — repaired manifest
first, reinstalls second.

Findings come in two severities, and the split is the contract.
**Errors** are broken invariants or claims that could not be verified —
the manifest says something the disk contradicts, or something the disk
would not even answer about: an entry that does not parse, a declared binary
missing, of the wrong type — a symlink counts, even one that resolves
to a healthy executable: lbin places regular files, and a symlink under
a managed name is structural drift — or not executable, or one binary
name claimed by two entries (a state lbin never writes itself; the finding
names no repair command: lbin's own commands begin by loading the
manifest, which refuses exactly these states, so the honest remedy is
repairing the file by hand or restoring it from a backup — and that is
what every validate-class finding says). A manifest that does
not deserialize at all is reported as the audit's one finding, and a
binary that cannot be inspected — permissions, I/O — is reported as
exactly that, never as "missing". Any error makes the exit status
non-zero. **Warnings** observe the surroundings while the managed state
itself is healthy: the crate also installed under the other known
prefix (legal by construction — `verify` does not know the history and
does not guess it), another executable with a managed binary's name on
`PATH` — whichever side resolves first, that is the one check that
notices drift *between* operations — and stage directories with no
live owner (kept deliberately as crash forensics; listed here because
nothing else ever lists them). For leased stages the owner is a kernel
lock — `verify` briefly takes and immediately releases a *shared*
lease, solely to determine liveness, and only a released lease is
named — while pre-lease stages keep the PID heuristic and its caution:
PIDs can be reused, so inspect before removing. Warnings alone exit
zero, so a deliberate `PATH` or cache debris cannot turn a healthy
prefix red in a script.

`verify` checks structure, not integrity: that an executable file
answers to every claimed name — `pacman -Qk`, not `-Qkk`. The manifest
records no hashes, by the same decision that makes [`migrate`](#migrate)
rebuild rather than copy: lbin does not attest that today's bytes are
the bytes it placed.

In the TUI, `v` runs the same audit in the build panel's shape; the
findings land in a panel that stays up until dismissed — and scrolls,
because a durable record whose tail is unreachable is not one. The
availability is the CLI's, including on a manifest the loader refuses:
the session starts degraded rather than dying — empty list, mutating
actions refused, `r` retrying the load, `B` still switching prefixes —
so the key that explains what is wrong stays reachable exactly when
something is. Degraded is a state the whole UI renders, not a message
that scrolls away: the footer says "managed crate count unavailable"
instead of an invented zero — the CLI's `Option` honesty, kept — and
its key bar advertises only what still works. One honest note on
scope: the `v` audit itself never writes, while a TUI *session*, like
every TUI session, prepares the state lock when it starts — strict
read-only-ness belongs to `cargo lbin verify`, the command.

`verify --json` emits the findings as one JSON document — stdout's only content (schema below). The exit status is the text mode's; on verification errors the error verdict goes to stderr, and a clean run prints nothing but the document.

## Clean

The mutating half of the pair `verify` opens: `verify` names cache debris read-only, `clean` removes it — through the very same liveness test, so the two can never disagree about what debris is. Every removal is opt-in — a mutating command does nothing it was not explicitly asked to do, and bare `clean` is an error. `--stages` removes the stage directories `verify` reports as having no live owner. For leased stages (the 0.13 layout) that scan is only candidate selection: the removal license is taking the stage's own lease exclusively, held through the whole delete, so a build still writing — even one orphaned by its cargo-lbin — vetoes the removal with its inherited lock, and a lease held at removal time defers that stage to a later pass (reported as deferred, not removed and not failed). For pre-lease stages ownership is still a heuristic, not proof: the owning cargo-lbin process is gone (a dead PID, or a name that is not a PID at all), but a build it spawned may survive it and still hold the directory — which is why stage removal is explicit and never a default. `--logs-older-than DAYS` removes failure logs past that age; the person names the retention, lbin does not invent one. Unlike `verify` (read-only, silent over an unreadable cache), `clean` refuses to report success over a cache it could not read: a missing directory is an empty one, any other read error is an error. `--dry-run` lists what would go and removes nothing. The cache is the user's own: the prefix state lock is not taken, no sudo is ever used, and a stage owned by a live run — on any prefix — is spared by the liveness test itself. Failed removals are reported and the command exits non-zero; `nothing to clean` exits zero.

## Remove

Remove one or more managed crates:

```bash
cargo lbin remove hexyl
cargo lbin remove foo bar
```

`cargo-lbin` removes only binaries recorded as belonging to the selected managed crate.

## Shell completion

Completion scripts are generated from the same Clap definition as `--help`, so there is no separate handwritten command specification to maintain. The script is a snapshot of the CLI as of the version that generated it: regenerate it after upgrading `cargo-lbin` to pick up CLI changes.

```bash
cargo lbin completions bash       > ~/.local/share/bash-completion/completions/cargo-lbin
cargo lbin completions zsh        > ~/.zfunc/_cargo-lbin            # with ~/.zfunc in fpath
cargo lbin completions fish       > ~/.config/fish/completions/cargo-lbin.fish
cargo lbin completions elvish     > ~/.config/elvish/lib/cargo-lbin.elv   # then `use cargo-lbin`
cargo lbin completions powershell > $HOME\cargo-lbin.ps1              # then `. $HOME\cargo-lbin.ps1` in $PROFILE
```

Each of these writes a file that the shell loads; regenerating after an upgrade overwrites it. (Appending to `$PROFILE` directly would add a second copy on every regeneration — hence the separate `.ps1` that the profile dot-sources.)

Static CLI completions only: subcommands, flags and known values (such as the shell names above). They are generated entirely from the command definition and never inspect the installation prefix, so installed crate names are deliberately not completed. The script completes the `cargo-lbin` binary; `cargo-lbin <Tab>` always works, while `cargo lbin <Tab>` depends on whether your cargo's own completion delegates to external subcommands.

## TUI

The TUI is an interactive frontend over the same core operations:

```bash
cargo lbin tui
```

```text
┌ cargo-lbin — /usr/local ──────────────────────────────────┐
│ Packages (3)  Updates (1)  Pinned (1)                     │
├───────────────────────────────────────────────────────────┤
│ NAME             VERSION       STATUS                     │
│ > ripgrep        14.1.1        ✓ up to date               │
│   bat            0.26.0        ↑ 0.26.1                   │
│   fd [pinned]    10.2.0        ↑ 10.3.0                   │
├ Selected ─────────────────────────────────────────────────┤
│ Crate      bat                                            │
│ Installed  0.26.0                                         │
│ Latest     0.26.1                                         │
│ Binaries   bat                                            │
│ Locked     no                                             │
│ Pinned     no                                             │
│ Also in    /home/user/.local @0.25.0                      │
├───────────────────────────────────────────────────────────┤
│ ↑/↓ select · Tab filter · Enter/u update · U update all … │
│ 3 packages · 1 updates · 1 pinned (1 behind) · checked 3h │
└───────────────────────────────────────────────────────────┘
```

The TUI starts entirely from disk — the manifest and the last `checkupdate` report. It performs no refresh, network request, update or installation on startup.

The Selected panel is the crate's full managed entry: installed version, `Latest` from the last check, every binary, and `Locked`/`Pinned` answered explicitly — in a details panel a missing line would read as "unknown", not as "no". Each copy under another known prefix appears as its own `Also in` line with its version — with a custom current prefix there can legally be two. Deliberately absent: lease and staging state (build plumbing, not crate state — `verify` and `clean` own it) and the full version history (`info --versions` is the archaeology; the panel's `Latest` is the decision surface).

Installing a crate another prefix already manages emits the same warning the CLI prints (see [Install](#install)) — the decision comes from the same code on both surfaces, so they cannot disagree. The TUI shows it in the panel as soon as it arrives and keeps it available while the build continues (arrows scroll it, `Esc`/`Enter` dismisses it); `c` still cancels. It informs and never blocks — and nothing waits on the display, so a build fast enough to finish in one breath may go straight to its final panel with the warning in it.

| Key | Action |
| --- | --- |
| `↑` / `↓`, `j` / `k` | Move selection |
| `g` / `G`, `Home` / `End` | First / last row |
| `Tab` | Cycle Packages, Updates and Pinned (Shift-Tab cycles back) |
| `Enter`, `u` | Update the selected crate |
| `U` | Run a fresh `update --all` |
| `i` | Open the install line (`NAME[@VERSION]... [--locked]`; `@VERSION` pins) |
| `x` | Remove the selected crate after TUI confirmation; in place unless removal needs `sudo` |
| `m` | Migrate the selected crate to the other prefix (asks first; known pair only) |
| `M` | Migrate every crate to the other prefix (asks first; `c` cancels the batch) |
| `B` | Jump to the other prefix of the known pair; the selection follows the crate |
| `c` | Cancel the running operation: for a build, a second `c` sends SIGKILL; for `r`/`v`/`s` and `D`'s version lookup, a cancel request — the update check stops between requests, a verify, search or lookup result is discarded on arrival |
| `p` | Pin or unpin the selected crate; in place unless pinning needs `sudo` |
| `D` | Downgrade the selected crate: the panel offers the older versions, a digit installs one |
| `v` | Verify the prefix; findings open in the report panel |
| `r` | Run `checkupdate` and refresh the saved report |
| `s` | Search crates.io by keyword |
| `1`..`9` | With search results open, pick a visible hit into the install line; with a downgrade offer open, install that version |
| `?` | Show help |
| `q`, `Esc` | Quit from the package list; `Esc` also dismisses transient views/input |
| `Ctrl-C` | Quit; with a build running, cancel it first and leave once it stops |

Search and update checks run without freezing the list. Search hits are displayed in the details panel; installed hits are marked, and pressing a digit opens the normal install input with that crate name, still editable so `--locked` can be added.

The saved update report is presentation only. `U` always invokes a real `update --all` and lets that command compute a fresh plan, even if the TUI currently shows zero cached updates.

The Updates tab shows what `update --all` will act on, so a pinned crate
with a newer version is not listed there — `update --all` would skip it,
and a count that promises updates it will not perform is a count that
lies. The held-back update is not hidden: it lives in the Pinned tab,
whose count sits in the header at all times, and the footer says how many
pinned crates are behind. The same split shapes the `r` result line:
`checked: 1 update(s) available; 1 pinned held back`.

A single-crate install builds in place, inside its own transient Build panel, and a removal or a pin flip runs in place too when it needs no password; `sudo` is asked for at placement rather than before the build, and the interface steps aside for that prompt; a migration out of a privileged prefix into a writable one escalates only after the build — the privileged half is retiring the source — and says so before the build starts, so a prompt arriving then is expected rather than startling (whether `sudo` asks at all depends on its own timestamp, so the notice promises the escalation, not the prompt); the CLI says the same before each crate's build; batch installs, `update` and — when that operation needs `sudo` — `remove` and `pin`/`unpin` temporarily hand the real terminal back to the normal CLI, where Cargo diagnostics, the update confirmation and password prompts behave exactly as they do outside the TUI, and the interface returns afterwards.

`D` offers the older versions in the panel: a cancellable lookup, then a numbered list — the same choice `cargo lbin downgrade` makes, so a browser of the whole history it is not — and a digit installs one. The list stops at nine because a digit names one entry; anything older is announced with a pointer to `install NAME@VERSION`. The build that follows is an ordinary in-panel install of an exact version: `c` cancels it, its warnings land in the panel, and the version is pinned for the same reason the command pins. The offer is computed against the version the row shows, so if the crate moves or disappears while the list is open, the digit starts nothing and says why.

An in-place build can be cancelled: `c` sends SIGTERM to cargo's whole
process group — every rustc and build script included — and a second `c`
escalates to SIGKILL. If the group has not stopped within about two
seconds of the first cancel, SIGKILL follows automatically: a group
member holding the build's output pipe can wedge the worker inside a
read — a partial line with no newline is enough — so the interface does
not depend on the worker to finish the job.

The one-shot jobs (`r`, `v`, `s`, and `D`'s version lookup) have their own, simpler door: `c`
requests cancellation — the update check stops between index requests,
a verify, search or version lookup runs to completion and its result is discarded on
arrival — and the same door works in the degraded state, where `v` is
the natural first move.

`m` migrates the selected crate to the other prefix of the known pair —
`/usr/local` from `~/.local` or the reverse — after a confirmation that
names the crate, the version, both prefixes and which contract applies:
a pinned crate is rebuilt at exactly its pinned version, an unpinned one
gets the latest, as [`migrate`](#migrate) defines. It is a frontend to
`migrate`, not a second implementation: the rebuild runs at the
destination inside the same Build panel, `c` cancels it
through the same door (a cancel before placement is a complete no-op —
neither prefix is touched), and once placement begins cancellation no
longer interrupts it: the migration proceeds through the retirement
attempt, and anything that fails past that point is reported as an
incomplete migration, never silently. The plan is frozen when you press
`m`: what the confirmation shows — name, version, pin, both prefixes —
is exactly what is revalidated before anything is retired, and a crate
that changed in the meantime is refused rather than migrated under a
plan you never confirmed. The shown version is the source state that
guard holds the migration to; what the destination receives follows the
pin, and the success note reports the version it actually committed. An
incomplete migration — the
destination committed, the source not retired — is shown in a wrapping
panel with the full explanation, because that message is the durable
record. With a custom `--prefix` the "other side" stops being a
function, so the TUI offers no path picker; the message points to the
CLI's explicit `--to`.

`B` jumps the whole interface to the other prefix of the known pair —
the same gate as `m` and `M`, one line pointing at `--prefix` anywhere
else. The list, the report age and the `[also in …]` annotations all
reload for the other side, and the selection follows the currently
selected crate by name when it is visible there under the current
filter; otherwise it falls back to the top. The jump refuses while an
operation is running or queued, and commits only when the other side's
manifest actually reads.

`M` is `migrate --all` in the same shape: the whole plan is frozen at
the keypress, confirmed once, and executed as a queue of the very same
single migrations `m` runs — each crate its own unit of work, so a
member's failure is tallied and the batch moves on, exactly like the
CLI. The plan is frozen from a fresh view of the prefix at the
keypress. The summary reports how many migrated; failures, incomplete
migrations and build warnings land in one report panel with their full
reasons. `c` cancels the current crate and ends the batch — the
remaining queue is dropped, never silently continued — and the summary
says how many were never attempted. A worker that cannot start at all
(a failed preflight, an uncooperative sudo) ends the batch the same
way, with the refusal recorded among the failures. Placement is the
exception: once binaries start moving into the prefix, a cancel is too
late — an install finishes placing, and a migration proceeds through
its retirement attempt — because killing `sudo install` between two
binaries is not an option, and placement is seconds, not minutes.
`Ctrl-C` keeps its traditional meaning of "quit", but with a build
running it cancels first and leaves only once the
worker has been collected, so cargo is never orphaned behind a dead
pipe. A cancelled build reports as one line — it is the outcome that was
asked for, not a failure.

## Prefixes and cache

The default prefix is `/usr/local`:

```text
/usr/local/bin
/usr/local/share/cargo-lbin/manifest.json
/usr/local/share/cargo-lbin/lock
```

The per-user prefix has a first-class flag — `--user` is an alias for
`--prefix ~/.local`:

```bash
cargo lbin --user install hexyl
cargo lbin --user list
```

Binaries land in `~/.local/bin` (on most distributions already on
`PATH`), state in `~/.local/share/cargo-lbin` — the default XDG user
data location — the same tree as `/usr/local`, owned by the user, and
`sudo` is never used. Any other custom prefix works via `--prefix`:

```bash
cargo lbin --prefix "$HOME/tools" install hexyl
```

Or set one once for every invocation:

```bash
export CARGO_LBIN_PREFIX="$HOME/.local"
cargo lbin install hexyl
cargo lbin list
```

Prefix precedence is:

```text
--user | --prefix > CARGO_LBIN_PREFIX > /usr/local
```

`--user` with an explicit `--prefix` is an error (two explicit answers
to one question); `--user` over an exported `CARGO_LBIN_PREFIX` wins
silently — an alias exists to be typed ad hoc, and ad hoc beats ambient
configuration.

### Cross-prefix awareness

With more than one prefix in use, `list` (and the TUI) annotates crates
installed in the *other* known prefixes:

```text
hexyl 0.17.0 [also in /usr/local @0.17.0] (hexyl)
```

The set of prefixes is finite and closed — `/usr/local` and `~/.local` —
and only cargo-lbin's own manifests are consulted: this is a "you also
installed this over there" reminder, not a `PATH` scanner (installed
binaries shadowed by anything else on `PATH` are reported at install
time). Foreign manifests are read without taking their lock: the
manifest is replaced atomically, so a lockless read always sees a
complete document, and a listing must never wait behind someone's build
in a prefix it was not even asked about. In `--json` output the same
information is the additive, optional `also_in` field, omitted when
empty — schema 1 documents for a single-prefix system are byte-identical
to pre-0.8 ones.

A prefix is the *parent* of `bin`, so `--prefix /usr/local` places binaries in `/usr/local/bin`. Passing the `bin` directory itself — `--prefix ~/.local/bin`, or `migrate --to ~/.local/bin` — therefore installs into `~/.local/bin/bin`, which is legal and occasionally even intended; `cargo-lbin` says so once, names the directory the binaries would land in and the parent it suspects you meant, and proceeds. A warning, never a refusal.

Custom prefixes must be writable by the invoking user. `cargo-lbin` only permits privilege escalation for the canonical `/usr/local` prefix; it will not use `sudo` to write into an arbitrary custom path.

Build staging and the update-report cache live under:

```text
$XDG_CACHE_HOME/cargo-lbin/
```

or, when `XDG_CACHE_HOME` is unset:

```text
~/.cache/cargo-lbin/
```

Builds use per-run staging directories under `stage-v2/`, named `<pid>-<nonce>` and owned through a `.lease` file the creating process locks (`flock(2)`) and every spawned child inherits — so independent operations cannot wipe each other's stages, ownership survives PID reuse, and a build orphaned by its cargo-lbin keeps its stage visibly owned until it exits. Pre-0.13 stages under `stage/` are still recognized by their PID-heuristic rules; the two layouts do not share a directory, so a concurrently installed 0.12 binary cannot sweep live leased runs.

The last `checkupdate` snapshot is stored under `checkupdate/`, one file per normalized prefix. Relative prefixes are anchored to the current working directory before that cache key is derived, so `--prefix local` in two different directories refers to two different prefix states. The snapshot is presentation-only state: it has no expiry, only `checkupdate` refreshes it, and it can be deleted at any time.

## Update rules

`checkupdate`, `info` and `update` use crates.io release data with the same eligibility rules:

- Yanked releases are never offered as updates.
- Stable installations are offered stable releases only.
- An installed pre-release may move to a newer pre-release or to a stable release. Once installed on a stable version, only stable releases are offered again.
- Versions are compared with SemVer.
- The version written to the manifest is the version Cargo actually built, read from Cargo's staging metadata rather than assumed from an earlier check.
- If a new release stops shipping one of a crate's previously managed binaries, the obsolete binary is removed during the update.

Network errors are reported rather than silently producing an incomplete update list.

`search` is intentionally different: it is a discovery view backed by the crates.io search API, not the sparse-index update resolver.

## Safety model

`cargo-lbin` is intentionally conservative because it builds third-party crates and may later perform a small number of filesystem operations with elevated privileges.

It does **not** sandbox Cargo. Installing a crate means trusting code that Cargo may execute as your user, including build scripts and proc macros. The safety model is about keeping that unprivileged build environment from being accidentally promoted into privileged filesystem access during placement.

### Builds never run through sudo

Crates are built first, as the invoking user, in an isolated staging root. Only placement into a protected `/usr/local` destination may trigger `sudo`.

Credentials are collected where they are spent: at the placement door,
after the build. A build is unprivileged work, so paying for it in
advance would mean paying for builds that fail — and holding a warm
credential for the whole length of every build that does not. `sudo -n
-v` checks the timestamp silently there, and only when sudo would
prompt does `cargo-lbin` announce why and run `sudo -v`. The TUI steps
off its screen for that prompt; the CLI asks in place.

One thing cannot wait: preparing a prefix's lock file the first time
anything is installed there. That is the first privileged touch and it
happens before the build, so a cold sudo is asked then — once per
prefix, since the lock file is world-readable once made. On a sudo
configured not to cache credentials at all, the refusal now arrives
after the build instead of before it; that is the cost of not holding
a password across work that does not need one.
`cargo-lbin` never reads, buffers or forwards the password itself: the
prompt, echo, retries, PAM and the credential cache are `sudo`'s business
alone. This is a design rule, not an implementation detail — a password
field will not be added to the TUI.

Running the whole tool as root is rejected:

```bash
sudo cargo lbin install foo
# error: cargo-lbin must not be run as root
```

For root-only containers or CI environments, the guard can be explicitly overridden:

```bash
CARGO_LBIN_ALLOW_ROOT=1 cargo lbin ...
```

### Existing files are not silently overwritten

Before placement, every destination name is checked.

A binary may be replaced only when the manifest already records it as belonging to the same crate. A name owned by another managed crate, or an unmanaged file already present at the destination, causes the operation to fail without clobbering it.

The manifest also enforces that every binary name has exactly one owning crate.

### Privileged commands use trusted paths

Operations that may run through `sudo` invoke trusted system tools by absolute path rather than resolving them through the user's `$PATH`. A build script therefore cannot place a fake `sudo`, `install`, `mv` or similar executable earlier in the path and have the privileged placement code execute it.

### Staged binaries are pinned before privileged placement

A staged binary is opened with symlink following disabled and verified as a regular file owned by the invoking user. Privileged placement copies the already-open file descriptor through `/proc/<pid>/fd/...`, rather than trusting a stage pathname that could be swapped after validation.

Binaries are installed atomically through a temporary file in the destination directory followed by rename. An interrupted replacement therefore does not leave half of a new executable in place of the old one.

### The manifest is validated, sealed and committed atomically

State lives at:

```text
<prefix>/share/cargo-lbin/manifest.json
```

The manifest is treated as untrusted input whenever it is loaded. Crate names, binary names, versions, duplicate ownership and path safety are validated before state can steer filesystem operations.

When writing state, `cargo-lbin` serializes the manifest into an anonymous Linux `memfd`, seals it against further modification, reads the sealed bytes back for verification, and only then hands the descriptor to the placement path. The final manifest is installed through a same-directory temporary file and atomic rename.

In short: `cargo-lbin` does not knowingly write manifest state that it would refuse to read back later.

### crates.io data is treated as untrusted input

External registry metadata is normalized before it reaches the CLI or TUI. Control characters are replaced with spaces and whitespace is normalized at the API boundary, returned crate names are validated with the same rules as names supplied by the user, and search-result limits are enforced locally rather than trusted solely to the server.

### Concurrent instances are serialized per prefix

Each prefix has a state lock:

```text
<prefix>/share/cargo-lbin/lock
```

Mutating operations (`install`, `update`, `remove`) take an exclusive lock. Readers take a shared lock only around the manifest snapshot where possible; network queries and the interactive update prompt do not hold the lock.

`update` reloads and verifies state after confirmation before mutating it, so changes made by another `cargo-lbin` while the prompt was open are not acted on blindly.

### Partial failures stay recoverable

Crates are committed to the manifest one successful operation at a time. A failure later in a batch does not erase earlier successful work.

For multi-binary installs and updates, newly introduced binary names placed before a manifest commit are removed on a best-effort basis if that operation fails. Existing names remain associated with the previous manifest entry and can be safely replaced on retry.

A hard kill or power loss in the narrow window between placing a new binary name and committing the manifest can still leave an unmanaged orphan. `cargo-lbin` then refuses to overwrite it and asks you to remove the leftover manually before retrying. A persistent transaction journal is intentionally out of scope.

## SELinux

After placement, `cargo-lbin` runs `restorecon` on installed binaries when it is available in a trusted system location. This is best-effort; systems without SELinux tooling simply skip the step.

## Requirements

`cargo-lbin` currently targets Linux, and assumes an FHS-style hierarchy —
in particular a mutable `/usr/local` alongside distribution-owned
`/usr/bin`. Distributions that do not follow the FHS (NixOS being the
canonical example: an immutable store, no populated `/usr/local`, nothing
that would put one on `PATH`) are unsupported; a custom `--prefix` may
happen to work there, but working by accident is not the same as being
supported.

- Rust/Cargo **1.91 or newer** to build the tool.
- A working Cargo setup.
- Network access to crates.io for installs, searches, exact info lookups and update checks.
- `/proc` mounted and available for descriptor-based placement.
- `sudo` when the canonical `/usr/local` destination is not writable by the invoking user.
- Standard GNU/Linux userland tools in their conventional trusted locations (`sudo`, `install`, `rm`, `mkdir`, `touch`, `mv`, `chmod`).

The implementation is developed with conventional Arch Linux and Fedora-style layouts in mind.

## Non-goals

Keeping the scope small is deliberate. `cargo-lbin` does not try to become another Cargo or a distro package manager.

It currently does **not** provide:

- Git or local-path sources (`--git`, `--path`).
- Copying binaries between prefixes. `migrate` rebuilds, permanently: a faithful copy can promise "the bytes placed are the bytes opened", but not that those bytes are still the artifact the manifest entry describes — and promoting `~/.local` to `/usr/local` is exactly where that difference matters.
- Privileged installation into arbitrary custom prefixes.
- Management of libraries, headers, systemd units, configuration files or other distro integration.
- A dependency resolver of its own — Cargo remains responsible for builds and dependencies.
- Automatic background checks, updates or a daemon. The TUI also never refreshes, searches or updates anything on its own.
- Security advisories. Vulnerabilities live in the dependencies compiled into a binary, and `cargo-lbin` records only the crates it installed, not their dependency graphs; an "audit" of the installed names alone would report clean binaries it had not looked inside. Doing it properly would mean capturing each build's resolved dependency set, keeping an advisory database and matching version ranges — a second tool grafted onto this one. Use `cargo audit` in a source tree with an appropriate `Cargo.lock`; `cargo-lbin` does not claim to audit the dependencies compiled into its installed binaries, and a crate's published lockfile need not match what a build without `--locked` resolved.

For a normal per-user `~/.cargo/bin` workflow, plain `cargo install` remains the simpler tool.

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT License

at your option.
