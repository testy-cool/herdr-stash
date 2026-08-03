//! What a stash is, on disk.
//!
//! One JSON file per stash, and everything needed to put the workspace back:
//! its label and directory, one tree per tab, and per pane the directory it was
//! in, the agent that was running, and the conversation that agent was in.
//!
//! The conversation reference is the part that makes any of this worth doing.
//! Herdr's integrations report it — a transcript path for pi and omp, a native
//! id for Claude — and it is stable across panes, so an agent killed here can be
//! started elsewhere later in the same conversation. Restoring is `agent.start`
//! with that reference on the command line, which is what Herdr's own
//! `resume_agents_on_restore` does after a server restart.
//!
//! ▲ `version` is checked on read, not on write: a record written by a later
//! version of this plugin is skipped with a message rather than half-understood.

use serde::{Deserialize, Serialize};

use crate::layout::Direction;

/// The only format this plugin writes, and the only one it restores.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stash {
    pub version: u32,
    /// Stable, sortable, and unique per stash: the epoch second it was taken,
    /// then the workspace it came from.
    pub id: String,
    /// Seconds since the epoch. Formatted for display at read time rather than
    /// stored formatted, so a record does not carry a rendering decision.
    pub stashed_at: u64,
    /// What the sidebar called it: the workspace's own label when it had one,
    /// otherwise the last component of its directory.
    pub label: String,
    pub cwd: Option<String>,
    pub tabs: Vec<Tab>,
    /// Which tab was active. Restore focuses it last.
    pub active_tab: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub layout: Node,
    /// The panes other plugins own, held outside the tree. See [`Attached`].
    #[serde(default)]
    pub plugins: Vec<Attached>,
}

/// The tab's tree with the panes' contents in the leaves.
///
/// The same shape as [`crate::layout::Shape`], which carries pane ids instead —
/// that one describes a live tab, this one describes a stopped one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    Pane(Pane),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pane {
    /// Where the pane was started. Restore puts the new pane here, so a shell
    /// comes back in the directory the work was in even when nothing else about
    /// the pane can be recovered.
    pub cwd: Option<String>,
    /// The pane's label, when the operator set one.
    pub label: Option<String>,
    pub focused: bool,
    pub agent: Option<Agent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// Herdr's own kind name — `pi`, `claude`, `omp` — which is what
    /// `agent.start` accepts.
    pub kind: String,
    /// `path` for a transcript file, `id` for a native conversation id.
    pub session_kind: String,
    pub session: String,
    /// What the agent called itself, for the picker to show. Never used to
    /// restore anything.
    pub title: Option<String>,
    /// The agent's original command line, without its program name, when it
    /// could be read. Best effort by design — see [`crate::capture::argv`].
    #[serde(default)]
    pub argv: Vec<String>,
}

/// A pane another plugin owns, recorded beside the tree instead of inside it.
///
/// `plugin.pane.open` is the only way to put one back, and it takes a target
/// pane and a direction rather than a place in a tree — so a plugin pane cannot
/// be a leaf that restore plants. It is pruned at capture and reopened against
/// the pane it sat beside, which is what the operator's own keybinding does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attached {
    pub plugin_id: String,
    pub entrypoint: String,
    /// The manifest title, which is both how the pane was recognised and what
    /// the picker shows.
    pub title: String,
    /// How the divide ran where this pane sat.
    pub direction: Direction,
    /// Whether it was the first child — the left or upper side. Herdr can only
    /// split rightwards and downwards, so restoring a left-hand sidebar means
    /// opening it on the right and swapping the two panes.
    pub first: bool,
    /// Which recorded pane it sat beside, as an index into the tab's leaves in
    /// tree order. An index rather than an id because the panes it refers to do
    /// not exist yet when the record is written.
    pub anchor: usize,
}

impl Agent {
    /// The arguments that resume this conversation, ready for `agent.start`.
    ///
    /// The flag per agent is Herdr's own table from its session-restore
    /// documentation, because matching what Herdr does after a server restart is
    /// the whole point: a stash should be no worse than a restart, and the flag
    /// Herdr uses is the one the agent's own integration reports against.
    ///
    /// An agent whose kind is not in that table returns `None`, and restore
    /// leaves that pane a plain shell in its directory rather than guessing a
    /// flag and starting a fresh conversation on top of an old one.
    pub fn resume(&self) -> Option<Vec<String>> {
        let session = self.session.clone();
        let args = match self.kind.as_str() {
            "pi" | "kimi" | "opencode" | "kilo" => vec!["--session".into(), session],
            "claude" | "cursor" | "devin" | "droid" | "grok" | "hermes" | "qodercli" => {
                vec!["--resume".into(), session]
            }
            "omp" => vec![format!("--resume={session}")],
            "copilot" => vec![format!("--resume={session}")],
            "codex" => vec!["resume".into(), session],
            "mastracode" => vec!["--thread".into(), session],
            _ => return None,
        };
        Some(args)
    }

    /// Everything to pass `agent.start`: the flags the agent was launched with,
    /// then the resume reference.
    ///
    /// The recorded argv comes first because that is where it was — a flag like
    /// `--model` belongs before the positional the resume form may use. Any
    /// resume flag already present in the captured argv is dropped from it: the
    /// captured one names the conversation this pane was resumed *into* last
    /// time, which may not be the one it ended in.
    pub fn launch(&self) -> Option<Vec<String>> {
        let resume = self.resume()?;
        let head = resume.first().cloned().unwrap_or_default();
        let stem = head.split('=').next().unwrap_or(&head).to_owned();

        let mut args = Vec::new();
        let mut skip = false;
        for arg in &self.argv {
            if skip {
                skip = false;
                continue;
            }
            if arg == &stem {
                // Either a flag whose value is the next argument, or a
                // subcommand whose positional is. Both are consumed; only the
                // inline `--flag=value` form carries its value with it.
                skip = !head.contains('=');
                continue;
            }
            if arg.starts_with(&format!("{stem}=")) || arg == &self.session {
                continue;
            }
            args.push(arg.clone());
        }
        args.extend(resume);
        Some(args)
    }
}

impl Stash {
    pub fn panes(&self) -> Vec<&Pane> {
        let mut found = Vec::new();
        for tab in &self.tabs {
            walk(&tab.layout, &mut found);
        }
        found
    }

    /// The agents this stash would restore, in tree order.
    pub fn agents(&self) -> Vec<&Agent> {
        self.panes()
            .into_iter()
            .filter_map(|pane| pane.agent.as_ref())
            .collect()
    }
}

fn walk<'a>(node: &'a Node, found: &mut Vec<&'a Pane>) {
    match node {
        Node::Pane(pane) => found.push(pane),
        Node::Split { first, second, .. } => {
            walk(first, found);
            walk(second, found);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: &str, argv: &[&str]) -> Agent {
        Agent {
            kind: kind.into(),
            session_kind: "path".into(),
            session: "/tmp/session.jsonl".into(),
            title: None,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn pi_resumes_by_transcript_path() {
        assert_eq!(
            agent("pi", &[]).launch(),
            Some(vec!["--session".into(), "/tmp/session.jsonl".into()])
        );
    }

    #[test]
    fn omp_resumes_with_an_inline_value() {
        assert_eq!(
            agent("omp", &[]).launch(),
            Some(vec!["--resume=/tmp/session.jsonl".into()])
        );
    }

    #[test]
    fn codex_resumes_through_a_subcommand() {
        assert_eq!(
            agent("codex", &[]).launch(),
            Some(vec!["resume".into(), "/tmp/session.jsonl".into()])
        );
    }

    #[test]
    fn grok_resumes_with_its_native_flag() {
        assert_eq!(
            agent("grok", &[]).launch(),
            Some(vec!["--resume".into(), "/tmp/session.jsonl".into()])
        );
    }

    /// The reason argv is captured at all: a flag the operator launched with is
    /// not recoverable from anywhere else, and losing it silently downgrades the
    /// resumed agent.
    #[test]
    fn captured_flags_are_kept_and_come_first() {
        assert_eq!(
            agent("claude", &["--dangerously-skip-permissions"]).launch(),
            Some(vec![
                "--dangerously-skip-permissions".into(),
                "--resume".into(),
                "/tmp/session.jsonl".into()
            ])
        );
    }

    #[test]
    fn hibernate_replay_flags_survive_codex_resumption() {
        let mut agent = agent(
            "codex",
            &[
                "resume",
                "/tmp/old-session",
                "--model",
                "gpt-5",
                "-c",
                "model_reasoning_effort=xhigh",
                "--sandbox",
                "workspace-write",
                "-a",
                "never",
                "--yolo",
                "-c",
                "mcp_servers.backlog.enabled=true",
            ],
        );
        agent.session = "/tmp/new-session".into();

        assert_eq!(
            agent.launch(),
            Some(vec![
                "--model".into(),
                "gpt-5".into(),
                "-c".into(),
                "model_reasoning_effort=xhigh".into(),
                "--sandbox".into(),
                "workspace-write".into(),
                "-a".into(),
                "never".into(),
                "--yolo".into(),
                "-c".into(),
                "mcp_servers.backlog.enabled=true".into(),
                "resume".into(),
                "/tmp/new-session".into(),
            ])
        );
    }

    /// A pane that was itself restored carries the previous resume flag in its
    /// argv. Keeping it would name a stale conversation ahead of the current one.
    #[test]
    fn a_stale_resume_flag_in_the_captured_argv_is_dropped() {
        let mut agent = agent("pi", &["--session", "/tmp/old.jsonl", "--model", "opus"]);
        agent.session = "/tmp/new.jsonl".into();
        assert_eq!(
            agent.launch(),
            Some(vec![
                "--model".into(),
                "opus".into(),
                "--session".into(),
                "/tmp/new.jsonl".into()
            ])
        );
    }

    #[test]
    fn an_inline_stale_value_is_dropped_too() {
        let mut agent = agent("omp", &["--resume=/tmp/old.jsonl", "--verbose"]);
        agent.session = "/tmp/new.jsonl".into();
        assert_eq!(
            agent.launch(),
            Some(vec!["--verbose".into(), "--resume=/tmp/new.jsonl".into()])
        );
    }

    /// Codex resumes through a subcommand rather than a flag, so the stale value
    /// to drop is a bare positional. Keeping it would leave `codex <old-id>
    /// resume <new-id>` on the command line.
    #[test]
    fn a_stale_subcommand_positional_is_dropped() {
        let mut agent = agent("codex", &["resume", "old-id", "--search"]);
        agent.session = "new-id".into();
        assert_eq!(
            agent.launch(),
            Some(vec!["--search".into(), "resume".into(), "new-id".into()])
        );
    }

    /// An agent kind with no documented resume form restores as a shell, and the
    /// caller can tell because there is nothing to launch.
    #[test]
    fn an_unknown_kind_has_no_resume_form() {
        assert_eq!(agent("gemini", &[]).launch(), None);
    }
}
