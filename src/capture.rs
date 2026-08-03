//! Turning a live workspace into a record, and only then closing it.
//!
//! The order is the safety property. Capture reads the whole session, rebuilds
//! every tab's tree, writes the file, and closes the workspace last — so a
//! workspace is never stopped on the strength of a record that does not exist
//! yet. Anything that cannot be captured aborts the stash with the workspace
//! untouched, which is why [`crate::layout::Layout::shape`] returns `None`
//! instead of a best guess.
//!
//! What a stash does not preserve, stated once so the picker and the README can
//! stop apologising for it: a pane running something that is not a recognised
//! agent comes back as a shell in its directory. Herdr has no way to resume an
//! arbitrary process, its own snapshot restore does not either, and a command
//! replayed blind — a migration, a deploy, a `rm` in a loop — is worse than a
//! prompt.

use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use herdr_sdk::Client;
use herdr_sdk::model::Pane as LivePane;

use crate::handoff;
use crate::herdr::{self, PluginPane, Snapshot};
use crate::layout::{Direction, Shape};
use crate::record::{Agent, Attached, Node, Pane, Stash, Tab, VERSION};

/// A captured workspace and the file it was written to.
pub struct Stashed {
    pub stash: Stash,
    pub path: std::path::PathBuf,
}

/// Capture `workspace_id`, write it, and close it.
///
/// `cwd` is the workspace's own directory, which the snapshot does not carry —
/// Herdr passes it to plugin actions as `workspace_cwd`, and the first pane's
/// directory is the fallback when this runs from somewhere without a context.
///
/// `force` gives up the one guarantee this plugin makes — see [`capture`].
pub fn stash(
    client: &mut Client,
    workspace_id: &str,
    cwd: Option<String>,
    force: bool,
) -> Result<Stashed> {
    let snapshot = herdr::snapshot(client).context("reading the session")?;
    let catalog = herdr::plugin_panes(client).unwrap_or_default();

    let stash = capture(client, &snapshot, &catalog, workspace_id, cwd, force)?;
    let path = crate::store::save(&stash).context("writing the stash")?;

    // Last, and only once the record is on disk.
    herdr::close_workspace(client, workspace_id).context("closing the stashed workspace")?;
    // A successful close consumes every restore bridge in that workspace. Do
    // this only after the stash is durable and the workspace is gone.
    let _ = crate::store::clear_workspace_handoffs(workspace_id);

    // ▲ Closing a workspace moves the active one, even when the closed workspace
    // was not it — upstream discussion #1328. Stashing one workspace must not
    // move the operator out of another, so where they were is put back.
    if let Some(focused) = snapshot.focused_workspace_id.as_deref()
        && focused != workspace_id
    {
        let _ = herdr::focus_workspace(client, focused);
    }

    Ok(Stashed { stash, path })
}

/// Build the record. Reads Herdr; changes nothing.
///
/// ▲ An agent that has no usable session in live metadata or a tightly scoped
/// restore handoff aborts the capture unless `force`. `agent.start` returns as
/// soon as the agent is ready for input, and its integration reports the
/// conversation a moment later, so the handoff closes that normal timing gap.
/// Recording a pane as a shell would turn a stash into the one thing it must
/// never be: a close that cannot be undone. Forcing it records the shell and says
/// so.
pub fn capture(
    client: &mut Client,
    snapshot: &Snapshot,
    catalog: &[PluginPane],
    workspace_id: &str,
    cwd: Option<String>,
    force: bool,
) -> Result<Stash> {
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .ok_or_else(|| anyhow!("no workspace {workspace_id} in this session"))?;

    let panes: HashMap<&str, &LivePane> = snapshot
        .panes
        .iter()
        .map(|pane| (pane.pane_id.as_str(), pane))
        .collect();

    let mut tabs: Vec<&herdr::Tab> = snapshot
        .tabs
        .iter()
        .filter(|tab| tab.workspace_id == workspace_id)
        .collect();
    tabs.sort_by_key(|tab| tab.number);
    if tabs.is_empty() {
        bail!("workspace {workspace_id} has no tabs");
    }

    let active_tab = workspace
        .active_tab_id
        .as_deref()
        .and_then(|active| tabs.iter().position(|tab| tab.tab_id == active))
        .unwrap_or(0);

    let mut recorded = Vec::with_capacity(tabs.len());
    for tab in &tabs {
        let layout = snapshot
            .layouts
            .iter()
            .find(|layout| layout.tab_id == tab.tab_id)
            .ok_or_else(|| anyhow!("no layout for tab {}", tab.tab_id))?;
        let shape = layout
            .shape()
            .ok_or_else(|| anyhow!("tab {} has a layout this version cannot read", tab.tab_id))?;

        let tab_cwd = layout
            .panes
            .first()
            .and_then(|placed| panes.get(placed.pane_id.as_str()))
            .and_then(|pane| pane.cwd.clone());

        let mut builder = Builder {
            client: &mut *client,
            panes: &panes,
            catalog,
            force,
            leaves: 0,
            attached: Vec::new(),
        };
        let slot = builder.build(&shape)?;
        let attached = builder.attached;
        let node = match slot {
            Slot::Node { node, .. } => node,
            // A tab holding nothing but plugin panes. It still has to come back
            // as something, and a shell in the tab's directory is the honest
            // minimum for the plugin panes to attach to.
            Slot::Plugins(_) => Node::Pane(Pane {
                cwd: tab_cwd.clone(),
                ..Pane::default()
            }),
        };

        recorded.push(Tab {
            label: tab.custom_label(),
            cwd: tab_cwd,
            layout: node,
            plugins: attached,
        });
    }

    let cwd = cwd.or_else(|| recorded.first().and_then(|tab| tab.cwd.clone()));

    let stashed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();

    Ok(Stash {
        version: VERSION,
        id: format!("{stashed_at}-{workspace_id}"),
        stashed_at,
        label: label(workspace, cwd.as_deref()),
        cwd,
        tabs: recorded,
        active_tab,
    })
}

/// What to call the stash: the workspace's label, or its directory's last
/// component, or its id. The same order the sidebar shows.
fn label(workspace: &herdr::Workspace, cwd: Option<&str>) -> String {
    if let Some(label) = workspace
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
    {
        return label.to_owned();
    }
    cwd.and_then(|cwd| cwd.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(&workspace.workspace_id)
        .to_owned()
}

/// One tab's conversion, carrying the two things a plain recursion cannot: how
/// many leaves have been recorded so far, and the plugin panes pruned out of the
/// tree on the way past.
struct Builder<'a, 'c> {
    client: &'c mut Client,
    panes: &'a HashMap<&'a str, &'a LivePane>,
    catalog: &'a [PluginPane],
    /// Record an unresumable agent as a shell instead of refusing.
    force: bool,
    leaves: usize,
    attached: Vec<Attached>,
}

/// What a subtree became.
enum Slot {
    Node {
        node: Node,
        /// Index of this subtree's first recorded leaf, which is what a plugin
        /// pane beside it anchors to.
        first_leaf: usize,
    },
    /// Nothing but plugin panes, still looking for a pane to attach to.
    Plugins(Vec<Pending>),
}

struct Pending {
    plugin: PluginPane,
    /// Set at the first merge that gives this pane a side; an inner plugin-only
    /// split has no side of its own to report.
    placement: Option<(Direction, bool)>,
}

impl Builder<'_, '_> {
    fn build(&mut self, shape: &Shape) -> Result<Slot> {
        match shape {
            Shape::Leaf(pane_id) => self.leaf(pane_id),
            Shape::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let left = self.build(first)?;
                let right = self.build(second)?;
                Ok(self.join(*direction, *ratio, left, right))
            }
        }
    }

    fn leaf(&mut self, pane_id: &str) -> Result<Slot> {
        // Copied out of the map, so nothing here keeps `self` borrowed while the
        // record is built.
        let pane: &LivePane = self
            .panes
            .get(pane_id)
            .copied()
            .ok_or_else(|| anyhow!("layout names pane {pane_id}, the snapshot does not"))?;

        if let Some(plugin) = self.owner(pane) {
            return Ok(Slot::Plugins(vec![Pending {
                plugin,
                placement: None,
            }]));
        }

        let first_leaf = self.leaves;
        self.leaves += 1;
        Ok(Slot::Node {
            node: Node::Pane(self.record(pane)?),
            first_leaf,
        })
    }

    /// Merge two subtrees, attaching any plugin panes to whichever side turned
    /// out to be real.
    fn join(&mut self, direction: Direction, ratio: f32, left: Slot, right: Slot) -> Slot {
        match (left, right) {
            (
                Slot::Node {
                    node: first,
                    first_leaf,
                },
                Slot::Node { node: second, .. },
            ) => Slot::Node {
                node: Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                },
                first_leaf,
            },
            (Slot::Node { node, first_leaf }, Slot::Plugins(pending)) => {
                self.attach(pending, direction, false, first_leaf);
                Slot::Node { node, first_leaf }
            }
            (Slot::Plugins(pending), Slot::Node { node, first_leaf }) => {
                self.attach(pending, direction, true, first_leaf);
                Slot::Node { node, first_leaf }
            }
            (Slot::Plugins(mut left), Slot::Plugins(right)) => {
                // Two plugin panes with no pane of their own between them. Both
                // keep looking; the outer merge will give them an anchor.
                for pane in right {
                    left.push(Pending {
                        placement: pane.placement.or(Some((direction, false))),
                        ..pane
                    });
                }
                Slot::Plugins(left)
            }
        }
    }

    fn attach(&mut self, pending: Vec<Pending>, direction: Direction, first: bool, anchor: usize) {
        for pane in pending {
            let (direction, first) = pane.placement.unwrap_or((direction, first));
            self.attached.push(Attached {
                plugin_id: pane.plugin.plugin_id,
                entrypoint: pane.plugin.entrypoint,
                title: pane.plugin.title,
                direction,
                first,
                anchor,
            });
        }
    }

    /// The plugin that owns this pane, if its label is a plugin pane's title.
    fn owner(&self, pane: &LivePane) -> Option<PluginPane> {
        let label = pane.label.as_deref()?;
        self.catalog
            .iter()
            .find(|entry| entry.title == label)
            .cloned()
    }

    fn record(&mut self, pane: &LivePane) -> Result<Pane> {
        let process_info = herdr::process_info(self.client, &pane.pane_id).ok();
        let hibernated = process_info
            .as_ref()
            .is_some_and(herdr::ProcessInfo::is_herdr_hibernate_stub);

        let agent = if hibernated {
            match crate::hibernate::session(
                &pane.pane_id,
                &pane.workspace_id,
                &pane.tab_id,
                process_info.as_ref(),
            ) {
                Ok(Some(session)) => {
                    let agent = Agent {
                        kind: session.kind,
                        session_kind: "id".into(),
                        session: session.id,
                        title: session
                            .title
                            .or_else(|| pane.terminal_title_stripped.clone()),
                        argv: session.argv,
                    };
                    if agent.launch().is_some() {
                        Some(agent)
                    } else if self.force {
                        None
                    } else {
                        bail!(
                            "Herdr Hibernate saved an unsupported {} session in {}",
                            agent.kind,
                            pane.pane_id
                        );
                    }
                }
                Ok(None) if self.force => None,
                Ok(None) => bail!(
                    "Herdr Hibernate has no recoverable session for {}",
                    pane.pane_id
                ),
                Err(_) if self.force => None,
                Err(error) => return Err(error),
            }
        } else {
            let kind = pane.agent.as_deref().filter(|kind| !kind.is_empty());
            let session = pane
                .agent_session
                .as_ref()
                .filter(|session| !session.value.is_empty());
            match (kind, session) {
                (Some(kind), Some(session)) => Some(Agent {
                    kind: kind.to_owned(),
                    session_kind: session.kind.clone(),
                    session: session.value.clone(),
                    title: pane
                        .tokens
                        .get("session_title")
                        .cloned()
                        .flatten()
                        .or_else(|| pane.terminal_title_stripped.clone()),
                    argv: argv(process_info.as_ref(), kind),
                }),
                (Some(kind), None) => {
                    let handoff = crate::store::find_handoff(
                        &pane.workspace_id,
                        &pane.tab_id,
                        &pane.pane_id,
                        kind,
                    );
                    match handoff {
                        Ok(handoff) => match handoff::resolve(pane, kind, handoff.as_ref())? {
                            Some(agent) => Some(agent),
                            None => {
                                match crate::native::discover(kind, pane, process_info.as_ref())? {
                                    Some(agent) => Some(agent),
                                    None if self.force => None,
                                    None => bail!(
                                        "{kind} in {} has no recoverable conversation, so stashing it would \
                                 close it for good — use the force action",
                                        pane.pane_id
                                    ),
                                }
                            }
                        },
                        Err(_error) if self.force => None,
                        Err(error) => return Err(error),
                    }
                }
                _ => None,
            }
        };

        Ok(Pane {
            cwd: pane.cwd.clone(),
            label: pane.label.clone(),
            focused: pane.focused,
            agent,
        })
    }
}

/// The agent's command line, without its program name — best effort, and
/// deliberately so.
///
/// ▲ The flags an agent was launched with — `--model`,
/// `--dangerously-skip-permissions` — are recorded nowhere Herdr persists, so a
/// restart drops them. That is upstream's gap as much as this plugin's: Herdr's
/// own restore resumes the conversation and launches it bare. `pane.process_info`
/// is the only place they still exist, in the live process.
///
/// ▲ And it does not always have them. Measured against 0.7.5: a `zsh` pane
/// reports `argv: ["-zsh"]`, while a running pi reports `argv0: "pi"` with `argv`
/// and `cmdline` both null, because pi rewrites its process title over its own
/// argv. So this yields the flags when the OS kept them and nothing when it did
/// not — and never guesses, because a captured line is only used when its program
/// name is still the agent's own.
fn argv(info: Option<&herdr::ProcessInfo>, kind: &str) -> Vec<String> {
    let Some(info) = info else {
        return Vec::new();
    };
    info.agent_argv(kind).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Workspace;

    #[test]
    fn a_labelled_workspace_keeps_its_label() {
        let workspace = Workspace {
            workspace_id: "w6".into(),
            label: Some("Access".into()),
            active_tab_id: None,
        };
        assert_eq!(
            label(&workspace, Some("/Users/victor/workspace/access")),
            "Access"
        );
    }

    #[test]
    fn an_unlabelled_workspace_is_named_after_its_directory() {
        let workspace = Workspace {
            workspace_id: "w6".into(),
            label: None,
            active_tab_id: None,
        };
        assert_eq!(
            label(&workspace, Some("/Users/victor/workspace/access")),
            "access"
        );
    }

    #[test]
    fn a_workspace_with_neither_falls_back_to_its_id() {
        let workspace = Workspace {
            workspace_id: "w6".into(),
            label: Some("   ".into()),
            active_tab_id: None,
        };
        assert_eq!(label(&workspace, None), "w6");
    }
}
