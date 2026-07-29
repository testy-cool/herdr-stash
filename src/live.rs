//! The live half of the picker.
//!
//! A stash picker that only lists stashes can show one direction of one verb.
//! Work moves both ways — a workspace is parked, a stash is brought back — so the
//! popup reads the session as well as the stash directory, and the two columns are
//! the two states the same work can be in.
//!
//! One `session.snapshot` answers all of it: which workspaces exist, how many
//! panes each has, which agents are in them, which one the operator is standing
//! in, and which are mid-turn.

use anyhow::Result;
use herdr_sdk::Client;

use crate::herdr;

/// A workspace as it stands, described for a row in the left column.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub workspace_id: String,
    pub label: String,
    pub cwd: Option<String>,
    pub panes: usize,
    /// Agent kinds, in pane order, for the same badges the stashed side shows.
    pub agents: Vec<String>,
    /// Any agent mid-turn. Stashing is refused for these unless forced, so the
    /// row says so before the operator asks.
    pub working: bool,
    /// The workspace the operator is in. Stashing it is allowed — the popup is a
    /// session resource and survives its workspace closing, verified against
    /// 0.7.5 — but it is worth marking.
    pub current: bool,
}

pub fn list(client: &mut Client) -> Result<Vec<Live>> {
    let snapshot = herdr::snapshot(client)?;
    let focused = snapshot.focused_workspace_id.as_deref();

    Ok(snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let panes: Vec<_> = snapshot
                .panes
                .iter()
                .filter(|pane| pane.workspace_id == workspace.workspace_id)
                .collect();

            Live {
                workspace_id: workspace.workspace_id.clone(),
                label: label(workspace, &panes),
                cwd: panes.first().and_then(|pane| pane.cwd.clone()),
                panes: panes.len(),
                agents: panes
                    .iter()
                    .filter_map(|pane| pane.agent.clone())
                    .filter(|agent| !agent.is_empty())
                    .collect(),
                working: panes
                    .iter()
                    .any(|pane| pane.agent_status.as_deref() == Some("working")),
                current: focused == Some(workspace.workspace_id.as_str()),
            }
        })
        .collect())
}

/// What the sidebar calls it: the workspace's label, its directory's last
/// component, or its id — the same order [`crate::capture`] records.
fn label(workspace: &herdr::Workspace, panes: &[&herdr_sdk::model::Pane]) -> String {
    if let Some(label) = workspace
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
    {
        return label.to_owned();
    }
    panes
        .first()
        .and_then(|pane| pane.cwd.as_deref())
        .and_then(|cwd| cwd.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(&workspace.workspace_id)
        .to_owned()
}
