//! The TUI's batch machinery: shared tally and reporting state, the
//! per-verb sections that classify member outcomes, and migration's
//! queue and stopping state. The crate stays the unit of execution
//! and commit; a batch is only an iteration policy above it.
//! The loop order that makes cancellation sound belongs to `App::run`
//! in mod.rs, not here.

use super::App;
use super::state::PendingAction;
use std::collections::VecDeque;
use std::path::PathBuf;

/// A confirmed migration on its way to `start_migrate`; the
/// destination preflight may need the terminal.
pub(super) struct PendingMigrate {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) dest: PathBuf,
    pub(super) snap: crate::MigrationSnapshot,
}

/// What an install batch's members can end up being. Fewer than a
/// migration's: an install has no half-done outcome to name — a crate
/// is installed or it is not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum InstallSection {
    Failed,
    /// A sweep's own outcome: the plan was confirmed about an entry
    /// that has since moved. Neither built nor broken.
    Skipped,
    /// Built, and then the placement door could not be opened. Not a
    /// failure to build, and the panel must not imply one.
    Refused,
    Noticed,
}

/// `i a b c`: the presentation of a batch whose execution belongs to
/// its worker.
///
/// The queue is not here. One operation holds one lock across every
/// member, so the order of execution is the worker's — and a runner
/// pretending to hand out members would be describing something that
/// is not happening. What is here is what the person sees: which
/// member is running, what each had to say, and the summary.
pub(super) struct InstallBatch {
    pub(super) runner: BatchRunner<(), InstallSection>,
    /// Which shape of batch this is. It selects the vocabulary of the
    /// summary; list and sweep share the same iteration policy.
    pub(super) shape: BatchShape,
    /// The member whose lines are arriving now, if one is building.
    /// `None` before the first and between members — which is exactly
    /// when a failure belongs to the plan rather than to anyone.
    pub(super) current: Option<String>,
    /// Members that got as far as starting. Counted here rather than
    /// derived from the sections, because a plan refused before the
    /// first build has no failed member and no attempt either.
    pub(super) attempted: usize,
    /// Warnings spoken before any member started: the duplicate check
    /// runs over the whole plan, and its lines belong to no crate.
    pub(super) plan_warnings: Vec<String>,
}

impl InstallBatch {
    /// A member is starting: anything said before it belongs to the
    /// plan, not to it.
    pub(super) fn begin_member(&mut self, name: &str, said_before: Vec<String>) {
        // Before *any* member, not merely between two: `current` is
        // also None in the gap, and a warning spoken there would belong
        // to whoever spoke it, not to the plan.
        if self.attempted == 0 {
            self.plan_warnings.extend(said_before);
        }
        self.current = Some(name.to_owned());
        self.attempted += 1;
    }

    /// A member is done, with its own warnings and its own tail.
    ///
    /// The same rules a single build follows: warnings survive a
    /// success and are dropped on a failure — a rollback removed the
    /// binaries they described — and a terse failure gets the tail.
    pub(super) fn finish_member(
        &mut self,
        name: &str,
        outcome: &crate::MemberOutcome,
        warnings: Vec<String>,
        tail: &VecDeque<String>,
    ) {
        match outcome {
            crate::MemberOutcome::Installed => {
                self.runner.succeeded();
                if !warnings.is_empty() {
                    self.runner.record(InstallSection::Noticed, name, warnings);
                }
            }
            crate::MemberOutcome::Failed(e) => {
                self.runner
                    .record(InstallSection::Failed, name, App::failure_lines(e, tail));
            }
            crate::MemberOutcome::Refused(reason) => {
                self.runner.record(
                    InstallSection::Refused,
                    name,
                    vec![crate::text::sanitize(reason)],
                );
            }
            crate::MemberOutcome::Skipped(reason) => {
                self.runner.record(
                    InstallSection::Skipped,
                    name,
                    vec![crate::text::sanitize(reason)],
                );
            }
            crate::MemberOutcome::Cancelled => {}
        }
        self.current = None;
    }
}

/// What kind of batch a panel is summarising.
///
/// Only the wording differs by kind now: every batch iterates the same
/// way — a member's failure or refusal is a shortfall the run carries
/// past, and only a cancel stops the members behind it — so the shape
/// picks the summary's verb and nothing else.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchShape {
    List,
    Rebuild,
    Update,
}

impl BatchShape {
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::List => "installed",
            Self::Rebuild => "rebuilt",
            Self::Update => "updated",
        }
    }
}

/// A group of per-member lines in a batch's closing panel: the
/// backend's key for it, and the heading the person reads.
struct BatchSection<K> {
    key: K,
    header: &'static str,
    entries: BatchEntries,
}

/// The queue and the tallies of a batch, and nothing about what its
/// members do.
///
/// This is the part two batches were always going to have in common:
/// how many there are, how many are done, what each of them had to say
/// and what is left if the batch stops early. What it deliberately does
/// *not* hold is the meaning of any of it — which outcome belongs in
/// which section and what the summary calls the work. Those stay with
/// the operation; a runner that knew them would be a runner with
/// `if kind` in it, which is the thing this extraction exists to
/// avoid.
///
/// Sections are declared by the backend at construction, in the order
/// the panel should show them, and named afterwards by the backend's
/// own key — never by the heading, which is there to be read.
pub(super) struct BatchRunner<T, K> {
    pub(super) queue: std::collections::VecDeque<T>,
    pub(super) total: usize,
    pub(super) succeeded: usize,
    sections: Vec<BatchSection<K>>,
    /// The last mid-batch reload failure, resurfaced at the summary.
    pub(super) reload_error: Option<String>,
}

impl<T, K: Copy + PartialEq + std::fmt::Debug> BatchRunner<T, K> {
    /// Declared with the backend's own key per section, paired with the
    /// heading it shows. The key is what `record` names; the heading is
    /// only ever read. Keeping them apart means rewording a panel
    /// cannot silently move where a member's lines are filed.
    pub(super) fn new(
        queue: std::collections::VecDeque<T>,
        sections: &[(K, &'static str)],
    ) -> Self {
        // A key declared twice would make `record` file everything into
        // the first and leave the second permanently empty — quiet, and
        // exactly the kind of quiet the panic in `record` exists to
        // prevent.
        // `assert!`, not `debug_assert!`: a duplicate key breaks the
        // invariant in a release build exactly as it does in a debug
        // one — everything files into the first section and the second
        // stays empty — and the panic in `record` is not conditional
        // either. A section table is a constant; if it is wrong, it is
        // wrong everywhere.
        assert!(
            sections
                .iter()
                .enumerate()
                .all(|(i, (key, _))| !sections[..i].iter().any(|(seen, _)| seen == key)),
            "a batch section key was declared twice"
        );
        Self {
            total: queue.len(),
            queue,
            succeeded: 0,
            sections: sections
                .iter()
                .map(|(key, header)| BatchSection {
                    key: *key,
                    header,
                    entries: Vec::new(),
                })
                .collect(),
            reload_error: None,
        }
    }

    /// The member to start next, if the queue still has one.
    pub(super) fn next_member(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    pub(super) fn remaining(&self) -> usize {
        self.queue.len()
    }

    /// One member finished well.
    pub(super) fn succeeded(&mut self) {
        self.succeeded += 1;
    }

    /// One member has something to say, filed under a section the
    /// backend declared.
    ///
    /// A key that was never declared is a programming error, and it
    /// panics rather than dropping the lines or filing them beside a
    /// neighbour. Both of those would be quiet: the diagnostics would
    /// vanish, the count behind them would stay zero, and a batch that
    /// failed could end up presenting itself as one that did not.
    pub(super) fn record(&mut self, key: K, name: &str, lines: Vec<String>) {
        let section = self
            .sections
            .iter_mut()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("batch section {key:?} was never declared"));
        section.entries.push((name.to_owned(), lines));
    }

    /// How many members are filed under a section — and, like `record`,
    /// loud about a key that was never declared. A silent zero here
    /// would be the same lie arriving by the other door: a backend
    /// asking about the wrong section would read "nothing failed" and
    /// summarise a batch that did.
    pub(super) fn recorded(&self, key: K) -> usize {
        self.sections
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("batch section {key:?} was never declared"))
            .entries
            .len()
    }

    pub(super) fn quiet(&self) -> bool {
        self.reload_error.is_none() && self.sections.iter().all(|s| s.entries.is_empty())
    }

    /// The closing panel's lines: the non-empty sections in declared
    /// order, then the reload failure if there was one.
    pub(super) fn report_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for section in &self.sections {
            if section.entries.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(section.header.to_owned());
            for (name, entry_lines) in &section.entries {
                lines.push(crate::text::sanitize(&format!("  {name}:")));
                for line in entry_lines {
                    lines.push(crate::text::sanitize(&format!("    {line}")));
                }
            }
        }
        if let Some(e) = &self.reload_error {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            // Neutral on purpose: the runner does not know when its
            // backend reloads. Migration reads between members, the
            // install batch reads once at the end under no lock at all,
            // and "mid-batch" would be false for the second.
            lines.push(crate::text::sanitize(&format!(
                "(and the list reload failed: {e})"
            )));
        }
        lines
    }

    /// `<head>`, plus what was left when a batch stopped early. The
    /// head is the backend's sentence: only it knows what the work was
    /// called or where it was going.
    pub(super) fn summary(&self, head: &str, ended_early: Option<&str>) -> String {
        let mut summary = head.to_owned();
        if let Some(how) = ended_early {
            let unprocessed = self.remaining();
            // write!, not push_str(&format!(..)): no second allocation.
            let _ = std::fmt::Write::write_fmt(
                &mut summary,
                format_args!(" ({how}; {unprocessed} not attempted)"),
            );
        }
        summary
    }
}

/// A batch member's name and the lines it contributed to the summary.
type BatchEntries = Vec<(String, Vec<String>)>;

/// A confirmed `M`: `migrate --all` as a queue of the very single
/// migrations `m` runs — each its own unit, preflight and cancel door.
/// The batch owns the tally and one final summary: the CLI's
/// "reported, and the batch moves on", in the TUI's shape.
///
/// The queue, plus the one fact that is migration's own.
///
/// Everything else — counts, per-member lines, what is left if it stops
/// — lives in the runner. What stays here is the destination, because
/// only a migration has one, and the section headers below, because
/// only a migration can end with a destination committed and a source
/// still standing.
pub(super) struct MigrateBatch {
    pub(super) dest: PathBuf,
    pub(super) runner: BatchRunner<PendingMigrate, MigrateSection>,
    /// `BatchControl::stopping`'s analogue. `M` has no worker-side
    /// control — its queue lives here in the interface — so the
    /// batch-level stop intent lives here too. Set the moment `c` is
    /// pressed, before the member's own door answers, so a cancel that
    /// loses to placement still speaks for the members not yet
    /// started.
    pub(super) stopping: bool,
}

/// What a migration's members can end up being, and therefore the
/// sections `M`'s panel can have. Migration's own vocabulary: no other
/// batch can commit a destination and leave a source standing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MigrateSection {
    /// The same diagnostics a single failure panel gets; the
    /// already-installed refusal counts as a shortfall, as on the CLI.
    Failed,
    /// Destination built, placement not authorized: the reason, not
    /// the diagnostics of a failure that did not happen.
    Refused,
    /// Destination committed, source not retired: the Incomplete reason
    /// plus that member's warnings.
    Incomplete,
    /// Fully migrated members whose build spoke warnings: the summary
    /// must not launder them.
    Noticed,
}

/// The headings those sections carry in the panel, in panel order.
pub(super) const MIGRATE_SECTIONS: &[(MigrateSection, &str)] = &[
    (MigrateSection::Failed, "failed:"),
    (MigrateSection::Refused, "built, but not placed:"),
    (
        MigrateSection::Incomplete,
        "destination committed, source not retired:",
    ),
    (MigrateSection::Noticed, "migrated, with build warnings:"),
];

/// Did the attempt actually start a worker?
///
/// Three answers, and the third exists because the second must not
/// hide a side effect. A refusal is the end of that attempt; a
/// `NeedsTerminal` is the attempt asking to be run somewhere else, and
/// it carries the action rather than scheduling it. The difference is
/// invisible with one build and fatal with a queue: a runner told
/// "refused" would record the member and move on while a handoff it
/// never agreed to ran the same crate in the terminal.
///
/// Both `start_build` and `start_migrate` answer with this, and that is
/// the whole contract a queue needs from them: *something* comes back
/// for every member, so a slot is never left waiting on an answer that
/// is not coming. What to do with each — stop, record and carry on,
/// hand over, refuse the whole batch — belongs to whoever owns the
/// queue, not here.
pub(super) enum StartOutcome {
    Started,
    Refused(String),
    /// This attempt cannot run captured; here is what would run it, and
    /// why. Nothing has been scheduled.
    NeedsTerminal {
        action: PendingAction,
        reason: String,
    },
}

/// What the escalation preflight found; see `preflight_escalation`.
pub(super) enum Preflight {
    /// Nothing to authorize *before this work starts*: either no
    /// privileged step precedes it, or credentials are validated and
    /// warm. Not a promise that no password will be wanted later — a
    /// build's placement asks at its own door, after the build, which
    /// is the point of that door.
    Ready,
    /// sudo validated but does not cache credentials: a captured
    /// `sudo -n` would be asked a question it cannot voice.
    NoCache,
    /// Something failed; the message is returned, not swallowed — the
    /// caller decides where it must survive.
    Reported(String),
}

/// Where a migration is headed.
pub(super) struct MigrateTarget {
    pub(super) version: String,
    pub(super) dest: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner files and counts; it does not judge. Sections are the
    /// backend's, declared in panel order, and a member's outcome is
    /// filed by the backend into one of them — which is the whole
    /// division this extraction exists to make.
    #[test]
    fn the_runner_keeps_the_queue_and_the_backend_keeps_the_meaning() {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Section {
            Failed,
            Noted,
            Undeclared,
        }
        let mut runner: BatchRunner<u8, Section> = BatchRunner::new(
            [1u8, 2, 3].into_iter().collect(),
            &[(Section::Failed, "failed:"), (Section::Noted, "noted:")],
        );
        assert_eq!(runner.total, 3);
        assert_eq!(runner.next_member(), Some(1));
        assert_eq!(runner.remaining(), 2, "what a stop would leave unattempted");
        assert!(runner.quiet(), "nothing to show yet");

        runner.succeeded();
        runner.record(Section::Failed, "bar", vec!["boom".to_owned()]);
        runner.record(Section::Noted, "baz", vec!["warning: x".to_owned()]);
        assert_eq!(runner.recorded(Section::Failed), 1);
        assert!(!runner.quiet(), "a filed line means a panel");

        // Sections appear in the order the backend declared, not the
        // order things happened.
        let lines = runner.report_lines();
        let failed_at = lines.iter().position(|l| l == "failed:").unwrap();
        let noted_at = lines.iter().position(|l| l == "noted:").unwrap();
        assert!(failed_at < noted_at, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("boom")), "{lines:?}");

        // The head is the backend's sentence; the runner only appends
        // what it alone knows — how much was left.
        assert_eq!(
            runner.summary("did 1 of 3", Some("cancelled")),
            "did 1 of 3 (cancelled; 2 not attempted)"
        );
        assert_eq!(runner.summary("did 1 of 3", None), "did 1 of 3");

        // A section the backend never declared is a programming error,
        // and it is loud: neither the lines nor the count behind them
        // may go missing quietly.
        let declared: &[(Section, &str)] = &[(Section::Failed, "f:")];
        let writing = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut runner: BatchRunner<u8, Section> =
                BatchRunner::new(std::collections::VecDeque::new(), declared);
            runner.record(Section::Undeclared, "qux", vec!["lost".to_owned()]);
        }));
        assert!(writing.is_err(), "filing into an undeclared section panics");
        // And asking about one: a silent zero would be the same lie
        // arriving by the other door.
        let reading = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let runner: BatchRunner<u8, Section> =
                BatchRunner::new(std::collections::VecDeque::new(), declared);
            runner.recorded(Section::Undeclared)
        }));
        assert!(reading.is_err(), "asking about one panics too");
    }
}
