//! The frame.
//!
//! Two columns — live workspaces, stashed records — because the work moves both
//! ways and a single list can only show one direction of one verb. The focused
//! column is the one whose border is lit.
//!
//! No colour is named here: every style comes from [`crate::theme`] by role. The
//! frame also reports back where it put every clickable thing, so a click resolves
//! against the geometry on screen rather than a second copy of the layout.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Button, Hits, Mode, Side};
use crate::live::Live;
use crate::record::Stash;
use crate::theme::Theme;

/// Two lines per entry: what it is, then where it was and what is in it.
const ROW: u16 = 2;

/// `▸ ` plus `[x]` — the marker and the box, which is the span of a row that
/// toggles rather than moves the cursor.
const BOX: u16 = 5;

/// Why forgetting is not like closing a pane. The overlay is sized to fit it.
const STAKES: &str = "their workspaces are already closed — this cannot be undone";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let outer = Block::bordered()
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(" stash ", app.theme.title()))
        .title_bottom(summary(app));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    let body = rows[0];
    let footer = rows[1];

    let columns = match app.mode {
        // A batch takes the whole body: what it is doing matters more than what
        // could be done next, and the columns are about to change anyway.
        Mode::Working | Mode::Report => {
            draw_progress(frame, body, app);
            [Rect::default(), Rect::default()]
        }
        _ => draw_columns(frame, body, app),
    };

    let buttons = draw_footer(frame, footer, app);
    let confirm = match app.mode {
        Mode::Confirm => draw_confirm(frame, area, app),
        _ => Vec::new(),
    };

    app.hits = Hits {
        columns,
        offsets: app.offsets,
        row_height: ROW,
        checkbox_width: BOX,
        buttons,
        confirm,
    };
}

/// The bottom border's tally: what a batch just did, or what is checked.
fn summary(app: &App) -> Line<'static> {
    let text = match app.done {
        Some((stashed, restored)) if stashed + restored > 0 => {
            let mut parts = Vec::new();
            if stashed > 0 {
                parts.push(format!("{stashed} stashed"));
            }
            if restored > 0 {
                parts.push(format!("{restored} restored"));
            }
            format!(" {} ", parts.join(" · "))
        }
        _ => {
            let live = app.selection(Side::Live).len();
            let stashed = app.selection(Side::Stashed).len();
            match (live, stashed) {
                (0, 0) => format!(" {} live · {} stashed ", app.live.len(), app.stashes.len()),
                (live, 0) => format!(" {live} checked to stash "),
                (0, stashed) => format!(" {stashed} checked to restore "),
                (live, stashed) => format!(" {live} to stash · {stashed} to restore "),
            }
        }
    };
    Line::from(Span::styled(text, app.theme.dimmed())).right_aligned()
}

/// Both columns, and the inner rectangle each one's rows were drawn in.
fn draw_columns(frame: &mut Frame, area: Rect, app: &mut App) -> [Rect; 2] {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let live = draw_column(frame, halves[0], app, Side::Live);
    let stashed = draw_column(frame, halves[1], app, Side::Stashed);
    [live, stashed]
}

fn draw_column(frame: &mut Frame, area: Rect, app: &mut App, side: Side) -> Rect {
    let focused = app.side == side;
    let (title, count) = match side {
        Side::Live => (" live ", app.live.len()),
        Side::Stashed => (" stashed ", app.stashes.len()),
    };

    let border = match focused {
        true => app.theme.accent,
        false => app.theme.border,
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title,
            match focused {
                true => app.theme.title(),
                false => app.theme.dimmed(),
            },
        ))
        .title(Line::from(Span::styled(format!(" {count} "), app.theme.dimmed())).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if count == 0 {
        let hint = match side {
            Side::Live => "no workspaces",
            Side::Stashed => "nothing stashed yet",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, app.theme.dimmed()))).centered(),
            centre(inner, 1),
        );
        return inner;
    }

    // Keep the cursor on screen. The offset lives on the app because a scroll has
    // to survive the frame that caused it.
    let visible = (inner.height / ROW).max(1) as usize;
    let index = match side {
        Side::Live => 0,
        Side::Stashed => 1,
    };
    let cursor = app.cursor(side);
    if cursor < app.offsets[index] {
        app.offsets[index] = cursor;
    } else if cursor >= app.offsets[index] + visible {
        app.offsets[index] = cursor + 1 - visible;
    }

    let mut lines = Vec::with_capacity(visible * ROW as usize);
    for row in app.offsets[index]..(app.offsets[index] + visible).min(count) {
        let on_cursor = focused && row == cursor;
        lines.extend(match side {
            Side::Live => live_row(&app.live[row], app, on_cursor, inner.width),
            Side::Stashed => stash_row(&app.stashes[row], app, on_cursor, inner.width),
        });
    }
    frame.render_widget(Paragraph::new(lines), inner);
    inner
}

/// The checkbox and cursor marker, which are one span so the click target is one
/// span too.
fn box_span(checked: bool, on_cursor: bool, theme: &Theme) -> Span<'static> {
    let marker = if on_cursor { "▸" } else { " " };
    let tick = if checked { "✔" } else { " " };
    Span::styled(
        format!("{marker} [{tick}]"),
        match checked {
            true => Style::default().fg(theme.accent),
            false => theme.dimmed(),
        },
    )
}

fn live_row(workspace: &Live, app: &App, on_cursor: bool, width: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let head = match on_cursor {
        true => theme.selected(),
        false => theme.plain(),
    };

    let mut first = vec![
        box_span(app.is_checked(&workspace.workspace_id), on_cursor, theme),
        Span::raw(" "),
        Span::styled(trim(&workspace.label, width.saturating_sub(BOX + 3)), head),
    ];
    // Mid-turn is why a stash would be refused, so the row says it up front.
    if workspace.working {
        first.push(Span::styled(" ▲", Style::default().fg(theme.warn)));
    }
    if workspace.current {
        first.push(Span::styled(" · here", theme.dimmed()));
    }

    let mut detail = format!(
        "{} · {} pane",
        home(workspace.cwd.as_deref()),
        workspace.panes
    );
    if workspace.panes != 1 {
        detail.push('s');
    }

    let mut second = vec![
        Span::raw(" ".repeat(BOX as usize + 1)),
        Span::styled(
            trim(
                &detail,
                width.saturating_sub(BOX + 1 + badge_width(&workspace.agents)),
            ),
            theme.dimmed(),
        ),
    ];
    second.extend(badges(&workspace.agents, theme));

    vec![Line::from(first), Line::from(second)]
}

fn stash_row(stash: &Stash, app: &App, on_cursor: bool, width: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let head = match on_cursor {
        true => theme.selected(),
        false => theme.plain(),
    };

    // The age is right-aligned: a column of ages down the edge reads as a column,
    // where one trailing each label reads as part of the name.
    let age = ago(stash.stashed_at);
    let room = width.saturating_sub(BOX + 2 + age.chars().count() as u16);
    let label = trim(&stash.label, room);
    let pad = (room as usize).saturating_sub(label.chars().count());
    let first = vec![
        box_span(app.is_checked(&stash.id), on_cursor, theme),
        Span::raw(" "),
        Span::styled(label, head),
        Span::raw(" ".repeat(pad + 1)),
        Span::styled(age, theme.dimmed()),
    ];

    let panes = stash.panes().len();
    let plugins: usize = stash.tabs.iter().map(|tab| tab.plugins.len()).sum();
    let mut detail = format!("{} · {} pane", home(stash.cwd.as_deref()), panes);
    if panes != 1 {
        detail.push('s');
    }
    if stash.tabs.len() > 1 {
        detail.push_str(&format!(" · {} tabs", stash.tabs.len()));
    }

    let kinds: Vec<String> = stash
        .agents()
        .into_iter()
        .map(|agent| agent.kind.clone())
        .collect();
    // Panes another plugin owns get their own badge rather than a word in the
    // detail line: they are restored by a different mechanism, and it is worth
    // seeing that a stash carries them.
    let plugin_badge = match plugins {
        0 => String::new(),
        many => format!(" ◧ {many}"),
    };
    let mut second = vec![
        Span::raw(" ".repeat(BOX as usize + 1)),
        Span::styled(
            trim(
                &detail,
                width.saturating_sub(
                    BOX + 1 + badge_width(&kinds) + plugin_badge.chars().count() as u16,
                ),
            ),
            theme.dimmed(),
        ),
    ];
    second.extend(badges(&kinds, theme));
    if !plugin_badge.is_empty() {
        second.push(Span::styled(
            plugin_badge,
            Style::default().fg(theme.plugin),
        ));
    }

    vec![Line::from(first), Line::from(second)]
}

/// One badge per agent, so the cost of stashing or restoring is visible before it
/// is paid.
fn badges(kinds: &[String], theme: &Theme) -> Vec<Span<'static>> {
    kinds
        .iter()
        .map(|kind| Span::styled(format!(" ● {kind}"), Style::default().fg(theme.agent)))
        .collect()
}

fn badge_width(kinds: &[String]) -> u16 {
    kinds
        .iter()
        .map(|kind| kind.chars().count() as u16 + 3)
        .sum()
}

fn draw_progress(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = app
        .progress
        .iter()
        .map(|line| Line::from(Span::styled(format!(" {line}"), app.theme.dimmed())))
        .collect();

    if let Some((stashed, restored)) = app.done.filter(|_| app.mode == Mode::Report) {
        lines.push(Line::from(Span::styled(
            format!(" ✔ {stashed} stashed · {restored} restored"),
            Style::default().fg(app.theme.ok),
        )));
    }
    if let Some(error) = &app.error {
        lines.push(Line::from(Span::styled(
            format!(" ✘ {error}"),
            Style::default().fg(app.theme.danger),
        )));
    }
    for warning in &app.warnings {
        lines.push(Line::from(Span::styled(
            format!(" ▲ {warning}"),
            Style::default().fg(app.theme.warn),
        )));
    }
    if app.mode == Mode::Report && !app.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            " records were kept for anything that did not come back",
            app.theme.dimmed(),
        )));
    }

    // The tail, because a long batch's newest line is the interesting one.
    let height = area.height as usize;
    if lines.len() > height {
        lines.drain(..lines.len() - height);
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The footer, and where each button landed.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) -> Vec<(Button, Rect)> {
    if app.mode != Mode::Browse {
        // Nothing under a confirm overlay: its own answers are the affordance, and
        // a second hint behind it only shows through as debris.
        let hint = match app.mode {
            Mode::Working => " working — this cannot be interrupted",
            Mode::Report => " any key to go back",
            _ => "",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, app.theme.dimmed()))),
            area,
        );
        return Vec::new();
    }

    let to_stash = app.selection(Side::Live).len();
    let to_restore = app.selection(Side::Stashed).len();
    let labels = [
        (
            Button::Stash,
            match to_stash {
                0 => "→ stash".to_owned(),
                many => format!("→ stash {many}"),
            },
            app.side == Side::Live,
        ),
        (
            Button::Restore,
            match to_restore {
                0 => "← restore".to_owned(),
                many => format!("← restore {many}"),
            },
            app.side == Side::Stashed,
        ),
        (Button::Forget, "d forget".to_owned(), false),
        (Button::Close, "esc close".to_owned(), false),
    ];

    let mut spans = Vec::new();
    let mut rects = Vec::new();
    let mut x = area.x;

    for (button, label, primary) in labels {
        let text = format!(" {label} ");
        let width = text.chars().count() as u16;
        // A button drawn past the edge is not clickable, so it is not claimed.
        if x + width > area.x + area.width {
            break;
        }
        spans.push(Span::styled(text, app.theme.button(primary)));
        spans.push(Span::raw(" "));
        rects.push((button, Rect::new(x, area.y, width, 1)));
        x += width + 1;
    }

    // The keys, when there is room for them. They are the same acts as the
    // buttons, which is the point of showing both.
    let hint = "space check · a all · tab side";
    if x + (hint.chars().count() as u16) < area.x + area.width {
        spans.push(Span::styled(hint, app.theme.dimmed()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    rects
}

/// The forget confirmation, and where its two answers landed.
fn draw_confirm(frame: &mut Frame, area: Rect, app: &App) -> Vec<(bool, Rect)> {
    let condemned = app.condemned();
    let what = match condemned.len() {
        1 => app
            .stashes
            .iter()
            .find(|stash| stash.id == condemned[0])
            .map(|stash| stash.label.clone())
            .unwrap_or_else(|| condemned[0].clone()),
        many => format!("{many} stashes"),
    };

    let widest = (what.chars().count() + 9).max(STAKES.chars().count()) as u16;
    let width = (widest + 2).clamp(30, area.width.saturating_sub(2));
    let box_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + area.height / 2,
        width,
        height: 5,
    };

    frame.render_widget(Clear, box_area);
    let block = Block::bordered()
        .border_style(Style::default().fg(app.theme.danger))
        .title(Span::styled(" forget ", app.theme.title()));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    let question = Line::from(vec![
        Span::styled("Forget ", app.theme.plain()),
        Span::styled(
            trim(&what, inner.width.saturating_sub(10)),
            app.theme.title(),
        ),
        Span::styled("?", app.theme.plain()),
    ]);
    let warning = Line::from(Span::styled(STAKES, app.theme.dimmed()));

    let yes = " y forget ";
    let no = " n keep ";
    let yes_rect = Rect::new(inner.x, inner.y + 2, yes.chars().count() as u16, 1);
    let no_rect = Rect::new(
        yes_rect.x + yes_rect.width + 1,
        yes_rect.y,
        no.chars().count() as u16,
        1,
    );
    let answers = Line::from(vec![
        Span::styled(yes, Style::default().fg(app.theme.danger)),
        Span::raw(" "),
        Span::styled(no, app.theme.dimmed()),
    ]);

    frame.render_widget(Paragraph::new(vec![question, warning, answers]), inner);
    vec![(true, yes_rect), (false, no_rect)]
}

/// A centred band of `height` rows, for an empty column.
fn centre(area: Rect, height: u16) -> Rect {
    Rect {
        y: area.y + area.height.saturating_sub(height) / 2,
        height: height.min(area.height),
        ..area
    }
}

/// `~` for the home directory: a column this narrow cannot spend twenty columns
/// saying `/Users/victor` on every row.
fn home(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return "—".to_owned();
    };
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && cwd.starts_with(&home) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd.to_owned(),
    }
}

/// How long ago, in the coarsest unit that is still true.
fn ago(stashed_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(stashed_at);
    let seconds = now.saturating_sub(stashed_at);
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Truncate to `width` columns, keeping the end legible.
fn trim(text: &str, width: u16) -> String {
    let width = width as usize;
    if text.chars().count() <= width {
        return text.to_owned();
    }
    match width {
        0 => String::new(),
        1 => "…".to_owned(),
        _ => text.chars().take(width - 1).collect::<String>() + "…",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Direction as SplitDirection;
    use crate::record::{Agent, Attached, Node, Pane, Tab, VERSION};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::mpsc::channel;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs()
    }

    /// A stash with everything a row has to say: an agent, a plugin pane, two
    /// panes, and a directory under `$HOME`.
    fn furnished() -> Stash {
        Stash {
            version: VERSION,
            id: "1-w6".into(),
            stashed_at: now() - 7200,
            label: "Access".into(),
            cwd: std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/work/access")),
            tabs: vec![Tab {
                label: None,
                cwd: None,
                layout: Node::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(Pane::default())),
                    second: Box::new(Node::Pane(Pane {
                        agent: Some(Agent {
                            kind: "claude".into(),
                            session_kind: "id".into(),
                            session: "abc".into(),
                            title: Some("Access".into()),
                            argv: Vec::new(),
                        }),
                        ..Pane::default()
                    })),
                },
                plugins: vec![Attached {
                    plugin_id: "vsh.explorer".into(),
                    entrypoint: "sidebar".into(),
                    title: "Explorer".into(),
                    direction: SplitDirection::Right,
                    first: true,
                    anchor: 0,
                }],
            }],
            active_tab: 0,
        }
    }

    fn workspace() -> Live {
        Live {
            workspace_id: "w6".into(),
            label: "pi-ecosystem".into(),
            cwd: std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/workspace/victor")),
            panes: 3,
            agents: vec!["pi".into()],
            working: true,
            current: true,
        }
    }

    /// The rendered popup, one string per line. The only honest way to check a
    /// frame: a popup has no pane id, so its screen cannot be read back out of
    /// Herdr the way a split pane's can.
    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("drawing the popup");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app(live: Vec<Live>, stashes: Vec<Stash>) -> App {
        let (sink, _inbox) = channel();
        App::new(&sink, live, stashes)
    }

    #[test]
    fn both_columns_are_drawn_with_their_own_counts() {
        let mut app = app(vec![workspace()], vec![furnished()]);
        let screen = rendered(&mut app, 96, 12);
        assert!(screen.contains(" live "), "{screen}");
        assert!(screen.contains(" stashed "), "{screen}");
        assert!(screen.contains("pi-ecosystem"), "{screen}");
        assert!(screen.contains("Access"), "{screen}");
        // Each side's own facts.
        assert!(screen.contains("3 panes"), "{screen}");
        assert!(screen.contains("2h ago"), "{screen}");
        assert!(screen.contains("● pi"), "{screen}");
        assert!(screen.contains("● claude"), "{screen}");
    }

    /// The two states a live row can be in that change what stashing it means.
    #[test]
    fn a_live_row_marks_mid_turn_and_the_current_workspace() {
        let mut app = app(vec![workspace()], Vec::new());
        let screen = rendered(&mut app, 96, 10);
        assert!(screen.contains("▲"), "{screen}");
        assert!(screen.contains("· here"), "{screen}");
    }

    #[test]
    fn checking_shows_in_the_box_the_footer_and_the_tally() {
        let mut app = app(vec![workspace()], vec![furnished()]);
        let before = rendered(&mut app, 96, 12);
        assert!(before.contains("[ ]"), "{before}");
        assert!(before.contains("→ stash"), "{before}");
        assert!(before.contains("1 live · 1 stashed"), "{before}");

        app.handle(crate::msg::Msg::Term(
            ratatui::crossterm::event::Event::Key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Char(' '),
            )),
        ));
        let after = rendered(&mut app, 96, 12);
        assert!(after.contains("[✔]"), "{after}");
        assert!(after.contains("→ stash 1"), "{after}");
        assert!(after.contains("1 checked to stash"), "{after}");
    }

    #[test]
    fn an_empty_side_says_so_rather_than_showing_a_blank_box() {
        let mut app = app(Vec::new(), Vec::new());
        let screen = rendered(&mut app, 96, 10);
        assert!(screen.contains("no workspaces"), "{screen}");
        assert!(screen.contains("nothing stashed yet"), "{screen}");
    }

    /// Forgetting several at once has to name the count, and still say why it
    /// cannot be taken back.
    #[test]
    fn the_forget_overlay_counts_what_it_would_forget() {
        let mut stash_b = furnished();
        stash_b.id = "2-w7".into();
        stash_b.label = "ompex".into();
        let mut app = app(Vec::new(), vec![furnished(), stash_b]);
        app.side = Side::Stashed;
        app.handle(crate::msg::Msg::Term(
            ratatui::crossterm::event::Event::Key(ratatui::crossterm::event::KeyEvent::from(
                ratatui::crossterm::event::KeyCode::Char('a'),
            )),
        ));
        app.mode = Mode::Confirm;
        let screen = rendered(&mut app, 96, 14);
        assert!(screen.contains("Forget 2 stashes?"), "{screen}");
        assert!(screen.contains("cannot be undone"), "{screen}");
        assert!(screen.contains("y forget"), "{screen}");
        assert!(screen.contains("n keep"), "{screen}");
    }

    #[test]
    fn a_partial_batch_reports_what_did_not_move() {
        let mut app = app(vec![workspace()], vec![furnished()]);
        app.mode = Mode::Report;
        app.progress = vec!["restoring 1/1 · Access".into()];
        app.done = Some((0, 1));
        app.warnings = vec!["pi: transcript is gone — left as a shell".into()];
        let screen = rendered(&mut app, 96, 12);
        assert!(screen.contains("✔ 0 stashed · 1 restored"), "{screen}");
        assert!(screen.contains("▲ pi: transcript is gone"), "{screen}");
        assert!(screen.contains("records were kept"), "{screen}");
    }

    #[test]
    fn an_age_reads_in_the_coarsest_true_unit() {
        let now = now();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 600), "10m ago");
        assert_eq!(ago(now - 7200), "2h ago");
        assert_eq!(ago(now - 172_800), "2d ago");
    }

    /// A clock that moved backwards must not panic or print a huge age.
    #[test]
    fn a_stash_from_the_future_is_just_now() {
        assert_eq!(ago(now() + 3600), "just now");
    }

    #[test]
    fn trimming_never_exceeds_the_width() {
        assert_eq!(trim("short", 10), "short");
        assert_eq!(trim("a rather long label", 6), "a rat…");
        assert_eq!(trim("anything", 1), "…");
        assert_eq!(trim("anything", 0), "");
    }
}
