use crate::app::{App, Focus};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};
use zq_proto::{Proto, ProxyStatus, RouteAction};

/// Unified color palette.
pub mod colors {
    use ratatui::style::Color;
    pub const ACCENT: Color = Color::Cyan;
    pub const HEADER: Color = Color::Rgb(180, 180, 220);
    pub const GOOD: Color = Color::Green;
    pub const WARN: Color = Color::Yellow;
    pub const BAD: Color = Color::Red;
    pub const MUTED: Color = Color::DarkGray;
    pub const PROXY: Color = Color::Magenta;
    pub const TCP: Color = Color::Blue;
    pub const HIGHLIGHT_BG: Color = Color::Rgb(40, 40, 60);
    pub const BYTES_IN: Color = Color::Rgb(100, 200, 100);
    pub const BYTES_OUT: Color = Color::Rgb(100, 150, 255);
}

/// Format a byte count into a human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Single-screen draw. Layout:
///   [header bar]       — 1 row: title + stats
///   [apps table]       — 40% of remaining
///   [flows table]      — 60% of remaining
///   [status bar]       — 1 row: connection + help
pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(40),
        Constraint::Percentage(60),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(f, app, chunks[0]);
    draw_apps(f, app, chunks[1]);
    draw_flows(f, app, chunks[2]);
    draw_status_bar(f, app, chunks[3]);
}

/// Header: title + inline stats.
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let conn_dot = if app.connected {
        Span::styled(" \u{25cf} ", Style::default().fg(colors::GOOD))
    } else {
        Span::styled(" \u{25cf} ", Style::default().fg(colors::BAD))
    };

    let proxy_span = match app.proxy_status {
        ProxyStatus::Reachable => Span::styled("proxy:up", Style::default().fg(colors::GOOD)),
        ProxyStatus::Unreachable => Span::styled("proxy:down", Style::default().fg(colors::BAD)),
        ProxyStatus::Unknown => Span::styled("proxy:?", Style::default().fg(colors::WARN)),
    };

    let total_in: u64 = app.flows.iter().map(|f| f.bytes_in).sum();
    let total_out: u64 = app.flows.iter().map(|f| f.bytes_out).sum();

    let line = Line::from(vec![
        Span::styled(" zq", Style::default().fg(colors::ACCENT).add_modifier(Modifier::BOLD)),
        conn_dot,
        Span::styled(
            format!("{} apps", app.apps.len()),
            Style::default().fg(colors::MUTED),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} flows", app.flows.len()),
            Style::default().fg(colors::MUTED),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("\u{2193}{}", format_bytes(total_in)),
            Style::default().fg(colors::BYTES_IN),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("\u{2191}{}", format_bytes(total_out)),
            Style::default().fg(colors::BYTES_OUT),
        ),
        Span::styled("  ", Style::default()),
        proxy_span,
    ]);

    let header = Paragraph::new(line)
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));

    f.render_widget(header, area);
}

/// Apps table.
fn draw_apps(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = matches!(app.focus, Focus::Apps);
    let border_color = if is_focused { colors::ACCENT } else { colors::MUTED };

    let header = Row::new(vec![
        Cell::from("Name"),
        Cell::from("PIDs"),
        Cell::from("Flows"),
        Cell::from("In"),
        Cell::from("Out"),
        Cell::from("Route"),
    ])
    .style(
        Style::default()
            .fg(colors::HEADER)
            .add_modifier(Modifier::UNDERLINED),
    );

    let rows: Vec<Row> = app
        .apps
        .iter()
        .map(|a| {
            let pids = a
                .pids
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let routing_cell = match a.routing {
                RouteAction::Passthrough => {
                    Cell::from("PASS").style(Style::default().fg(colors::MUTED))
                }
                RouteAction::RouteToProxy => Cell::from("PROXY")
                    .style(Style::default().fg(colors::PROXY).add_modifier(Modifier::BOLD)),
            };

            Row::new(vec![
                Cell::from(a.name.clone()),
                Cell::from(pids),
                Cell::from(a.flow_count.to_string()),
                Cell::from(format_bytes(a.bytes_in))
                    .style(Style::default().fg(colors::BYTES_IN)),
                Cell::from(format_bytes(a.bytes_out))
                    .style(Style::default().fg(colors::BYTES_OUT)),
                routing_cell,
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(16),
        Constraint::Length(12),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Applications ")
                .border_style(Style::default().fg(border_color))
                .title_style(Style::default().fg(border_color)),
        )
        .row_highlight_style(
            Style::default()
                .bg(colors::HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = TableState::default();
    if !app.apps.is_empty() && is_focused {
        state.select(Some(app.selected_app));
    }

    f.render_stateful_widget(table, area, &mut state);
}

/// Flows table.
fn draw_flows(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = matches!(app.focus, Focus::Flows);
    let border_color = if is_focused { colors::ACCENT } else { colors::MUTED };

    let header = Row::new(vec![
        Cell::from("ID"),
        Cell::from("App"),
        Cell::from("Proto"),
        Cell::from("Remote"),
        Cell::from("In"),
        Cell::from("Out"),
        Cell::from("Route"),
    ])
    .style(
        Style::default()
            .fg(colors::HEADER)
            .add_modifier(Modifier::UNDERLINED),
    );

    let rows: Vec<Row> = app
        .flows
        .iter()
        .map(|fl| {
            let proto_cell = match fl.proto {
                Proto::Tcp => Cell::from("TCP").style(Style::default().fg(colors::TCP)),
                Proto::Udp => Cell::from("UDP").style(Style::default().fg(colors::WARN)),
            };

            let routing_cell = match fl.routing {
                RouteAction::Passthrough => {
                    Cell::from("PASS").style(Style::default().fg(colors::MUTED))
                }
                RouteAction::RouteToProxy => Cell::from("PROXY")
                    .style(Style::default().fg(colors::PROXY).add_modifier(Modifier::BOLD)),
            };

            Row::new(vec![
                Cell::from(fl.flow_id.to_string()),
                Cell::from(fl.process_name.clone()),
                proto_cell,
                Cell::from(fl.remote_addr.clone()),
                Cell::from(format_bytes(fl.bytes_in))
                    .style(Style::default().fg(colors::BYTES_IN)),
                Cell::from(format_bytes(fl.bytes_out))
                    .style(Style::default().fg(colors::BYTES_OUT)),
                routing_cell,
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(6),
        Constraint::Min(12),
        Constraint::Length(5),
        Constraint::Length(22),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(8),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Flows ")
                .border_style(Style::default().fg(border_color))
                .title_style(Style::default().fg(border_color)),
        )
        .row_highlight_style(
            Style::default()
                .bg(colors::HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = TableState::default();
    if !app.flows.is_empty() && is_focused {
        state.select(Some(app.selected_flow));
    }

    f.render_stateful_widget(table, area, &mut state);
}

/// Status bar with help text.
fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let left = vec![
        Span::raw(" "),
        Span::styled(&app.status_message, Style::default().fg(colors::MUTED)),
    ];

    let help = "[Tab] Focus  [r] Route  [g] Global  [\u{2191}/\u{2193}] Select  [q] Quit";
    let right_span = Span::styled(help, Style::default().fg(colors::MUTED));

    let left_width: usize = left.iter().map(|s| s.width()).sum();
    let right_width = right_span.width() + 1;
    let pad = area.width as usize - left_width.min(area.width as usize) - right_width.min(area.width as usize);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad.max(1))));
    spans.push(right_span);
    spans.push(Span::raw(" "));

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::Rgb(30, 30, 40)));

    f.render_widget(bar, area);
}
