//! Stash a Herdr workspace: stop its agents, keep its shape, bring it back.
//!
//! Herdr restores every workspace it has ever held on every start, and offers
//! two ways out: leave it there, or close it and lose it. Upstream declined the
//! middle — an archive section for the sidebar, asked for and closed without a
//! design — so this is that middle, built on the primitives Herdr already
//! publishes.
//!
//! Entrypoints, because Herdr splits them: an **action** runs server-side with no
//! TTY, so it can only do work and toast about it; a **pane** runs in a terminal
//! and can draw. `stash` is an action, `picker` is the pane, and `open` is the
//! action that asks Herdr for that pane.
//!
//! The picker shows both sides — live workspaces and stashed records — because
//! work moves both ways, and it batches: check rows on either side and move them
//! together.
//!
//! `list` and `restore` exist for the operator and for testing the round trip
//! without a TUI in the way.

mod app;
mod capture;
mod herdr;
mod layout;
mod live;
mod msg;
mod record;
mod restore;
mod store;
mod theme;
mod tui;
mod ui;

use anyhow::{Context as _, Result, anyhow, bail};

const PLUGIN_ID: &str = "vsh.stash";
const PANE_ENTRYPOINT: &str = "picker";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("stash") => stash(false),
        Some("stash-force") => stash(true),
        Some("open") => open(),
        Some("picker") => tui::run(),
        Some("list") => list(),
        Some("restore") => restore_one(args.next().as_deref()),
        other => bail!(
            "expected `stash`, `stash-force`, `open`, `picker`, `list` or `restore <id>`, got {other:?}"
        ),
    }
}

/// Capture the workspace this was invoked from, then close it.
///
/// ▲ Two refusals, both waived by `stash-force`, and both about the same thing:
/// a stash must be undoable.
///
/// * An agent **mid-turn**. Closing the workspace stops the process, and a turn
///   killed halfway is the one thing here that destroys work rather than parking
///   it — the transcript keeps what the agent had already written, not what it
///   was about to.
/// * An agent that has **not reported its conversation** yet, which
///   [`capture::capture`] refuses on: nothing could resume it afterwards.
fn stash(force: bool) -> Result<()> {
    let context = herdr_sdk::plugin_context();
    let workspace_id = context
        .workspace_id
        .clone()
        .filter(|id| !id.is_empty())
        .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok())
        .ok_or_else(|| anyhow!("no workspace in this invocation context"))?;

    let mut client = herdr::client()?;

    if !force {
        let snapshot = herdr::snapshot(&mut client)?;
        let working: Vec<String> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.workspace_id == workspace_id)
            .filter(|pane| pane.agent_status.as_deref() == Some("working"))
            .map(|pane| pane.label())
            .collect();
        if !working.is_empty() {
            let body = format!(
                "{} mid-turn. Use the force action to stash anyway.",
                working.join(", ")
            );
            let _ = herdr::notify(&mut client, "Not stashed", &body);
            bail!("{body}");
        }
    }

    let stashed = capture::stash(
        &mut client,
        &workspace_id,
        context.workspace_cwd.clone(),
        force,
    )?;
    let agents = stashed.stash.agents().len();
    let panes = stashed.stash.panes().len();

    let _ = herdr::notify(
        &mut client,
        &format!("Stashed {}", stashed.stash.label),
        &format!("{panes} pane(s), {agents} agent(s) stopped · restore from the stash picker"),
    );
    println!("{}", stashed.path.display());
    Ok(())
}

/// Ask Herdr for the picker.
///
/// A popup rather than a split: it is session-modal, it does not disturb the
/// tiled layout the operator is looking at, and it closes when the process exits.
/// Verified against 0.7.5 that it outlives both a focus change and the closing of
/// the workspace it was opened over — which is what makes stashing the current
/// workspace from inside the picker safe.
/// The socket rather than shelling out to `herdr plugin pane open`, because this
/// already speaks it.
fn open() -> Result<()> {
    let mut client = herdr::client()?;
    let _: serde_json::Value = client.call(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": PLUGIN_ID,
            "entrypoint": PANE_ENTRYPOINT,
            "placement": "popup",
            "width": "62%",
            "height": "52%",
        }),
    )?;
    Ok(())
}

fn list() -> Result<()> {
    for stash in store::list() {
        println!(
            "{}\t{}\t{} pane(s)\t{} agent(s)",
            stash.id,
            stash.label,
            stash.panes().len(),
            stash.agents().len()
        );
    }
    Ok(())
}

/// Restore by id, printing each step. The picker's path with the terminal in
/// place of a frame.
fn restore_one(id: Option<&str>) -> Result<()> {
    let id = id.context("restore needs a stash id — see `list`")?;
    let stash = store::list()
        .into_iter()
        .find(|stash| stash.id == id)
        .ok_or_else(|| anyhow!("no stash {id}"))?;

    let mut client = herdr::client()?;
    // One stash, asked for by name: being put in it is the point.
    let done = restore::restore(&mut client, &stash, true, |step| println!("{step:?}"))?;
    for warning in &done.warnings {
        eprintln!("▲ {warning}");
    }
    // Popping, like the picker: a restored stash whose panes all came back is a
    // duplicate of a live workspace.
    if done.warnings.is_empty() {
        store::delete(&stash.id)?;
    }
    println!("{} · {} agent(s)", done.workspace_id, done.agents);
    Ok(())
}
