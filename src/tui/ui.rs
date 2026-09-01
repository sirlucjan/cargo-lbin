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
    let [header, list, details, footer] = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(8),
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
        draw_help(frame, area);
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = Filter::ALL.iter().map(|f| match f {
        Filter::All => format!(" Packages ({}) ", app.total()),
        Filter::Updates => format!(" Updates ({}) ", app.updates_available()),
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
            let name = if row.locked {
                format!("{} [locked]", row.name)
            } else {
                row.name.clone()
            };
            TableRow::new(vec![
                Cell::from(name),
                Cell::from(row.version.clone()),
                status_cell(&row.status),
            ])
        })
        .collect();

    let empty_note = match app.filter {
        Filter::All => "nothing installed under this prefix — press i to install",
        Filter::Updates => "no updates known — press r to check crates.io",
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

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    // A finished lookup takes over the panel until dismissed; it is the
    // one piece of information here that did not come from the manifest.
    if let Some(info) = &app.info_result {
        let text: Vec<Line> = info
            .text
            .lines()
            .map(|l| Line::from(l.to_owned()))
            .collect();
        let panel = Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Info: {} (Esc to dismiss) ", info.name)),
        );
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
    } else {
        " ↑/↓ select · Tab filter · Enter/u update · U update all · i install · x remove · r check · s info · ? help · q quit"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(Color::DarkGray))),
        keys_area,
    );

    let checked = match app.report_age {
        Some(age) => format!("checked {}", describe_age(age)),
        None => "never checked".to_owned(),
    };
    // "0 updates" alone would read as "all current"; the unknown count
    // keeps the footer as honest as the status column.
    let mut status = format!(
        " {} packages · {} updates",
        app.total(),
        app.updates_available()
    );
    if app.not_checked() > 0 {
        let _ = write!(status, " · {} not checked", app.not_checked());
    }
    let _ = write!(status, " · {checked}");
    frame.render_widget(
        Paragraph::new(Span::styled(status, Style::default().fg(Color::DarkGray))),
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
            InputPurpose::Info => "info: ",
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
    } else if let Some(label) = app.busy() {
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

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = [
        "↑/↓ j/k     select        Tab       Packages / Updates",
        "g / G       first / last  ?         this help",
        "",
        "Enter, u    update selected crate (confirmed in the terminal)",
        "U           run update --all: fresh plan from crates.io, not the cache",
        "i           install: NAME... [--locked]",
        "x           remove selected crate (asks first)",
        "r           check crates.io for updates (writes the report)",
        "s           info: look one crate up on crates.io by exact name",
        "",
        "Nothing runs on its own: no refresh or network access on start.",
        "Commands that build hand the terminal to cargo and sudo and",
        "return when you press Enter.",
        "",
        "q / Esc     quit (from the list)",
        "",
        "any key closes this help",
    ];
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
