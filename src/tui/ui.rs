//! Rendering: a pure function of the [`App`] state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState, Tabs, Wrap,
};

use super::{App, Filter, InputPurpose, MessageKind, RowStatus};
use crate::report::describe_age;

pub fn draw(frame: &mut Frame, app: &App) {
    // The gauge gets its own framed transient panel: the footer stays
    // free for messages, which the old arrangement hid for the whole
    // build.
    if let Some(gauge) = app.build_progress() {
        let [header, list, details, build, footer] = Layout::new(
            Direction::Vertical,
            [
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(8),
                Constraint::Length(3),
                Constraint::Length(3),
            ],
        )
        .areas(frame.area());
        draw_tabs(frame, app, header);
        draw_list(frame, app, list);
        draw_details(frame, app, details);
        draw_gauge(frame, &gauge, app.transient_panel_title(), build);
        draw_footer(frame, app, footer);
        if app.show_help {
            draw_help(frame, app, frame.area());
        }
        return;
    }
    // A pinned report gets more rows than the resting pane: the findings
    // are the thing being read. Bounded by the terminal, floored at the
    // resting height, still scrollable past either.
    let details_h = if app.build_report.is_some() {
        frame.area().height.saturating_sub(11).clamp(8, 16)
    } else {
        8
    };
    let [header, list, details, footer] = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(details_h),
            Constraint::Length(3),
        ],
    )
    .areas(frame.area());

    draw_tabs(frame, app, header);
    draw_list(frame, app, list);
    draw_details(frame, app, details);
    draw_footer(frame, app, footer);

    if app.show_help {
        let area = frame.area();
        draw_help(frame, app, area);
    }
}

/// A framed, labeled box — one yellow frameless row blended into the
/// footer, and a build deserves to be unmissable. A status line, never
/// a progress bar: cargo knows what it started, not what remains, so
/// a percentage would be an invention.
fn draw_gauge(frame: &mut Frame, gauge: &str, title: &str, area: Rect) {
    // The title follows the job: the shape is shared with verify (one
    // mechanism), but a frame saying Build over an audit would promise
    // the wrong operation.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {gauge}"),
            Style::default().fg(Color::Yellow),
        )),
        inner,
    );
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Filter::ALL.iter().map(|f| match f {
        Filter::All => format!(" Packages ({}) ", app.total()),
        Filter::Updates => format!(" Updates ({}) ", app.updates_available()),
        Filter::Pinned => format!(" Pinned ({}) ", app.pinned_count()),
    });
    let tabs = Tabs::new(titles)
        .select(app.filter.index())
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" cargo-lbin — {} ", app.prefix().display())),
        );
    frame.render_widget(tabs, area);
}

fn status_cell(status: &RowStatus) -> Cell<'static> {
    let (text, color) = match status {
        RowStatus::UpToDate => ("✓ up to date".to_owned(), Color::Green),
        RowStatus::Outdated(latest) => (format!("↑ {latest}"), Color::Yellow),
        // Gray, not DarkGray: the selected row paints a DarkGray
        // background, and the one status that must never be missed is
        // the one that would vanish into it.
        RowStatus::Unknown => ("? not checked".to_owned(), Color::Gray),
    };
    Cell::from(Span::styled(text, Style::default().fg(color)))
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<TableRow> = app
        .visible()
        .into_iter()
        .map(|row| {
            let mut name = row.name.clone();
            if row.locked {
                name.push_str(" [locked]");
            }
            if row.pinned {
                name.push_str(" [pinned]");
            }
            if !row.also.is_empty() {
                name.push_str(&row.also);
            }
            TableRow::new(vec![
                Cell::from(name),
                Cell::from(row.version.clone()),
                status_cell(&row.status),
            ])
        })
        .collect();

    let empty_note = match app.filter {
        Filter::All => "nothing installed under this prefix — press i to install",
        Filter::Updates => "no unpinned updates known — press r to check crates.io",
        Filter::Pinned => "no pinned crates — p in Packages pins the selected crate",
    };
    let block = Block::default().borders(Borders::ALL);
    if rows.is_empty() {
        let note = Paragraph::new(Span::styled(
            format!(" {empty_note}"),
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(note, area);
        return;
    }

    let header = TableRow::new(["NAME", "VERSION", "STATUS"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(16),
            Constraint::Length(24),
        ],
    )
    .header(header)
    .block(block)
    // Both colors explicit: with only the background set, a light theme's
    // dark default foreground would sink into DarkGray.
    .row_highlight_style(
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");

    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

/// The sticky report panel: a failure's tail and log path, or a
/// success's warnings. Split from `draw_details` for exactly the reason
/// clippy suggests — it is its own panel with its own rules.
fn draw_report(frame: &mut Frame, report: &crate::tui::BuildReport, scroll: u16, area: Rect) {
    let color = if report.failed {
        Color::Red
    } else {
        Color::Yellow
    };
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        report.title.clone(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(
        report
            .lines
            .iter()
            .map(|l| Line::from(Span::raw(l.clone()))),
    );
    // The scroll clamp, computed here by the renderer's own word wrapper
    // (`Paragraph::line_count`): any arithmetic stand-in undercounts
    // exactly when word wrap breaks early, and an undercounted height is
    // a tail the reader cannot reach. Clamped for display, not mutated.
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2).max(1);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total_rows = u16::try_from(paragraph.line_count(inner_w)).unwrap_or(u16::MAX);
    let max_scroll = total_rows.saturating_sub(inner_h);
    let effective = scroll.min(max_scroll);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if max_scroll > 0 {
            " Esc/Enter dismisses · Up/Down scrolls "
        } else {
            " Esc/Enter dismisses "
        });
    // Wrapped: a panel that truncates the log path defeats its purpose.
    frame.render_widget(paragraph.block(block).scroll((effective, 0)), area);
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    // A build's report owns the panel until dismissed: either kind must
    // survive longer than one keypress.
    if let Some(report) = &app.build_report {
        draw_report(frame, report, app.report_scroll, area);
        return;
    }
    // A finished search takes over the panel until dismissed; it is the
    // one piece of information here that did not come from the manifest.
    if let Some(search) = &app.search_result {
        let name_w = search.hits.iter().map(|h| h.name.len()).max().unwrap_or(0);
        // `api::search` bounds the hits, so the numbering and the footer
        // describe the same list.
        let lines: Vec<Line> = search
            .hits
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let mut spans = vec![
                    Span::styled(
                        format!("[{}] ", i + 1),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{:<name_w$}  {}  ", hit.name, hit.version)),
                ];
                if let Some(have) = search.installed.get(&hit.name) {
                    spans.push(Span::styled(
                        format!("[installed {have}]  "),
                        Style::default().fg(Color::Green),
                    ));
                }
                spans.push(Span::styled(
                    hit.description.clone(),
                    Style::default().fg(Color::Gray),
                ));
                Line::from(spans)
            })
            .collect();
        let panel = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(
            format!(" Search: {} (digit installs, Esc dismisses) ", search.query),
        ));
        frame.render_widget(panel, area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    if let Some(row) = app.selected_row() {
        // Plain values inherit the terminal's foreground; only the three
        // status states carry a color of their own, so the panel reads
        // the same on light and dark schemes.
        lines.push(kv("Crate", &row.name, Style::default()));
        lines.push(kv("Installed", &row.version, Style::default()));
        match &row.status {
            RowStatus::UpToDate => {
                lines.push(kv(
                    "Latest",
                    &row.version,
                    Style::default().fg(Color::Green),
                ));
            }
            RowStatus::Outdated(latest) => lines.push(kv(
                "Latest",
                &latest.to_string(),
                Style::default().fg(Color::Yellow),
            )),
            RowStatus::Unknown => lines.push(kv(
                "Latest",
                "not checked (press r)",
                Style::default().fg(Color::Gray),
            )),
        }
        lines.push(kv("Binaries", &row.bins.join(", "), Style::default()));
        if row.locked {
            lines.push(kv("Build", "--locked (reused on update)", Style::default()));
        }
        if row.pinned {
            lines.push(kv(
                "Pinned",
                "held at this version; p to unpin",
                Style::default().fg(Color::Yellow),
            ));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "nothing selected",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let panel =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Selected "));
    frame.render_widget(panel, area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    use std::fmt::Write as _;
    let [keys_area, status_area, line_area] = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ],
    )
    .areas(area);

    let keys = if app.confirm.is_some() {
        " y confirm · any other key cancel"
    } else if app.input.is_some() {
        " Enter run · Esc cancel"
    } else if app.manifest_error.is_some() {
        // The degraded key bar advertises only what the gate lets
        // through — a bar promising `u update · i install` over a
        // manifest that will not load is instructions for a door that
        // is locked — and it renames `r` to what `r` now does.
        " manifest unavailable · v verify · r retry load · B other prefix · ? help · q quit"
    } else if app.build_running() {
        // Deliberately uncategorical: the hint describes the controls; the
        // truthful per-press outcome is the runtime message's job.
        " ↑/↓ select · c cancel/escalate · Ctrl-C cancel/quit"
    } else {
        " ↑/↓ select · Tab filter · Enter/u update · U update all · i install · x remove · \
         m migrate · M migrate all · B other prefix · p pin · D downgrade · v verify · r check · s search · ? help · q quit"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(Color::DarkGray))),
        keys_area,
    );

    let checked = match app.report_age {
        Some(age) => format!("checked {}", describe_age(age)),
        None => "never checked".to_owned(),
    };
    // "0 updates" alone would read as "all current". In the degraded
    // state the whole line is one honest sentence — "0 packages" over a
    // manifest that refused to load would be the invented zero
    // VerifyReport's Option forbids: not zero, unknown.
    let (status, status_color) = if app.manifest_error.is_some() {
        (
            " managed crate count unavailable — the manifest did not load".to_owned(),
            Color::Red,
        )
    } else {
        let mut status = format!(
            " {} packages · {} updates",
            app.total(),
            app.updates_available()
        );
        // The pinned backlog is absent from the updates count (not something
        // `U` will do), so it is voiced here.
        match (app.pinned_count(), app.pinned_outdated()) {
            (0, _) => {}
            (p, 0) => {
                let _ = write!(status, " · {p} pinned");
            }
            (p, held) => {
                let _ = write!(status, " · {p} pinned ({held} behind)");
            }
        }
        if app.not_checked() > 0 {
            let _ = write!(status, " · {} not checked", app.not_checked());
        }
        let _ = write!(status, " · {checked}");
        (status, Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(status, Style::default().fg(status_color))),
        status_area,
    );

    // Bottom line, by priority: a pending confirmation, an open input, a
    // background job, the last message.
    if let Some(confirm) = &app.confirm {
        let line = Span::styled(
            format!(" {}", confirm.prompt),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(Paragraph::new(line), line_area);
    } else if let Some(input) = &app.input {
        let label = match input.purpose {
            InputPurpose::Install => "install: ",
            InputPurpose::Search => "search: ",
        };
        let text = format!(" {label}{}", input.buffer);
        // Cursor after the typed text; widths are clamped to u16 because
        // that is what the terminal addresses, and a line longer than the
        // terminal is already unreadable.
        let cursor_x = line_area
            .x
            .saturating_add(u16::try_from(text.chars().count()).unwrap_or(u16::MAX));
        frame.render_widget(Paragraph::new(text), line_area);
        frame.set_cursor_position((
            cursor_x.min(line_area.right().saturating_sub(1)),
            line_area.y,
        ));
    } else if app.build_progress().is_none()
        && let Some(label) = app.busy()
    {
        // The Build panel owns build status; skipping the generic label here
        // is also what lets "a build is running; c cancels it" show while
        // the gauge is up.
        let line = Span::styled(format!(" {label}"), Style::default().fg(Color::Cyan));
        frame.render_widget(Paragraph::new(line), line_area);
    } else if let Some(message) = &app.message {
        let color = match message.kind {
            MessageKind::Info => Color::Green,
            MessageKind::Warning => Color::Yellow,
            MessageKind::Error => Color::Red,
        };
        let line = Span::styled(format!(" {}", message.text), Style::default().fg(color));
        frame.render_widget(Paragraph::new(line), line_area);
    }
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    // The degraded help is its own short page, not the normal one grayed
    // out: the one function still standing must not hand out
    // instructions for locked doors — worst of all `r`, renamed by the
    // footer. Five keys work; the page lists five.
    if app.manifest_error.is_some() {
        let lines = [
            "v           verify: audit the broken managed state",
            "r           retry loading the manifest",
            "B           switch to the other known prefix",
            "q / Esc     quit (from the list)",
            "",
            "The manifest did not load. Mutating actions, update",
            "checks and search are disabled until it is repaired",
            "by hand — v names every finding.",
            "",
            "any key closes this help",
        ];
        draw_help_box(frame, &lines, area);
        return;
    }
    let lines = [
        "↑/↓ j/k     select        Tab       Packages / Updates / Pinned",
        "g/G Home/End first / last ?         this help",
        "",
        "Enter, u    update selected crate (confirmed in the terminal)",
        "U           run update --all: fresh plan from crates.io, not the cache",
        "i           install: NAME[@VERSION]... [--locked]  (@VERSION pins);",
        "            a single crate builds in place inside the Build panel,",
        "            a batch hands the terminal over as before",
        "x           remove selected crate (asks first; in place unless",
        "            removal needs sudo — then the terminal, as before)",
        "m           migrate selected crate to the other prefix (asks",
        "            first; pinned rebuilds its exact version there,",
        "            unpinned installs the latest; retired here;",
        "            /usr/local <-> ~/.local only, custom via CLI --to)",
        "M           migrate every crate (asks first; a queue of single",
        "            migrations, one summary; c cancels the batch)",
        "c           cancel the running build (again, or automatically",
        "            after ~2s of no effect: SIGKILL); past the placement",
        "            door a cancel is too late: an install finishes placing,",
        "            a migration proceeds through its retirement attempt",
        "B           jump to the other prefix (/usr/local <-> ~/.local);",
        "            the selection follows the crate when it is visible there",
        "p           pin / unpin selected crate; a pin declares the version:",
        "            update --all holds it back, m migrates exactly it",
        "            (in place unless pinning needs sudo)",
        "D           downgrade: pick an older version in the terminal, pinned",
        "v           verify: the manifest's claims checked against the disk,",
        "            read-only; violations and warnings land in a panel,",
        "            naming the repair where one is unambiguous",
        "r           check crates.io for updates (writes the report)",
        "s           search crates.io by keyword; a digit then picks a hit",
        "            and opens the install line with its name",
        "",
        "Nothing runs on its own: no refresh or network access on start.",
        "Single-crate installs build inside the TUI: sudo credentials are",
        "checked up front and prompts happen on the real terminal (again",
        "only if the timestamp expires); a failed build",
        "keeps its last lines and the log path in the details panel.",
        "update, downgrade and batch installs hand the terminal to cargo",
        "and sudo as before, and return when you press Enter.",
        "",
        "q / Esc     quit (from the list); during a build, Ctrl-C",
        "            cancels it and quits once the worker stops",
        "",
        "any key closes this help",
    ];
    draw_help_box(frame, &lines, area);
}

/// The framed, centered popup both help pages share: sizing from the
/// widest line, one box, one title — so the degraded page differs only
/// in what it says, never in how it appears.
fn draw_help_box(frame: &mut Frame, lines: &[&str], area: Rect) {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let popup = centered(
        area,
        u16::try_from(width + 4).unwrap_or(u16::MAX),
        u16::try_from(lines.len() + 2).unwrap_or(u16::MAX),
    );
    frame.render_widget(Clear, popup);
    let text: Vec<Line> = lines.iter().map(|l| Line::from(format!(" {l}"))).collect();
    let help = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Keys "));
    frame.render_widget(help, popup);
}

/// A `width`×`height` rectangle in the middle of `area`, shrunk to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn kv(key: &str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<10}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_owned(), value_style),
    ])
}
