//! Putting a stash back.
//!
//! Three passes, in this order, because each one would disturb the last:
//!
//! 1. **Shape.** Create the workspace and its tabs, then replay every split.
//!    Ratios go in exactly as recorded — `pane.split`'s ratio is the existing
//!    pane's share, which is the same convention the layout publishes.
//! 2. **Agents.** `agent.start` requires its target pane to be sitting at a
//!    prompt, so no agent may start while splits are still landing. Each one
//!    blocks until Herdr reports it ready, which is why this pass reports
//!    progress instead of running silently.
//! 3. **Plugin panes.** They change the layout, so they come after the shape is
//!    final. A pane that was on the left is opened on the right and swapped,
//!    because Herdr splits rightwards and downwards only.
//!
//! Focus is last of all, and it is the only thing here that moves the operator.

use std::time::Duration;

use anyhow::{Context as _, Result};
use herdr_sdk::Client;

use crate::herdr;
use crate::record::{Agent, Attached, Node, Pane, Stash};

/// How long a freshly split pane is given to reach its prompt. Generous because
/// the cost of being wrong is an agent that does not come back, and a shell that
/// takes seconds to start is a real configuration rather than a broken one.
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);

/// What the caller is told as a restore proceeds, so a UI can show it and a
/// terminal can print it.
#[derive(Debug, Clone)]
pub enum Step {
    Shape {
        tabs: usize,
    },
    Agent {
        index: usize,
        total: usize,
        label: String,
    },
    Plugin {
        title: String,
    },
    Focused,
}

/// Restore `stash` into a new workspace, reporting each step.
///
/// `focus` moves the operator into the result. True for a single restore, which is
/// a request to be there; false in a batch, where "the last one built" is an
/// arbitrary place to be dropped and the caller puts focus back where it was.
///
/// Everything a step cannot do is collected rather than thrown: a stash with one
/// dead transcript should come back missing one agent, not fail as a whole and
/// leave a half-built workspace nobody asked for. The warnings are returned for
/// the caller to show.
pub fn restore(
    client: &mut Client,
    stash: &Stash,
    focus: bool,
    mut report: impl FnMut(Step),
) -> Result<Restored> {
    let mut warnings = Vec::new();

    report(Step::Shape {
        tabs: stash.tabs.len(),
    });

    let (workspace_id, root) = herdr::create_workspace(client, stash.cwd.as_deref(), &stash.label)
        .context("creating the workspace")?;

    // Per tab: the panes it planted, in the same tree order capture numbered
    // them, so a plugin pane's anchor index still names the pane it sat beside.
    let mut planted: Vec<Vec<Planted>> = Vec::with_capacity(stash.tabs.len());
    let mut tab_roots = Vec::with_capacity(stash.tabs.len());

    for (index, tab) in stash.tabs.iter().enumerate() {
        let pane = match index {
            // The workspace came with one.
            0 => root.clone(),
            _ => herdr::create_tab(
                client,
                &workspace_id,
                tab.cwd.as_deref(),
                tab.label.as_deref(),
            )
            .with_context(|| format!("creating tab {}", index + 1))?,
        };
        // Herdr labels a tab it creates with its number; a recorded label is only
        // ever a name the operator chose, so the first tab needs it applied too.
        tab_roots.push(pane.clone());

        let mut leaves = Vec::new();
        plant(client, &tab.layout, &pane, &mut leaves)?;
        planted.push(leaves);
    }

    // Labels before agents: renaming a pane is instant, starting an agent is not,
    // and a workspace that shows its pane names while the agents come up reads as
    // progress rather than as a stall.
    for planted in planted.iter().flatten() {
        if let Some(label) = planted.pane.label.as_deref().filter(|l| !l.is_empty()) {
            let _ = herdr::rename_pane(client, &planted.pane_id, label);
        }
    }

    let mut taken = herdr::agent_names(client).unwrap_or_default();
    let total = stash.agents().len();
    let mut started = 0;
    let mut resumed = 0;

    for leaves in &planted {
        for planted in leaves {
            let Some(agent) = planted.pane.agent.as_ref() else {
                continue;
            };
            started += 1;
            report(Step::Agent {
                index: started,
                total,
                label: agent.title.clone().unwrap_or_else(|| agent.kind.clone()),
            });

            let Some(args) = launch_args(agent) else {
                warnings.push(format!(
                    "{}: no resume form for this agent kind — left as a shell",
                    agent.kind
                ));
                continue;
            };
            if agent.session_kind == "path" && !std::path::Path::new(&agent.session).exists() {
                warnings.push(format!(
                    "{}: transcript is gone — left as a shell",
                    agent.kind
                ));
                continue;
            }

            let name = free_name(&agent.kind, &mut taken);
            match start(client, &name, &agent.kind, &planted.pane_id, &args) {
                Ok(()) => resumed += 1,
                Err(error) => warnings.push(format!("{}: {error}", agent.kind)),
            }
        }
    }

    for (tab, leaves) in stash.tabs.iter().zip(&planted) {
        for attached in &tab.plugins {
            report(Step::Plugin {
                title: attached.title.clone(),
            });
            if let Err(error) = reopen(client, attached, leaves) {
                warnings.push(format!("{}: {error}", attached.title));
            }
        }
    }

    // The recorded focus, then the workspace itself.
    if focus {
        if let Some(pane_id) = planted
            .get(stash.active_tab)
            .and_then(|leaves| leaves.iter().find(|planted| planted.pane.focused))
            .map(|planted| planted.pane_id.clone())
            .or_else(|| tab_roots.get(stash.active_tab).cloned())
        {
            let _ = herdr::focus_pane(client, &pane_id);
        }
        herdr::focus_workspace(client, &workspace_id).context("focusing the restored workspace")?;
        report(Step::Focused);
    }

    Ok(Restored {
        workspace_id,
        agents: resumed,
        warnings,
    })
}

const MCP_USE_DEFAULT: &str = "--mcp-use-default";

/// Add the local MCP-launcher bypass required for automatic Claude and Codex
/// startup, while leaving the recorded/native resume contract untouched for
/// every other agent kind.
fn launch_args(agent: &Agent) -> Option<Vec<String>> {
    let mut args = agent.launch()?;
    if !matches!(agent.kind.as_str(), "claude" | "codex") {
        return Some(args);
    }

    args.retain(|arg| arg != MCP_USE_DEFAULT);
    args.insert(0, MCP_USE_DEFAULT.to_owned());
    Some(args)
}

pub struct Restored {
    pub workspace_id: String,
    pub agents: usize,
    pub warnings: Vec<String>,
}

/// Start an agent in a pane that may not be ready for one yet.
///
/// ▲ Two conditions, and only one of them is observable. A pane with a command
/// running is ruled out by [`herdr::ProcessInfo::at_prompt`]. The other is
/// Herdr's own: `agent.start` answers `agent_pane_busy` — *is not an available
/// shell* — until it has seen the pane's prompt, which no field or event
/// reports. Measured against 0.7.5, a pane split and started in the same breath
/// fails, and the same pane accepts an agent about a second later.
///
/// So the start itself is the readiness probe. A real failure — a missing
/// binary, a bad flag — fails every attempt and is reported as it stands, so
/// retrying costs a few seconds in the case that was going to fail anyway.
fn start(
    client: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<()> {
    let deadline = std::time::Instant::now() + SHELL_TIMEOUT;
    let mut last;
    loop {
        herdr::wait_for_shell(client, pane_id, SHELL_TIMEOUT);
        match herdr::start_agent(client, name, kind, pane_id, args) {
            Ok(()) => return Ok(()),
            Err(error) => last = error,
        }
        if std::time::Instant::now() >= deadline {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// One recorded pane and the pane that now stands for it.
struct Planted<'a> {
    pane: &'a Pane,
    pane_id: String,
}

/// Replay one tree into `pane_id`, which already exists and becomes the first
/// leaf of whatever is planted in it.
fn plant<'a>(
    client: &mut Client,
    node: &'a Node,
    pane_id: &str,
    leaves: &mut Vec<Planted<'a>>,
) -> Result<()> {
    match node {
        Node::Pane(pane) => {
            leaves.push(Planted {
                pane,
                pane_id: pane_id.to_owned(),
            });
            Ok(())
        }
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            // The new pane is always the second child, and the directory is the
            // one that child was recorded in — a shell that comes back in the
            // wrong directory is the failure this plugin is least allowed to have.
            let cwd = first_cwd(second);
            let new = herdr::split(client, pane_id, *direction, *ratio, cwd.as_deref())
                .context("splitting a restored pane")?;
            plant(client, first, pane_id, leaves)?;
            plant(client, second, &new, leaves)
        }
    }
}

/// The directory of a subtree's first pane, which is the pane a split creates.
fn first_cwd(node: &Node) -> Option<String> {
    match node {
        Node::Pane(pane) => pane.cwd.clone(),
        Node::Split { first, .. } => first_cwd(first),
    }
}

/// Reopen a plugin pane beside the pane it sat next to.
fn reopen(client: &mut Client, attached: &Attached, leaves: &[Planted<'_>]) -> Result<()> {
    let anchor = leaves
        .get(attached.anchor)
        .or_else(|| leaves.first())
        .context("no pane to attach to")?;

    let opened = herdr::open_plugin_pane(
        client,
        &attached.plugin_id,
        &attached.entrypoint,
        &anchor.pane_id,
        attached.direction,
    )?;

    // Herdr has no `left` and no `up`: a pane that was the first child comes
    // back on the wrong side, and a swap is the only way to put it back. Cheap,
    // exact, and the reason the side is recorded at all.
    if attached.first {
        herdr::swap_panes(client, &opened, &anchor.pane_id)?;
    }
    Ok(())
}

/// An agent name Herdr will accept: its kind, then a number if that is taken.
///
/// Herdr requires `[a-z][a-z0-9_-]{0,31}` and uniqueness among live agents, and
/// it rejects the whole `agent.start` on a collision — so the names already in
/// use are read once and reserved as they are handed out.
fn free_name(kind: &str, taken: &mut Vec<String>) -> String {
    let stem: String = kind
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
        .collect();
    let stem = match stem.chars().next() {
        Some(first) if first.is_ascii_lowercase() => stem,
        _ => format!("a{stem}"),
    };

    let mut candidate = stem.clone();
    let mut suffix = 1;
    while taken.iter().any(|name| name == &candidate) {
        suffix += 1;
        candidate = format!("{stem}{suffix}");
    }
    taken.push(candidate.clone());
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: &str, argv: &[&str]) -> Agent {
        Agent {
            kind: kind.into(),
            session_kind: "id".into(),
            session: "session-id".into(),
            title: None,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn mcp_launcher_bypass_is_first_for_claude_and_codex() {
        for kind in ["claude", "codex"] {
            let args = launch_args(&agent(kind, &[])).unwrap();
            assert_eq!(args.first().map(String::as_str), Some(MCP_USE_DEFAULT));
            assert_eq!(args[1..], agent(kind, &[]).launch().unwrap());
        }
    }

    #[test]
    fn mcp_launcher_bypass_is_absent_for_other_agents() {
        for kind in ["grok", "pi", "omp"] {
            let args = launch_args(&agent(kind, &[])).unwrap();
            assert!(!args.iter().any(|arg| arg == MCP_USE_DEFAULT));
        }
    }

    #[test]
    fn mcp_launcher_bypass_is_not_duplicated() {
        let args = launch_args(&agent(
            "claude",
            &["--mcp-use-default", "--resume", "old", "--mcp-use-default"],
        ))
        .unwrap();

        assert_eq!(args, ["--mcp-use-default", "--resume", "session-id",]);
    }

    #[test]
    fn a_free_name_is_just_the_kind() {
        let mut taken = vec!["reviewer".to_owned()];
        assert_eq!(free_name("pi", &mut taken), "pi");
    }

    #[test]
    fn a_taken_name_gains_a_number_and_is_reserved() {
        let mut taken = vec!["pi".to_owned()];
        assert_eq!(free_name("pi", &mut taken), "pi2");
        // Reserved as it is handed out, so two agents of one kind cannot collide.
        assert_eq!(free_name("pi", &mut taken), "pi3");
    }

    /// Herdr's name grammar starts with a letter. A kind that somehow does not is
    /// prefixed rather than rejected — the agent still has to be started.
    #[test]
    fn a_name_always_starts_with_a_letter() {
        let mut taken = Vec::new();
        assert_eq!(free_name("2fast", &mut taken), "a2fast");
    }

    #[test]
    fn the_split_directory_is_the_new_panes_own() {
        let node = Node::Split {
            direction: crate::layout::Direction::Right,
            ratio: 0.5,
            first: Box::new(Node::Pane(Pane {
                cwd: Some("/left".into()),
                ..Pane::default()
            })),
            second: Box::new(Node::Split {
                direction: crate::layout::Direction::Down,
                ratio: 0.5,
                first: Box::new(Node::Pane(Pane {
                    cwd: Some("/right-top".into()),
                    ..Pane::default()
                })),
                second: Box::new(Node::Pane(Pane::default())),
            }),
        };
        let Node::Split { second, .. } = &node else {
            panic!("a split");
        };
        assert_eq!(first_cwd(second), Some("/right-top".into()));
    }
}
