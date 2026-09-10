//! Parsing of cargo's line-oriented stderr into progress events a
//! frontend can render. With stderr piped (not a tty), cargo drops its
//! own progress bar and usually its colors too — and the captured build
//! forces `CARGO_TERM_COLOR=never` for the configurations where "usually"
//! is not enough — so the parser is a prefix match on trimmed, plain
//! lines by contract, not by luck.

/// One parsed line of `cargo install` stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildEvent {
    /// `   Compiling serde v1.0.219` — also `Checking` and `Building`,
    /// which appear for check builds and pipelined backends; all three
    /// advance the counter, because all three mean "one unit started".
    Compiling { name: String, version: String },
    /// `  Downloading crates ...` / `   Downloaded serde v1.0.219` —
    /// the pre-build phase; the gauge shows activity but does not count.
    Downloading,
    /// ```text
    ///     Finished `release` profile [optimized] target(s) in 12.3s
    /// ```
    Finished,
    /// `error[E0308]: ...` or `error: ...` — the tail buffer becomes
    /// interesting from the first of these.
    Error,
    /// Everything else: warnings, notes, `Installing`, blank lines.
    /// Logged, not displayed.
    Other,
}

/// Classify one stderr line. Lines are matched after trimming leading
/// whitespace, because cargo right-aligns its verb column.
pub fn parse_line(line: &str) -> BuildEvent {
    let t = line.trim_start();
    for verb in ["Compiling ", "Checking ", "Building "] {
        if let Some(rest) = t.strip_prefix(verb) {
            // `name vX.Y.Z (path)` — name and version are the first two
            // whitespace-separated fields; the version keeps its `v`.
            let mut it = rest.split_whitespace();
            if let (Some(name), Some(version)) = (it.next(), it.next()) {
                return BuildEvent::Compiling {
                    name: name.to_owned(),
                    version: version.to_owned(),
                };
            }
            return BuildEvent::Other;
        }
    }
    if t.starts_with("Downloading") || t.starts_with("Downloaded") {
        return BuildEvent::Downloading;
    }
    if t.starts_with("Finished") {
        return BuildEvent::Finished;
    }
    if t.starts_with("error[") || t.starts_with("error:") {
        return BuildEvent::Error;
    }
    BuildEvent::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiling(name: &str, version: &str) -> BuildEvent {
        BuildEvent::Compiling {
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    #[test]
    fn verbs_advance_the_counter() {
        assert_eq!(
            parse_line("   Compiling serde v1.0.219"),
            compiling("serde", "v1.0.219")
        );
        assert_eq!(
            parse_line("    Checking anyhow v1.0.99"),
            compiling("anyhow", "v1.0.99")
        );
        assert_eq!(
            parse_line("   Building foo v0.1.0 (/tmp/stage)"),
            compiling("foo", "v0.1.0")
        );
    }

    #[test]
    fn phases_and_errors_classify() {
        assert_eq!(parse_line("  Downloading crates ..."), BuildEvent::Downloading);
        assert_eq!(
            parse_line("   Downloaded serde v1.0.219"),
            BuildEvent::Downloading
        );
        assert_eq!(
            parse_line("    Finished `release` profile [optimized] target(s) in 12.34s"),
            BuildEvent::Finished
        );
        assert_eq!(
            parse_line("error[E0308]: mismatched types"),
            BuildEvent::Error
        );
        assert_eq!(parse_line("error: could not compile `foo`"), BuildEvent::Error);
    }

    #[test]
    fn noise_stays_noise() {
        assert_eq!(parse_line(""), BuildEvent::Other);
        assert_eq!(parse_line("warning: unused variable"), BuildEvent::Other);
        assert_eq!(
            parse_line("  Installing /tmp/stage/bin/foo"),
            BuildEvent::Other
        );
        // A crate whose *name* mentions a verb must not confuse the
        // prefix match — the verb column is at line start after trim.
        assert_eq!(
            parse_line("   Compiling compiling-tools v1.0.0"),
            compiling("compiling-tools", "v1.0.0")
        );
    }
}
