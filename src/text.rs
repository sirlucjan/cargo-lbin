//! External text headed for a `Span` is data, never terminal control —
//! one rule for registry metadata, build output and subprocess stderr
//! alike. The plain CLI mostly still prints paths raw, as it always
//! has, with one deliberate exception: `verify`'s findings are
//! sanitized at the `VerifyReport` boundary for both renderers at once,
//! because they carry manifest content from before validation.
//! Extending the rule to the rest of the CLI is future work, and this
//! comment refuses to promise it early.

/// Control characters (ESC, BEL, CR games) become spaces; everything
/// else passes through.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
