//! The run loop.
//!
//! Blocks on one channel. A frame is drawn only when a message says it would
//! differ, and everything already queued is drained first, so a burst of
//! progress lines costs one frame rather than one frame each. There is no tick.

use std::io::stdout;
use std::sync::mpsc::{Receiver, channel};

use anyhow::Result;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture, read as read_term};
use ratatui::crossterm::execute;

use crate::app::App;
use crate::msg::Msg;
use crate::{herdr, live, store, ui};

pub fn run() -> Result<()> {
    let (sink, inbox) = channel::<Msg>();

    std::thread::Builder::new()
        .name("stash-input".into())
        .spawn({
            let sink = sink.clone();
            move || {
                while let Ok(event) = read_term() {
                    if sink.send(Msg::Term(event)).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning the input thread");

    // The live side needs Herdr; a picker that cannot reach it still opens, with
    // an empty left column and the stash directory intact on the right.
    let live = herdr::client()
        .and_then(|mut client| live::list(&mut client))
        .unwrap_or_default();
    let mut app = App::new(&sink, live, store::list());

    // ▲ `ratatui::init()` sets raw mode and the alternate screen and nothing
    // else — it does not enable mouse reporting, so without this the terminal
    // keeps handling clicks itself and the popup never sees one.
    //
    // The cost, stated because it is real: while capture is on, click-drag text
    // selection inside this pane stops working. Most terminals still allow it
    // with a modifier held (Option on iTerm2). For a list whose whole purpose is
    // choosing a row, being clickable is the better trade.
    let mut terminal = ratatui::init();
    let mouse = execute!(stdout(), EnableMouseCapture).is_ok();

    let outcome = pump(&mut terminal, &mut app, &inbox);

    // Before `restore`, so the sequence is undone while the alternate screen is
    // still up. Leaving capture on would hand the shell underneath a terminal
    // that prints escape codes when it is clicked.
    if mouse {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    outcome
}

fn pump(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    inbox: &Receiver<Msg>,
) -> Result<()> {
    terminal.draw(|frame| ui::draw(frame, app))?;
    while let Ok(message) = inbox.recv() {
        let mut dirty = app.handle(message);
        while let Ok(queued) = inbox.try_recv() {
            dirty |= app.handle(queued);
        }
        if app.quit {
            return Ok(());
        }
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
        }
    }
    Ok(())
}
