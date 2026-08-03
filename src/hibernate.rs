//! Importing a conversation already parked by Herdr Hibernate.
//!
//! Hibernate replaces an agent with a tiny shell stub, so Herdr's live pane no
//! longer carries an agent or session. The durable record beside that stub does:
//! agent kind, native session id, and the whitelisted resume argv Hibernate
//! captured before stopping it. Reading that record lets Stash move the same
//! conversation into its own durable record without waking the agent first.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::herdr::ProcessInfo;
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

pub fn session(
    pane_id: &str,
    workspace_id: &str,
    tab_id: &str,
    process_info: Option<&ProcessInfo>,
) -> Result<Option<Session>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    let path = std::path::PathBuf::from(home).join(".config/herdr-hibernate/state.json");
    let state_error = match std::fs::read_to_string(&path) {
        Ok(body) => match parse(&body, pane_id, workspace_id, tab_id) {
            Ok(Some(session)) => return Ok(Some(session)),
            Ok(None) => None,
            Err(error) => Some(error),
        },
        Err(error) => Some(anyhow!("reading {}: {error}", path.display())),
    };

    if let Some(info) = process_info
        && let Some(stub) = expected_stub_path(pane_id)?
        && process_runs_stub(info, &stub)
    {
        let body = std::fs::read_to_string(&stub)
            .with_context(|| format!("reading generated Hibernate stub {}", stub.display()))?;
        return parse_stub(&body, pane_id)
            .with_context(|| format!("reading generated Hibernate stub for {pane_id}"))
            .map(Some);
    }

    match state_error {
        Some(error) => {
            Err(error).with_context(|| format!("reading Hibernate record for {pane_id}"))
        }
        None => Ok(None),
    }
}

fn expected_stub_path(pane_id: &str) -> Result<Option<PathBuf>> {
    if pane_id.is_empty()
        || pane_id
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-')))
    {
        return Ok(None);
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(Some(
        PathBuf::from(home)
            .join(".config/herdr-hibernate/panes")
            .join(pane_id.replace(':', "_") + ".sh"),
    ))
}

fn process_runs_stub(info: &ProcessInfo, expected: &Path) -> bool {
    let expected = expected.to_string_lossy();
    info.foreground_processes.iter().any(|process| {
        process.argv.as_ref().is_some_and(|argv| {
            argv.len() == 2 && is_shell(argv[0].as_str()) && argv[1] == expected.as_ref()
        }) || process.cmdline.as_deref().is_some_and(|cmdline| {
            let mut words = cmdline.split_whitespace();
            words.next().is_some_and(is_shell)
                && words.next() == Some(expected.as_ref())
                && words.next().is_none()
        })
    })
}

fn is_shell(program: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "bash" | "sh"))
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

fn parse_stub(body: &str, pane_id: &str) -> Result<Session> {
    let marker = body
        .lines()
        .find_map(|line| line.strip_prefix("# herdr-hibernate stub for pane "))
        .ok_or_else(|| anyhow!("generated Hibernate stub marker is missing"))?;
    let (stub_pane, id) = marker
        .split_once(" — session ")
        .ok_or_else(|| anyhow!("generated Hibernate stub marker is malformed"))?;
    if stub_pane != pane_id {
        bail!("generated Hibernate stub belongs to pane {stub_pane}, not {pane_id}");
    }
    if !is_uuid(id) {
        bail!("generated Hibernate stub has an invalid session id");
    }
    if !body
        .lines()
        .any(|line| line.trim() == "export HERDR_HIBERNATE_STUB=1")
    {
        bail!("generated Hibernate stub marker is missing");
    }

    let command = body
        .lines()
        .find_map(|line| line.strip_prefix("    exec "))
        .ok_or_else(|| anyhow!("generated Hibernate stub has no resume command"))?;
    let mut words = shell_words(command)?;
    let program = words
        .first()
        .and_then(|program| Path::new(program).file_name())
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("generated Hibernate stub has no agent command"))?;
    let agent = program.to_owned();
    words.remove(0);
    if !words.iter().any(|word| word == id) {
        bail!("generated Hibernate stub resume command does not name its session");
    }

    Ok(Session {
        kind: agent,
        id: id.to_owned(),
        argv: words,
        title: None,
    })
}

/// Parse only the single command form emitted by Hibernate. Shell operators,
/// substitutions, and other executable syntax are rejected rather than
/// interpreted or searched broadly.
fn shell_words(command: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                '$' | '`' => bail!("generated Hibernate stub has shell substitution"),
                _ => word.push(character),
            },
            Some(_) => bail!("generated Hibernate stub has an unsupported quote"),
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                ' ' | '\t' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                '$' | '`' | ';' | '|' | '&' | '<' | '>' | '(' | ')' => {
                    bail!("generated Hibernate stub has shell syntax")
                }
                _ => word.push(character),
            },
        }
    }
    if escaped || quote.is_some() {
        bail!("generated Hibernate stub has an unfinished shell word");
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_imports_the_matching_session_and_its_safe_resume_flags() {
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
    fn handoff_rejects_records_missing_tab_metadata() {
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
    fn handoff_accepts_legacy_record_without_workspace_id() {
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
    fn handoff_rejects_mismatched_scope_and_resume_metadata() {
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

    #[test]
    fn handoff_accepts_the_two_real_generated_stub_session_ids() {
        for (pane_id, id) in [
            ("wB:p1", "019fb885-2aec-7630-b03f-40a178ccbe77"),
            ("wB:p4", "019fbf6d-e5b4-7b53-951a-65c4c079a5d0"),
        ] {
            let body = format!(
                "# herdr-hibernate stub for pane {pane_id} — session {id}\nexport HERDR_HIBERNATE_STUB=1\n    exec codex resume {id} -s workspace-write\n"
            );
            let session = parse_stub(&body, pane_id).unwrap();
            assert_eq!(session.kind, "codex");
            assert_eq!(session.id, id);
            assert_eq!(session.argv, ["resume", id, "-s", "workspace-write"]);
        }
    }

    #[test]
    fn handoff_requires_the_exact_generated_stub_process_path() {
        let info = ProcessInfo {
            foreground_processes: vec![crate::herdr::Process {
                argv: Some(vec![
                    "bash".into(),
                    "/home/testycool/.config/herdr-hibernate/panes/wB_p1.sh".into(),
                ]),
                cmdline: Some("bash /home/testycool/.config/herdr-hibernate/panes/wB_p1.sh".into()),
                ..crate::herdr::Process::default()
            }],
            ..ProcessInfo::default()
        };
        let expected = Path::new("/home/testycool/.config/herdr-hibernate/panes/wB_p1.sh");
        let wrong = Path::new("/home/testycool/.config/herdr-hibernate/panes/wB_p4.sh");

        assert!(process_runs_stub(&info, expected));
        assert!(!process_runs_stub(&info, wrong));
    }
}
