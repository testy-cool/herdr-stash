//! Everything that can wake the popup.
//!
//! One channel, two producers: the terminal, and the worker thread doing a batch.
//! Stashing and restoring are seconds to minutes of blocking socket calls —
//! `agent.start` alone waits for an agent to be ready — so they cannot happen on
//! the thread that draws, and their progress arrives here instead.

use ratatui::crossterm::event::Event as TermEvent;

use crate::live::Live;
use crate::record::Stash;

pub enum Msg {
    Term(TermEvent),
    /// Both columns, re-read. Sent at startup and after every batch, because a
    /// batch changes each side by definition.
    Lists {
        live: Vec<Live>,
        stashes: Vec<Stash>,
    },
    /// One line of a batch in flight.
    Progress(String),
    /// A batch that finished, whether or not every item moved.
    Done {
        stashed: usize,
        restored: usize,
        warnings: Vec<String>,
    },
    /// A batch that could not start.
    Failed(String),
}
