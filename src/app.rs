//! The popup's state, and every way it changes.
//!
//! Two columns, one verb each way: a checked workspace on the left moves right by
//! being stashed, a checked stash on the right moves left by being restored. The
//! columns are the two states the same work can be in, which is why the picker
//! shows the session as well as the stash directory — a list of only stashes can
//! show one direction of one verb.
//!
//! Nothing here draws and nothing here talks to Herdr on the drawing thread. A
//! batch is handed to a worker and reported back through [`crate::msg::Msg`],
//! because `agent.start` blocks until an agent is ready and a frozen popup during
//! the one operation the operator is watching would be the worst moment to freeze.
//!
//! Restoring **consumes** the stash, the way popping does. A stash whose workspace
//! is live again is a duplicate in every later listing, and the record is kept only
//! when something did not come back.

use std::collections::HashSet;
use std::sync::mpsc::Sender;

use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::live::Live;
use crate::msg::Msg;
use crate::record::Stash;
use crate::restore::Step;
use crate::theme::Theme;
#[cfg(not(test))]
use crate::{capture, restore};
use crate::{herdr, live, store};

/// Which column has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Live,
    Stashed,
}

impl Side {
    pub fn other(self) -> Self {
        match self {
            Side::Live => Side::Stashed,
            Side::Stashed => Side::Live,
        }
    }

    fn index(self) -> usize {
        match self {
            Side::Live => 0,
            Side::Stashed => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// A forget waiting to be confirmed. Deleting a record is the one
    /// irreversible thing this popup can do.
    Confirm,
    Working,
    /// A batch that finished with something worth reading.
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Stash,
    Restore,
    Forget,
    Close,
}

/// Where the last frame put everything clickable. Rebuilt by every draw, so a
/// click resolves against the geometry actually on screen rather than a second
/// copy of the layout.
#[derive(Debug, Default, Clone)]
pub struct Hits {
    /// One rectangle per column, in [`Side::index`] order.
    pub columns: [Rect; 2],
    /// The first row drawn in each column.
    pub offsets: [usize; 2],
    /// How many terminal rows one entry occupies.
    pub row_height: u16,
    /// Columns of a row that toggle its checkbox rather than move the cursor.
    pub checkbox_width: u16,
    pub buttons: Vec<(Button, Rect)>,
    /// The confirm overlay's two answers.
    pub confirm: Vec<(bool, Rect)>,
}

pub struct App {
    pub live: Vec<Live>,
    pub stashes: Vec<Stash>,
    pub side: Side,
    /// One cursor per column, so switching sides does not lose the other's place.
    pub cursor: [usize; 2],
    pub offsets: [usize; 2],
    /// Checked workspace ids and stash ids. One set: the two id spaces cannot
    /// collide, and a single set means "what is checked" has one answer.
    pub checked: HashSet<String>,
    pub mode: Mode,
    pub theme: Theme,
    pub progress: Vec<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    /// The last batch's outcome, for the line that says what happened.
    pub done: Option<(usize, usize)>,
    pub hits: Hits,
    pub quit: bool,
    sink: Sender<Msg>,
}

impl App {
    /// The lists are passed in rather than read here, so the state machine can be
    /// tested without a session or a stash directory.
    pub fn new(sink: &Sender<Msg>, live: Vec<Live>, stashes: Vec<Stash>) -> Self {
        Self {
            live,
            stashes,
            // The left column is where work starts, and stashing is the verb that
            // brought most people here.
            side: Side::Live,
            cursor: [0, 0],
            offsets: [0, 0],
            checked: HashSet::new(),
            mode: Mode::Browse,
            theme: Theme::load(),
            progress: Vec::new(),
            warnings: Vec::new(),
            error: None,
            done: None,
            hits: Hits::default(),
            quit: false,
            sink: sink.clone(),
        }
    }

    pub fn rows(&self, side: Side) -> usize {
        match side {
            Side::Live => self.live.len(),
            Side::Stashed => self.stashes.len(),
        }
    }

    pub fn cursor(&self, side: Side) -> usize {
        self.cursor[side.index()]
    }

    pub fn is_checked(&self, id: &str) -> bool {
        self.checked.contains(id)
    }

    /// The ids checked on one side, in the order they are drawn.
    pub fn selection(&self, side: Side) -> Vec<String> {
        match side {
            Side::Live => self
                .live
                .iter()
                .map(|workspace| workspace.workspace_id.clone())
                .filter(|id| self.checked.contains(id))
                .collect(),
            Side::Stashed => self
                .stashes
                .iter()
                .map(|stash| stash.id.clone())
                .filter(|id| self.checked.contains(id))
                .collect(),
        }
    }

    /// What an action would act on: the checked rows, or the row under the cursor
    /// when nothing is checked. Checking is for batches; the cursor is still the
    /// obvious single-item gesture.
    fn targets(&self, side: Side) -> Vec<String> {
        let checked = self.selection(side);
        if !checked.is_empty() {
            return checked;
        }
        match side {
            Side::Live => self
                .live
                .get(self.cursor(side))
                .map(|workspace| vec![workspace.workspace_id.clone()])
                .unwrap_or_default(),
            Side::Stashed => self
                .stashes
                .get(self.cursor(side))
                .map(|stash| vec![stash.id.clone()])
                .unwrap_or_default(),
        }
    }

    /// Returns whether the frame would differ.
    pub fn handle(&mut self, message: Msg) -> bool {
        match message {
            Msg::Term(TermEvent::Key(key)) => self.key(key),
            Msg::Term(TermEvent::Mouse(mouse)) => self.mouse(mouse),
            Msg::Term(TermEvent::Resize(..)) => true,
            Msg::Term(_) => false,
            Msg::Lists { live, stashes } => {
                self.live = live;
                self.stashes = stashes;
                self.clamp();
                true
            }
            Msg::Progress(line) => {
                self.progress.push(line);
                true
            }
            Msg::Done {
                stashed,
                restored,
                warnings,
            } => {
                self.finish(stashed, restored, warnings);
                true
            }
            Msg::Failed(error) => {
                self.error = Some(error);
                self.mode = Mode::Report;
                true
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        // Windows sends both; a repeat is a press as far as this popup cares.
        if key.kind == KeyEventKind::Release {
            return false;
        }
        match self.mode {
            Mode::Confirm => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.forget(),
                _ => {
                    self.mode = Mode::Browse;
                    true
                }
            },
            // A batch is not interruptible: workspaces are being closed and built,
            // and abandoning one halfway would leave panes with no record of what
            // they were.
            Mode::Working => false,
            Mode::Report => {
                self.mode = Mode::Browse;
                true
            }
            Mode::Browse => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.quit = true;
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                KeyCode::Home | KeyCode::Char('g') => self.move_to(0),
                KeyCode::End | KeyCode::Char('G') => self.move_to(usize::MAX),
                KeyCode::Tab | KeyCode::BackTab => self.focus(self.side.other()),
                KeyCode::Left | KeyCode::Char('h') => self.focus(Side::Live),
                KeyCode::Right | KeyCode::Char('l') => self.focus(Side::Stashed),
                KeyCode::Char(' ') => self.toggle(),
                KeyCode::Char('a') => self.toggle_all(),
                // The move: whichever side has the cursor decides the direction.
                KeyCode::Enter => self.act(),
                KeyCode::Char('d') | KeyCode::Delete => self.confirm(),
                KeyCode::Char('r') => self.reload(),
                _ => false,
            },
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) -> bool {
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            MouseEventKind::Down(MouseButton::Left) => self.click(at),
            _ => false,
        }
    }

    /// A click on a checkbox toggles it; a click anywhere else on a row takes the
    /// cursor there, switching column if that is where the click landed.
    ///
    /// Deliberately **no click-to-act on a row**: on the left, acting closes a
    /// workspace and stops its processes, and a gesture that destructive should
    /// not be one pixel away from selecting.
    fn click(&mut self, at: Position) -> bool {
        if self.mode == Mode::Confirm {
            let answer = self
                .hits
                .confirm
                .iter()
                .find(|(_, rect)| rect.contains(at))
                .map(|(yes, _)| *yes);
            return match answer {
                Some(true) => self.forget(),
                Some(false) => {
                    self.mode = Mode::Browse;
                    true
                }
                None => false,
            };
        }
        if self.mode == Mode::Report {
            self.mode = Mode::Browse;
            return true;
        }
        if self.mode != Mode::Browse {
            return false;
        }

        if let Some((button, _)) = self
            .hits
            .buttons
            .iter()
            .find(|(_, rect)| rect.contains(at))
            .copied()
        {
            return match button {
                Button::Stash => self.start(Side::Live),
                Button::Restore => self.start(Side::Stashed),
                Button::Forget => self.confirm_on(Side::Stashed),
                Button::Close => {
                    self.quit = true;
                    true
                }
            };
        }

        for side in [Side::Live, Side::Stashed] {
            let column = self.hits.columns[side.index()];
            if !column.contains(at) || self.hits.row_height == 0 {
                continue;
            }
            let row = (at.y - column.y) / self.hits.row_height;
            let index = self.hits.offsets[side.index()] + row as usize;
            if index >= self.rows(side) {
                return false;
            }
            self.side = side;
            self.cursor[side.index()] = index;
            // The checkbox is at the start of the row's first line.
            let on_box = at.x < column.x + self.hits.checkbox_width
                && (at.y - column.y).is_multiple_of(self.hits.row_height);
            if on_box {
                self.toggle();
            }
            return true;
        }
        false
    }

    fn focus(&mut self, side: Side) -> bool {
        std::mem::replace(&mut self.side, side) != side
    }

    fn move_by(&mut self, delta: isize) -> bool {
        let rows = self.rows(self.side);
        if rows == 0 {
            return false;
        }
        let index = self.side.index();
        let next = (self.cursor[index] as isize + delta).clamp(0, rows as isize - 1) as usize;
        std::mem::replace(&mut self.cursor[index], next) != next
    }

    fn move_to(&mut self, row: usize) -> bool {
        let rows = self.rows(self.side);
        if rows == 0 {
            return false;
        }
        let index = self.side.index();
        let next = row.min(rows - 1);
        std::mem::replace(&mut self.cursor[index], next) != next
    }

    fn id_at(&self, side: Side, row: usize) -> Option<String> {
        match side {
            Side::Live => self
                .live
                .get(row)
                .map(|workspace| workspace.workspace_id.clone()),
            Side::Stashed => self.stashes.get(row).map(|stash| stash.id.clone()),
        }
    }

    fn toggle(&mut self) -> bool {
        let Some(id) = self.id_at(self.side, self.cursor(self.side)) else {
            return false;
        };
        if !self.checked.remove(&id) {
            self.checked.insert(id);
        }
        true
    }

    /// Check everything on this side, or clear it if all of it already is.
    fn toggle_all(&mut self) -> bool {
        let ids: Vec<String> = (0..self.rows(self.side))
            .filter_map(|row| self.id_at(self.side, row))
            .collect();
        if ids.is_empty() {
            return false;
        }
        let all = ids.iter().all(|id| self.checked.contains(id));
        for id in ids {
            match all {
                true => self.checked.remove(&id),
                false => self.checked.insert(id),
            };
        }
        true
    }

    fn reload(&mut self) -> bool {
        let sink = self.sink.clone();
        // Off the drawing thread, like everything else that speaks to Herdr.
        std::thread::spawn(move || {
            let _ = sink.send(Msg::Lists {
                live: herdr::client()
                    .and_then(|mut client| live::list(&mut client))
                    .unwrap_or_default(),
                stashes: store::list(),
            });
        });
        false
    }

    fn clamp(&mut self) {
        for side in [Side::Live, Side::Stashed] {
            let index = side.index();
            self.cursor[index] = self.cursor[index].min(self.rows(side).saturating_sub(1));
        }
        // A row that no longer exists cannot stay checked, or the next batch would
        // act on an id that is gone.
        let alive: HashSet<String> = self
            .live
            .iter()
            .map(|workspace| workspace.workspace_id.clone())
            .chain(self.stashes.iter().map(|stash| stash.id.clone()))
            .collect();
        self.checked.retain(|id| alive.contains(id));
    }

    fn confirm(&mut self) -> bool {
        self.confirm_on(self.side)
    }

    /// Forgetting only applies to records; a live workspace is not this popup's to
    /// destroy, and the way to remove one is to stash it.
    fn confirm_on(&mut self, side: Side) -> bool {
        if side != Side::Stashed || self.stashes.is_empty() {
            return false;
        }
        self.side = Side::Stashed;
        if self.targets(Side::Stashed).is_empty() {
            return false;
        }
        self.mode = Mode::Confirm;
        true
    }

    pub fn condemned(&self) -> Vec<String> {
        self.targets(Side::Stashed)
    }

    fn forget(&mut self) -> bool {
        for id in self.condemned() {
            if let Err(error) = store::delete(&id) {
                self.error = Some(error.to_string());
            }
            self.checked.remove(&id);
        }
        self.mode = Mode::Browse;
        self.stashes = store::list();
        self.clamp();
        true
    }

    /// Move whatever the focused side points at to the other side.
    fn act(&mut self) -> bool {
        self.start(self.side)
    }

    fn start(&mut self, side: Side) -> bool {
        let targets = self.targets(side);
        if targets.is_empty() {
            return false;
        }

        self.mode = Mode::Working;
        self.progress.clear();
        self.warnings.clear();
        self.error = None;
        self.done = None;

        // The directories come from the live rows, because `session.snapshot` does
        // not carry a workspace's own cwd and the record wants it.
        let cwds: Vec<Option<String>> = targets
            .iter()
            .map(|id| {
                self.live
                    .iter()
                    .find(|workspace| &workspace.workspace_id == id)
                    .and_then(|workspace| workspace.cwd.clone())
            })
            .collect();

        let sink = self.sink.clone();

        // ▲ Never spawn under `cargo test`. [`batch`] dials the real Herdr socket,
        // and a unit test that reaches it does not simulate a batch — it performs
        // one in the operator's live session. Four stray workspaces named after a
        // fixture stash are how this was found, and the earlier click-to-act
        // gesture is what reached it. The state change is still asserted; only the
        // side effect is withheld.
        #[cfg(test)]
        let spawned: std::io::Result<()> = {
            let _ = (sink, side, targets, cwds);
            Ok(())
        };
        #[cfg(not(test))]
        let spawned = std::thread::Builder::new()
            .name("stash-batch".into())
            .spawn(move || batch(&sink, side, targets, cwds))
            .map(|_| ());

        if let Err(error) = spawned {
            self.error = Some(error.to_string());
            self.mode = Mode::Report;
        }
        true
    }

    /// A batch always leaves the popup open: it is a workbench with two columns,
    /// not a one-shot dialog, and the operator usually has a second thing to move.
    /// Restoring already focuses the workspace it built, so closing lands there.
    fn finish(&mut self, stashed: usize, restored: usize, warnings: Vec<String>) {
        self.done = Some((stashed, restored));
        self.warnings = warnings;
        self.mode = match self.warnings.is_empty() {
            true => Mode::Browse,
            false => Mode::Report,
        };
        self.reload();
    }
}

/// One batch, on its own thread and its own client.
///
/// Compiled out of test builds: see the note in [`App::start`].
#[cfg(not(test))]
fn batch(sink: &Sender<Msg>, side: Side, targets: Vec<String>, cwds: Vec<Option<String>>) {
    let mut client = match herdr::client() {
        Ok(client) => client,
        Err(error) => {
            let _ = sink.send(Msg::Failed(error.to_string()));
            return;
        }
    };

    let mut warnings = Vec::new();
    let mut stashed = 0;
    let mut restored = 0;
    let total = targets.len();
    // One item is a request to be taken there; a batch is not, so the operator's
    // place is remembered and given back.
    let single = total == 1;
    let was_focused = match single {
        true => None,
        false => herdr::snapshot(&mut client)
            .ok()
            .and_then(|snapshot| snapshot.focused_workspace_id),
    };

    for (index, id) in targets.iter().enumerate() {
        match side {
            Side::Live => {
                let _ = sink.send(Msg::Progress(format!(
                    "stashing {}/{total} · {id}",
                    index + 1
                )));
                let cwd = cwds.get(index).cloned().flatten();
                match capture::stash(&mut client, id, cwd, false) {
                    Ok(done) => {
                        stashed += 1;
                        let _ = sink.send(Msg::Progress(format!("stashed {}", done.stash.label)));
                    }
                    Err(error) => warnings.push(format!("{id}: {error:#}")),
                }
            }
            Side::Stashed => {
                let Some(stash) = store::list().into_iter().find(|stash| &stash.id == id) else {
                    warnings.push(format!("{id}: no longer on disk"));
                    continue;
                };
                let _ = sink.send(Msg::Progress(format!(
                    "restoring {}/{total} · {}",
                    index + 1,
                    stash.label
                )));
                let progress = sink.clone();
                match restore::restore(&mut client, &stash, single, |step| {
                    let _ = progress.send(Msg::Progress(format!("  {}", describe(&step))));
                }) {
                    Ok(done) => {
                        restored += 1;
                        warnings.extend(done.warnings.iter().cloned());
                        // Popping: a stash that came back whole is a duplicate of a
                        // live workspace. One that did not keeps its record.
                        if done.warnings.is_empty() {
                            let _ = store::delete(&stash.id);
                        }
                    }
                    Err(error) => warnings.push(format!("{}: {error:#}", stash.label)),
                }
            }
        }
    }

    // Stashing already puts focus back per workspace closed; this covers a batch
    // restore, which deliberately did not focus anything it built.
    if let Some(workspace_id) = was_focused {
        let _ = herdr::focus_workspace(&mut client, &workspace_id);
    }

    let _ = sink.send(Msg::Done {
        stashed,
        restored,
        warnings,
    });
}

/// One line of progress, in the operator's terms rather than the API's.
fn describe(step: &Step) -> String {
    match step {
        Step::Shape { tabs } => match tabs {
            1 => "rebuilding the layout".to_owned(),
            many => format!("rebuilding the layout · {many} tabs"),
        },
        Step::Agent {
            index,
            total,
            label,
        } => format!("resuming {index}/{total} · {label}"),
        Step::Plugin { title } => format!("reopening {title}"),
        Step::Focused => "focusing".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Node, Pane, Tab, VERSION};
    use std::sync::mpsc::channel;

    fn stash(id: &str) -> Stash {
        Stash {
            version: VERSION,
            id: id.into(),
            stashed_at: 1,
            label: id.into(),
            cwd: None,
            tabs: vec![Tab {
                label: None,
                cwd: None,
                layout: Node::Pane(Pane::default()),
                plugins: Vec::new(),
            }],
            active_tab: 0,
        }
    }

    fn workspace(id: &str) -> Live {
        Live {
            workspace_id: id.into(),
            label: id.into(),
            panes: 1,
            ..Live::default()
        }
    }

    fn app(live: usize, stashed: usize) -> App {
        let (sink, _inbox) = channel();
        App::new(
            &sink,
            (0..live).map(|n| workspace(&format!("w{n}"))).collect(),
            (0..stashed).map(|n| stash(&format!("s{n}"))).collect(),
        )
    }

    #[test]
    fn each_column_keeps_its_own_cursor() {
        let mut app = app(3, 3);
        assert!(app.move_by(1));
        assert_eq!(app.cursor(Side::Live), 1);
        assert!(app.focus(Side::Stashed));
        assert_eq!(
            app.cursor(Side::Stashed),
            0,
            "the other column is untouched"
        );
        assert!(app.move_by(2));
        assert_eq!(app.cursor(Side::Stashed), 2);
        assert!(app.focus(Side::Live));
        assert_eq!(app.cursor(Side::Live), 1, "coming back lands where it left");
    }

    #[test]
    fn checking_accumulates_and_reads_back_per_side() {
        let mut app = app(3, 2);
        app.toggle();
        app.move_by(2);
        app.toggle();
        assert_eq!(app.selection(Side::Live), vec!["w0", "w2"]);
        assert!(app.selection(Side::Stashed).is_empty());
        // And a second toggle un-checks rather than adding twice.
        app.toggle();
        assert_eq!(app.selection(Side::Live), vec!["w0"]);
    }

    #[test]
    fn check_all_clears_when_everything_is_already_checked() {
        let mut app = app(3, 0);
        app.toggle_all();
        assert_eq!(app.selection(Side::Live).len(), 3);
        app.toggle_all();
        assert!(app.selection(Side::Live).is_empty());
    }

    /// Checking is for batches; a cursor on its own is still a target, so the
    /// single-item gesture does not require a checkbox first.
    #[test]
    fn the_cursor_is_the_target_when_nothing_is_checked() {
        let mut app = app(2, 2);
        app.move_by(1);
        assert_eq!(app.targets(Side::Live), vec!["w1"]);
        app.toggle();
        app.move_by(-1);
        assert_eq!(
            app.targets(Side::Live),
            vec!["w1"],
            "a check outranks the cursor"
        );
    }

    /// A batch acts on ids, so a row that vanished must not stay checked.
    #[test]
    fn a_reload_drops_checks_for_rows_that_are_gone() {
        let mut app = app(2, 0);
        app.toggle_all();
        assert_eq!(app.selection(Side::Live).len(), 2);
        app.handle(Msg::Lists {
            live: vec![workspace("w0")],
            stashes: Vec::new(),
        });
        assert_eq!(app.selection(Side::Live), vec!["w0"]);
    }

    /// Forgetting is for records. A live workspace is removed by stashing it, not
    /// by deleting something.
    #[test]
    fn forgetting_refuses_the_live_column() {
        let mut app = app(2, 0);
        assert!(!app.confirm());
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn forgetting_from_the_live_column_switches_sides_rather_than_acting_on_it() {
        let mut app = app(2, 2);
        assert_eq!(app.side, Side::Live);
        assert!(app.confirm_on(Side::Stashed));
        assert_eq!(app.side, Side::Stashed);
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(app.condemned(), vec!["s0"]);
    }

    #[test]
    fn a_batch_in_flight_ignores_keys() {
        let mut app = app(1, 1);
        app.mode = Mode::Working;
        assert!(
            !app.handle(Msg::Term(TermEvent::Key(KeyEvent::from(KeyCode::Char(
                'q'
            )))))
        );
        assert!(!app.quit);
    }

    /// A click on the checkbox toggles; a click on the rest of the row only moves
    /// the cursor, because acting on the left column stops processes.
    #[test]
    fn a_click_toggles_on_the_box_and_moves_elsewhere() {
        let mut app = app(3, 1);
        app.hits = Hits {
            columns: [Rect::new(0, 1, 30, 6), Rect::new(31, 1, 30, 6)],
            offsets: [0, 0],
            row_height: 2,
            checkbox_width: 5,
            buttons: Vec::new(),
            confirm: Vec::new(),
        };
        // Second row's first line, inside the checkbox.
        assert!(app.click(Position::new(2, 3)));
        assert_eq!(app.cursor(Side::Live), 1);
        assert_eq!(app.selection(Side::Live), vec!["w1"]);
        // Same row, past the checkbox: cursor only, and nothing acted on.
        assert!(app.click(Position::new(20, 3)));
        assert_eq!(app.selection(Side::Live), vec!["w1"]);
        assert_eq!(app.mode, Mode::Browse);
        // The right column takes focus when clicked.
        assert!(app.click(Position::new(40, 1)));
        assert_eq!(app.side, Side::Stashed);
    }

    /// A batch that moved everything leaves the popup on the columns: it is a
    /// workbench, and the operator usually has a second thing to move.
    #[test]
    fn a_clean_batch_returns_to_the_columns_with_a_tally() {
        let mut app = app(1, 1);
        app.mode = Mode::Working;
        app.handle(Msg::Progress("stashing 1/1 · w0".into()));
        assert_eq!(app.progress, vec!["stashing 1/1 · w0"]);
        app.handle(Msg::Done {
            stashed: 1,
            restored: 0,
            warnings: Vec::new(),
        });
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.done, Some((1, 0)));
        assert!(!app.quit, "the popup stays open");
    }

    /// Anything that did not move has to be read before it scrolls away, so the
    /// report holds the frame until dismissed.
    #[test]
    fn a_batch_with_warnings_holds_the_report() {
        let mut app = app(0, 1);
        app.mode = Mode::Working;
        app.handle(Msg::Done {
            stashed: 0,
            restored: 1,
            warnings: vec!["pi: transcript is gone".into()],
        });
        assert_eq!(app.mode, Mode::Report);
        assert_eq!(app.warnings.len(), 1);
        // And any key goes back to the columns rather than closing the popup.
        app.handle(Msg::Term(TermEvent::Key(KeyEvent::from(KeyCode::Char(
            'x',
        )))));
        assert_eq!(app.mode, Mode::Browse);
        assert!(!app.quit);
    }

    #[test]
    fn a_batch_that_could_not_start_says_why() {
        let mut app = app(1, 0);
        app.handle(Msg::Failed(
            "herdr-stash needs a running Herdr session".into(),
        ));
        assert_eq!(app.mode, Mode::Report);
        assert!(app.error.is_some());
    }

    #[test]
    fn progress_lines_read_as_prose() {
        assert_eq!(
            describe(&Step::Agent {
                index: 1,
                total: 2,
                label: "claude · Access".into()
            }),
            "resuming 1/2 · claude · Access"
        );
        assert_eq!(describe(&Step::Shape { tabs: 1 }), "rebuilding the layout");
    }
}
