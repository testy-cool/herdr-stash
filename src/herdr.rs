//! The socket calls this plugin makes, and nothing else.
//!
//! Every one goes through [`herdr_sdk::Client`], which dials per call because
//! Herdr answers one request per connection. Nothing here holds state; the
//! ordering that matters lives in [`crate::capture`] and [`crate::restore`].
//!
//! ▲ `session.snapshot` is not in `herdr --help`. It is in
//! `herdr api schema --json`, it is what `herdr api snapshot` calls, and it is
//! the only call that returns workspaces, tabs, panes, per-tab geometry and
//! every pane's agent session together — one round trip for a whole capture,
//! against `pane.list` plus a `pane.layout` per tab.

use anyhow::{Context as _, Result};
use herdr_sdk::Client;
use herdr_sdk::model::Pane;
use serde::Deserialize;

use crate::layout::{Direction, Layout};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
    pub layouts: Vec<Layout>,
    pub focused_workspace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: Option<String>,
    pub active_tab_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: Option<String>,
    pub number: u32,
}

impl Tab {
    /// The label worth recording: one the operator chose.
    ///
    /// Herdr labels an unnamed tab with its own number, so restoring that string
    /// would pin a name that only ever meant "the first tab" onto whichever tab
    /// the restore happens to create.
    pub fn custom_label(&self) -> Option<String> {
        self.label
            .as_deref()
            .filter(|label| !label.is_empty() && *label != self.number.to_string())
            .map(str::to_owned)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProcessInfo {
    pub shell_pid: Option<u32>,
    pub foreground_process_group_id: Option<u32>,
    pub foreground_processes: Vec<Process>,
}

impl ProcessInfo {
    /// The pane's foreground process group leader, which for an agent pane is the
    /// agent itself.
    pub fn leader(&self) -> Option<&Process> {
        let pid = self.foreground_process_group_id?;
        self.foreground_processes
            .iter()
            .find(|process| process.pid == pid)
    }

    /// Whether this pane is sitting at its own prompt with nothing running in
    /// front of it.
    ///
    /// The signal is the pane's shell being its own foreground process group.
    ///
    /// ▲ Necessary and not sufficient. Measured against 0.7.5, a pane satisfies
    /// this the instant its shell is spawned, while `agent.start` still refuses it
    /// with `agent_pane_busy` — *agent target pane … is not an available shell* —
    /// for up to about a second afterwards, because Herdr wants a prompt it has
    /// actually seen. There is no field or event for that state, so this rules out
    /// a pane with a command running and [`crate::restore`] retries the start.
    pub fn at_prompt(&self) -> bool {
        match (self.shell_pid, self.foreground_process_group_id) {
            (Some(shell), Some(foreground)) => shell == foreground,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Process {
    pub pid: u32,
    pub argv0: Option<String>,
    /// The real argument vector, when the OS will give it up.
    ///
    /// ▲ Null for some agents. Measured against 0.7.5: a `zsh` pane reports
    /// `argv: ["-zsh"]` and a `cmdline`, while a running pi reports `argv0: "pi"`
    /// with both null — pi rewrites its process title, and there is nothing
    /// underneath it to read.
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

/// A pane entrypoint some installed plugin owns, keyed by the title Herdr shows
/// as the pane's label — the only thread back from a live pane to the plugin
/// that opened it.
#[derive(Debug, Clone)]
pub struct PluginPane {
    pub plugin_id: String,
    pub entrypoint: String,
    pub title: String,
}

pub fn snapshot(client: &mut Client) -> Result<Snapshot> {
    #[derive(Deserialize)]
    struct Envelope {
        snapshot: Snapshot,
    }
    let envelope: Envelope = client.call("session.snapshot", serde_json::json!({}))?;
    Ok(envelope.snapshot)
}

/// Every pane entrypoint of every installed plugin.
///
/// ▲ A pane does not say which plugin owns it — measured against 0.7.5 by
/// reading a live plugin pane out of `session.snapshot`: it has a `label` and no
/// `plugin_id`. So ownership is inferred by matching that label against these
/// titles. A title two plugins share is ambiguous and the first wins; both are
/// still reopened as *a* plugin pane rather than mistaken for a shell.
pub fn plugin_panes(client: &mut Client) -> Result<Vec<PluginPane>> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        plugins: Vec<Plugin>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Plugin {
        plugin_id: String,
        panes: Vec<Entry>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Entry {
        id: String,
        title: String,
    }

    let envelope: Envelope = client.call("plugin.list", serde_json::json!({}))?;
    Ok(envelope
        .plugins
        .into_iter()
        .flat_map(|plugin| {
            plugin.panes.into_iter().map(move |pane| PluginPane {
                plugin_id: plugin.plugin_id.clone(),
                entrypoint: pane.id,
                title: pane.title,
            })
        })
        .filter(|pane| !pane.title.is_empty())
        .collect())
}

pub fn process_info(client: &mut Client, pane_id: &str) -> Result<ProcessInfo> {
    #[derive(Deserialize)]
    struct Envelope {
        process_info: ProcessInfo,
    }
    let envelope: Envelope = client.call(
        "pane.process_info",
        serde_json::json!({ "pane_id": pane_id }),
    )?;
    Ok(envelope.process_info)
}

/// Create a workspace, and return its id and the id of the pane it opens with.
pub fn create_workspace(
    client: &mut Client,
    cwd: Option<&str>,
    label: &str,
) -> Result<(String, String)> {
    #[derive(Deserialize)]
    struct Created {
        workspace: Workspace,
        root_pane: Pane,
    }
    let created: Created = client.call(
        "workspace.create",
        serde_json::json!({ "cwd": cwd, "label": label, "focus": false }),
    )?;
    Ok((created.workspace.workspace_id, created.root_pane.pane_id))
}

/// Create a tab, and return the pane it opens with.
pub fn create_tab(
    client: &mut Client,
    workspace_id: &str,
    cwd: Option<&str>,
    label: Option<&str>,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Created {
        root_pane: Pane,
    }
    let created: Created = client.call(
        "tab.create",
        serde_json::json!({
            "workspace_id": workspace_id,
            "cwd": cwd,
            "label": label,
            "focus": false,
        }),
    )?;
    Ok(created.root_pane.pane_id)
}

/// Split `pane_id` and return the new pane, which is always the second child.
///
/// `ratio` is the share the **existing** pane keeps, which is what Herdr's own
/// layout publishes — so a recorded ratio can be replayed unchanged.
pub fn split(
    client: &mut Client,
    pane_id: &str,
    direction: Direction,
    ratio: f32,
    cwd: Option<&str>,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Created {
        pane: Pane,
    }
    let created: Created = client.call(
        "pane.split",
        serde_json::json!({
            "target_pane_id": pane_id,
            "direction": direction_name(direction),
            "ratio": ratio,
            "cwd": cwd,
            "focus": false,
        }),
    )?;
    Ok(created.pane.pane_id)
}

pub fn rename_pane(client: &mut Client, pane_id: &str, label: &str) -> Result<()> {
    let _: serde_json::Value = client.call(
        "pane.rename",
        serde_json::json!({ "pane_id": pane_id, "label": label }),
    )?;
    Ok(())
}

/// Block until `pane_id` is an available shell, or until `timeout` runs out.
///
/// ▲ `agent.start` requires its target pane to be at an interactive prompt and
/// fails outright otherwise — measured against 0.7.5, where a restore that
/// started an agent immediately after `pane.split` failed with *agent target pane
/// w1B:p2 is not an available shell*. A pane's shell is spawned asynchronously,
/// and on a machine whose shell startup does real work there is a window of some
/// hundreds of milliseconds where the pane exists and is not yet usable. There is
/// no event for it, so this polls.
pub fn wait_for_shell(client: &mut Client, pane_id: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if process_info(client, pane_id).is_ok_and(|info| info.at_prompt()) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Start an agent, blocking until Herdr sees it ready for input.
///
/// The timeout is explicit and generous: an agent resuming a long conversation
/// reads its whole transcript before it prompts, and Herdr's 30-second default
/// is a startup budget rather than a resume budget.
pub fn start_agent(
    client: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<()> {
    let _: serde_json::Value = client.call(
        "agent.start",
        serde_json::json!({
            "name": name,
            "kind": kind,
            "pane_id": pane_id,
            "args": args,
            "timeout_ms": 120_000,
        }),
    )?;
    Ok(())
}

/// The agent names already taken, so a restore does not collide with one.
pub fn agent_names(client: &mut Client) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        agents: Vec<Named>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Named {
        name: Option<String>,
    }
    let envelope: Envelope = client.call("agent.list", serde_json::json!({}))?;
    Ok(envelope.agents.into_iter().filter_map(|a| a.name).collect())
}

/// Open another plugin's pane beside `target_pane_id`, and return the pane it
/// created.
///
/// The response carries the pane, which is what makes the left-hand-sidebar swap
/// possible without re-reading the layout to guess which pane appeared.
pub fn open_plugin_pane(
    client: &mut Client,
    plugin_id: &str,
    entrypoint: &str,
    target_pane_id: &str,
    direction: Direction,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Envelope {
        plugin_pane: Opened,
    }
    #[derive(Deserialize)]
    struct Opened {
        pane: Pane,
    }
    let envelope: Envelope = client.call(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": plugin_id,
            "entrypoint": entrypoint,
            "placement": "split",
            "direction": direction_name(direction),
            "target_pane_id": target_pane_id,
            "focus": false,
        }),
    )?;
    Ok(envelope.plugin_pane.pane.pane_id)
}

/// Exchange two panes' places in the layout.
///
/// ▲ This focuses the workspace it acts in and there is no way to ask it not to —
/// measured against 0.7.5, where a single `pane.swap` emitted
/// `workspace.focus` for the workspace being restored and pulled the operator
/// out of the one they were working in. `workspace.create`, `pane.split` and
/// `plugin.pane.open` all take `focus: false`; this does not. So a caller that
/// swaps must put the operator's focus back — [`crate::restore`] does it by
/// swapping before its own final focus, and [`crate::capture`] never swaps.
pub fn swap_panes(client: &mut Client, source: &str, target: &str) -> Result<()> {
    let _: serde_json::Value = client.call(
        "pane.swap",
        serde_json::json!({ "source_pane_id": source, "target_pane_id": target }),
    )?;
    Ok(())
}

pub fn focus_pane(client: &mut Client, pane_id: &str) -> Result<()> {
    let _: serde_json::Value =
        client.call("pane.focus", serde_json::json!({ "pane_id": pane_id }))?;
    Ok(())
}

pub fn focus_workspace(client: &mut Client, workspace_id: &str) -> Result<()> {
    let _: serde_json::Value = client.call(
        "workspace.focus",
        serde_json::json!({ "workspace_id": workspace_id }),
    )?;
    Ok(())
}

/// Close a workspace, which stops every process in it.
///
/// That is the point rather than a side effect: a stash is meant to release the
/// agents, and this is the call that does it. Verified against 0.7.5 — the
/// resumed agent's pid was gone within two seconds of the close returning.
pub fn close_workspace(client: &mut Client, workspace_id: &str) -> Result<()> {
    let _: serde_json::Value = client.call(
        "workspace.close",
        serde_json::json!({ "workspace_id": workspace_id }),
    )?;
    Ok(())
}

/// Say something to the operator from a process that has no terminal.
///
/// A plugin action runs server-side with no TTY, so a toast is the only channel
/// it has. Failure is swallowed by the caller: a stash that worked and could not
/// announce itself is still a stash that worked.
pub fn notify(client: &mut Client, title: &str, body: &str) -> Result<()> {
    let _: serde_json::Value = client.call(
        "notification.show",
        serde_json::json!({ "title": title, "body": body, "sound": "none" }),
    )?;
    Ok(())
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Right => "right",
        Direction::Down => "down",
    }
}

/// Connect, with the failure the operator can act on.
pub fn client() -> Result<Client> {
    Client::connect().context("herdr-stash needs a running Herdr session")
}
