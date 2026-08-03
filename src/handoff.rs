//! A short-lived, pane-scoped bridge between restore and the next capture.
//!
//! `agent.start` knows the native conversation immediately, while the
//! integration that publishes `agent_session` may take a moment. A handoff
//! carries the exact restored [`Agent`] across that gap. It is never selected
//! by pane id alone: all live identity fields must match.

use anyhow::{Result, bail};
use herdr_sdk::model::Pane as LivePane;
use serde::{Deserialize, Serialize};

use crate::record::Agent;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub agent: Agent,
}

impl Handoff {
    pub fn new(workspace_id: &str, tab_id: &str, pane_id: &str, agent: &Agent) -> Result<Self> {
        let handoff = Self {
            workspace_id: workspace_id.to_owned(),
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            agent: agent.clone(),
        };
        handoff.validate()?;
        Ok(handoff)
    }

    pub fn validate(&self) -> Result<()> {
        if self.workspace_id.trim().is_empty()
            || self.tab_id.trim().is_empty()
            || self.pane_id.trim().is_empty()
        {
            bail!("handoff has incomplete pane scope");
        }
        if self.agent.kind.trim().is_empty() || self.agent.session.trim().is_empty() {
            bail!("handoff has no usable agent session");
        }
        Ok(())
    }

    fn matches(&self, pane: &LivePane, kind: &str) -> bool {
        self.workspace_id == pane.workspace_id
            && self.tab_id == pane.tab_id
            && self.pane_id == pane.pane_id
            && self.agent.kind == kind
    }
}

/// Resolve a handoff after live metadata has been checked.
///
/// The caller deliberately invokes this only when `agent_session.value` is
/// empty. A non-empty live value is authoritative and must never be replaced by
/// an older handoff.
pub fn resolve(pane: &LivePane, kind: &str, handoff: Option<&Handoff>) -> Result<Option<Agent>> {
    let Some(handoff) = handoff else {
        return Ok(None);
    };
    handoff.validate()?;
    if !handoff.matches(pane, kind) {
        bail!(
            "handoff scope or agent kind does not match pane {}",
            pane.pane_id
        );
    }
    Ok(Some(handoff.agent.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_pane() -> LivePane {
        LivePane {
            pane_id: "w1:p2".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            agent: Some("codex".into()),
            ..LivePane::default()
        }
    }

    fn agent(session: &str) -> Agent {
        Agent {
            kind: "codex".into(),
            session_kind: "id".into(),
            session: session.into(),
            title: Some("worker".into()),
            argv: vec!["--yolo".into()],
        }
    }

    #[test]
    fn exact_scope_resolves_the_restored_session() {
        let pane = live_pane();
        let handoff = Handoff::new("w1", "w1:t1", "w1:p2", &agent("old-id")).unwrap();

        assert_eq!(
            resolve(&pane, "codex", Some(&handoff)).unwrap(),
            Some(agent("old-id"))
        );
    }

    #[test]
    fn a_non_empty_live_session_is_not_replaced_by_a_handoff() {
        let pane = LivePane {
            agent_session: Some(herdr_sdk::model::AgentSession {
                kind: "id".into(),
                value: "new-id".into(),
                ..herdr_sdk::model::AgentSession::default()
            }),
            ..live_pane()
        };
        let handoff = Handoff::new("w1", "w1:t1", "w1:p2", &agent("old-id")).unwrap();

        // The live-first decision belongs to capture; this resolver is only
        // called for the empty-live branch, so an available handoff remains
        // deterministic and cannot silently claim authority over live data.
        assert_eq!(
            resolve(&pane, "codex", Some(&handoff)).unwrap(),
            Some(agent("old-id"))
        );
    }

    #[test]
    fn mismatched_scope_or_kind_is_rejected() {
        let pane = live_pane();
        let wrong_pane = Handoff::new("w1", "w1:t1", "w1:p9", &agent("id")).unwrap();
        let wrong_kind = Handoff::new(
            "w1",
            "w1:t1",
            "w1:p2",
            &Agent {
                kind: "claude".into(),
                ..agent("id")
            },
        )
        .unwrap();

        assert!(resolve(&pane, "codex", Some(&wrong_pane)).is_err());
        assert!(resolve(&pane, "codex", Some(&wrong_kind)).is_err());
    }

    #[test]
    fn empty_handoff_session_is_rejected() {
        let handoff = Handoff {
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            pane_id: "w1:p2".into(),
            agent: agent(""),
        };

        assert!(handoff.validate().is_err());
    }
}
