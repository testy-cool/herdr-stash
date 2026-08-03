//! Importing a conversation already parked by Herdr Hibernate.
//!
//! Hibernate replaces an agent with a tiny shell stub, so Herdr's live pane no
//! longer carries an agent or session. The durable record beside that stub does:
//! agent kind, native session id, and the whitelisted resume argv Hibernate
//! captured before stopping it. Reading that record lets Stash move the same
//! conversation into its own durable record without waking the agent first.

use std::{collections::HashMap, path::Path};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub kind: String,
    pub id: String,
    /// Original resume arguments without the executable. [`crate::record::Agent`]
    /// removes the stale resume reference and keeps Hibernate's safe replay flags.
    pub argv: Vec<String>,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Record {
    uuid: String,
    agent: String,
    resume: Vec<String>,
    #[serde(default)]
    agent_name: Option<String>,
    workspace_id: Option<String>,
    tab_id: String,
    #[serde(default)]
    label: Option<String>,
}

pub fn session(pane_id: &str, workspace_id: &str, tab_id: &str) -> Result<Option<Session>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    let path = std::path::PathBuf::from(home).join(".config/herdr-hibernate/state.json");
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse(&body, pane_id, workspace_id, tab_id)
        .with_context(|| format!("reading Hibernate record for {pane_id}"))
}

fn parse(body: &str, pane_id: &str, workspace_id: &str, tab_id: &str) -> Result<Option<Session>> {
    let mut records: HashMap<String, Record> =
        serde_json::from_str(body).context("parsing Hibernate state.json")?;
    let Some(record) = records.remove(pane_id) else {
        return Ok(None);
    };

    if record.agent.trim().is_empty() {
        bail!("Hibernate record has no agent kind");
    }
    if !is_uuid(&record.uuid) {
        bail!("Hibernate record has an invalid session id");
    }
    if record.resume.is_empty() {
        bail!("Hibernate record has no resume command");
    }
    if record.tab_id.trim().is_empty() {
        bail!("Hibernate record has no tab id");
    }
    if record.tab_id != tab_id {
        bail!(
            "Hibernate record belongs to tab {}, not {tab_id}",
            record.tab_id
        );
    }
    if let Some(recorded) = record.workspace_id.as_deref().filter(|id| !id.is_empty())
        && recorded != workspace_id
    {
        bail!("Hibernate record belongs to workspace {recorded}, not {workspace_id}");
    }

    let mut resume = record.resume;
    if let Some(program) = resume.first() {
        let executable = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        if executable != record.agent {
            bail!(
                "Hibernate record says agent {}, but its resume command starts {executable}",
                record.agent
            );
        }
        resume.remove(0);
    }
    if !resume.iter().any(|arg| arg == &record.uuid) {
        bail!("Hibernate resume command does not name its saved session");
    }

    Ok(Some(Session {
        kind: record.agent,
        id: record.uuid,
        argv: resume,
        title: record
            .agent_name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| record.label.filter(|label| !label.trim().is_empty())),
    }))
}

fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    [8, 4, 4, 4, 12].into_iter().all(|length| {
        parts
            .next()
            .is_some_and(|part| part.len() == length && part.bytes().all(|b| b.is_ascii_hexdigit()))
    }) && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_the_matching_session_and_its_safe_resume_flags() {
        let body = r#"{
            "wG:p9": {
                "uuid": "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                "agent": "codex",
                "agent_name": "worker",
                "workspace_id": "wG",
                "tab_id": "wG:t1",
                "resume": [
                    "codex", "resume", "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                    "-s", "workspace-write", "-a", "never"
                ]
            }
        }"#;

        let imported = parse(body, "wG:p9", "wG", "wG:t1").unwrap().unwrap();

        assert_eq!(imported.kind, "codex");
        assert_eq!(imported.id, "019fc697-75e1-7c63-a4d0-48fd9f2f7299");
        assert_eq!(imported.title.as_deref(), Some("worker"));
        assert_eq!(
            imported.argv,
            [
                "resume",
                "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                "-s",
                "workspace-write",
                "-a",
                "never"
            ]
        );
    }

    #[test]
    fn rejects_records_missing_tab_metadata() {
        let body = r#"{
            "wG:p9": {
                "uuid": "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                "agent": "codex",
                "resume": [
                    "codex", "resume", "019fc697-75e1-7c63-a4d0-48fd9f2f7299"
                ]
            }
        }"#;

        assert!(parse(body, "wG:p9", "wG", "wG:t1").is_err());
    }

    #[test]
    fn legacy_record_without_workspace_id_still_imports() {
        let body = r#"{
            "wG:p9": {
                "uuid": "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                "agent": "codex",
                "tab_id": "wG:t1",
                "resume": [
                    "codex", "resume", "019fc697-75e1-7c63-a4d0-48fd9f2f7299"
                ]
            }
        }"#;

        assert!(parse(body, "wG:p9", "other", "wG:t1").is_ok());
    }

    #[test]
    fn rejects_mismatched_scope_and_resume_metadata() {
        let body = r#"{
            "wG:p9": {
                "uuid": "019fc697-75e1-7c63-a4d0-48fd9f2f7299",
                "agent": "codex",
                "workspace_id": "wG",
                "tab_id": "wG:t1",
                "resume": [
                    "codex", "resume", "different-session"
                ]
            }
        }"#;

        assert!(parse(body, "wG:p9", "other", "wG:t1").is_err());
        assert!(parse(body, "wG:p9", "wG", "other").is_err());
        assert!(parse(body, "wG:p9", "wG", "wG:t1").is_err());
    }
}
