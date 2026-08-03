//! Exact native conversation discovery from the live agent process.
//!
//! Herdr's live `agent_session` can lag behind an already-running agent. This
//! module closes that gap without searching a workspace or choosing a recent
//! file: it accepts the exact native resume id in the pane's exact foreground
//! agent argv, or one transcript held open by that same PID under the native
//! agent root.

use std::{
    fs::{self, File},
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use herdr_sdk::model::Pane as LivePane;
use serde::Deserialize;

use crate::{herdr::ProcessInfo, record::Agent};

#[derive(Debug, Clone)]
struct OpenFd {
    fd: u32,
    link: PathBuf,
    read: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Transcript {
    id: String,
}

/// Discover a conversation only from the recognized agent process in this
/// pane's foreground process set.
pub fn discover(
    kind: &str,
    pane: &LivePane,
    process_info: Option<&ProcessInfo>,
) -> Result<Option<Agent>> {
    if !matches!(kind, "codex" | "claude") {
        return Ok(None);
    }
    let Some(process_info) = process_info else {
        return Ok(None);
    };
    let Some(process) = process_info.agent_process(kind) else {
        return Ok(None);
    };
    let pid = process.pid;
    let argv = process_info.agent_argv(kind).unwrap_or_default();

    if let Some(id) = resume_id(kind, &argv)? {
        return Ok(Some(agent(kind, id, pane, argv)));
    }

    let root = transcript_root(kind)?;
    let fds = open_fds(pid)?;
    discover_from_fds(kind, pane, &argv, &root, &fds)
}

fn agent(kind: &str, id: String, pane: &LivePane, argv: Vec<String>) -> Agent {
    Agent {
        kind: kind.to_owned(),
        session_kind: "id".into(),
        session: id,
        title: pane
            .tokens
            .get("session_title")
            .cloned()
            .flatten()
            .or_else(|| pane.terminal_title_stripped.clone()),
        argv,
    }
}

fn resume_id(kind: &str, argv: &[String]) -> Result<Option<String>> {
    let mut ids = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        let candidate = match kind {
            "codex" if arg == "resume" => {
                index += 1;
                argv.get(index).cloned()
            }
            "claude" if arg == "--resume" => {
                index += 1;
                argv.get(index).cloned()
            }
            "claude" => arg.strip_prefix("--resume=").map(str::to_owned),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if !is_uuid(&candidate) {
                bail!("{kind} has an invalid native resume id in its foreground argv");
            }
            if !ids.contains(&candidate) {
                ids.push(candidate);
            }
        } else if kind == "codex" && arg == "resume" {
            bail!("codex has no native resume id after resume in its foreground argv");
        } else if kind == "claude" && arg == "--resume" {
            bail!("claude has no native resume id after --resume in its foreground argv");
        }
        index += 1;
    }
    if ids.len() > 1 {
        bail!("{kind} foreground argv names multiple native resume ids");
    }
    Ok(ids.into_iter().next())
}

fn transcript_root(kind: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    match kind {
        "codex" => Ok(PathBuf::from(home).join(".codex/sessions")),
        "claude" => Ok(PathBuf::from(home).join(".claude/projects")),
        _ => bail!("unsupported native agent kind {kind}"),
    }
}

fn open_fds(pid: u32) -> Result<Vec<OpenFd>> {
    let dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let mut fds = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let fd = entry.file_name().to_string_lossy().parse::<u32>().ok();
        let Some(fd) = fd else {
            continue;
        };
        let read = entry.path();
        let link = match fs::read_link(&read) {
            Ok(link) => link,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", read.display()));
            }
        };
        fds.push(OpenFd { fd, link, read });
    }
    fds.sort_by_key(|fd| fd.fd);
    Ok(fds)
}

fn discover_from_fds(
    kind: &str,
    pane: &LivePane,
    argv: &[String],
    root: &Path,
    fds: &[OpenFd],
) -> Result<Option<Agent>> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("canonicalizing native transcript root {}", root.display()))?;
    let mut found: Option<Transcript> = None;

    for fd in fds {
        let Some(path) = candidate_path(&fd.link, &root) else {
            continue;
        };
        let transcript = parse_transcript(kind, pane, &path, &fd.read)
            .with_context(|| format!("reading native transcript on fd {}", fd.fd))?;
        if let Some(previous) = &found {
            if previous.id != transcript.id {
                bail!("native agent process has multiple recoverable transcripts");
            }
        } else {
            found = Some(transcript);
        }
    }

    Ok(found.map(|transcript| agent(kind, transcript.id, pane, argv.to_vec())))
}

fn candidate_path(link: &Path, root: &Path) -> Option<PathBuf> {
    if link.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return None;
    }
    let canonical = fs::canonicalize(link).ok()?;
    if canonical == root || !canonical.starts_with(root) {
        return None;
    }
    Some(canonical)
}

#[derive(Debug, Deserialize)]
struct CodexRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Debug, Deserialize)]
struct CodexPayload {
    id: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    originator: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeRecord {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
}

fn parse_transcript(
    kind: &str,
    pane: &LivePane,
    path: &Path,
    read_path: &Path,
) -> Result<Transcript> {
    let file = File::open(read_path).with_context(|| format!("opening {}", read_path.display()))?;
    match kind {
        "codex" => parse_codex(pane, path, file),
        "claude" => parse_claude(pane, path, file),
        _ => bail!("unsupported native agent kind {kind}"),
    }
}

fn parse_codex(pane: &LivePane, path: &Path, file: File) -> Result<Transcript> {
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let record: CodexRecord = serde_json::from_str(&line).context("parsing Codex JSONL")?;
        if record.record_type.as_deref() != Some("session_meta") {
            continue;
        }
        let payload = record
            .payload
            .context("Codex session metadata has no payload")?;
        let id = payload
            .id
            .as_deref()
            .or(payload.session_id.as_deref())
            .context("Codex session metadata has no session id")?
            .to_owned();
        if let (Some(id), Some(session_id)) = (&payload.id, &payload.session_id)
            && id != session_id
        {
            bail!("Codex session metadata has mismatched session ids");
        }
        let cwd = payload
            .cwd
            .as_deref()
            .context("Codex session metadata has no cwd")?;
        if !matches!(
            payload.originator.as_deref(),
            Some("codex") | Some("codex-tui")
        ) {
            bail!("Codex session metadata has a mismatched agent kind");
        }
        validate_metadata(path, "codex", &id, cwd, pane)?;
        return Ok(Transcript { id });
    }
    bail!("Codex transcript has no session metadata")
}

fn parse_claude(pane: &LivePane, path: &Path, file: File) -> Result<Transcript> {
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut id = None;
    let mut cwd = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let record: ClaudeRecord = serde_json::from_str(&line).context("parsing Claude JSONL")?;
        if let Some(candidate) = record.session_id {
            if let Some(previous) = &id
                && previous != &candidate
            {
                bail!("Claude transcript has mismatched session ids");
            }
            id = Some(candidate);
        }
        if let Some(candidate) = record.cwd {
            if let Some(previous) = &cwd
                && previous != &candidate
            {
                bail!("Claude transcript has mismatched cwds");
            }
            cwd = Some(candidate);
        }
        if id.is_some() && cwd.is_some() {
            break;
        }
    }
    let id = id.context("Claude transcript has no session id")?;
    let cwd = cwd.context("Claude transcript has no cwd")?;
    validate_metadata(path, "claude", &id, &cwd, pane)?;
    Ok(Transcript { id })
}

fn validate_metadata(path: &Path, kind: &str, id: &str, cwd: &str, pane: &LivePane) -> Result<()> {
    if !is_uuid(id) {
        bail!("{kind} transcript has an invalid session id");
    }
    if pane.cwd.as_deref() != Some(cwd) {
        bail!("{kind} transcript cwd does not match the pane cwd");
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("native transcript has no valid filename")?;
    let matches_name = match kind {
        "codex" => stem == id || stem.ends_with(&format!("-{id}")),
        "claude" => stem == id,
        _ => false,
    };
    if !matches_name {
        bail!("{kind} transcript filename does not match its session id");
    }
    Ok(())
}

fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    [8, 4, 4, 4, 12].into_iter().all(|length| {
        parts.next().is_some_and(|part| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }) && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Process;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "herdr-stash-native-{}-{suffix}-{serial}",
                std::process::id()
            ));
            let root = root.join(".codex/sessions");
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn transcript(&self, filename: &str, body: &str) -> PathBuf {
            let path = self.root.join(filename);
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.root.parent().and_then(Path::parent).unwrap());
        }
    }

    fn pane(cwd: &str) -> LivePane {
        LivePane {
            pane_id: "wF:pG".into(),
            tab_id: "wF:t1".into(),
            workspace_id: "wF".into(),
            cwd: Some(cwd.into()),
            agent: Some("codex".into()),
            ..LivePane::default()
        }
    }

    fn process(pid: u32, argv: Vec<&str>) -> ProcessInfo {
        ProcessInfo {
            foreground_processes: vec![Process {
                pid,
                argv: Some(argv.into_iter().map(str::to_owned).collect()),
                ..Process::default()
            }],
            ..ProcessInfo::default()
        }
    }

    fn codex_body(id: &str, cwd: &str, originator: &str) -> String {
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"session_id\":\"{id}\",\"cwd\":\"{cwd}\",\"originator\":\"{originator}\"}}}}\n"
        )
    }

    fn fds(path: &Path) -> Vec<OpenFd> {
        vec![OpenFd {
            fd: 32,
            link: path.to_owned(),
            read: path.to_owned(),
        }]
    }

    #[test]
    fn native_argv_prefers_exact_codex_resume_id_from_agent_process() {
        let id = "019fc187-c231-7d12-b060-beca7f083597";
        let info = ProcessInfo {
            foreground_process_group_id: Some(10),
            foreground_processes: vec![
                Process {
                    pid: 10,
                    argv: Some(vec!["node".into(), "/launcher/codex".into()]),
                    ..Process::default()
                },
                Process {
                    pid: 11,
                    argv: Some(vec!["/opt/codex".into(), "resume".into(), id.into()]),
                    ..Process::default()
                },
            ],
            ..ProcessInfo::default()
        };

        let discovered = discover("codex", &pane("/workspace/site"), Some(&info))
            .unwrap()
            .unwrap();
        assert_eq!(discovered.session, id);
        assert_eq!(discovered.argv, ["resume", id]);
    }

    #[test]
    fn native_pid_fd_discovers_exact_codex_transcript() {
        let fixture = Fixture::new();
        let id = "019fc187-c231-7d12-b060-beca7f083597";
        let path = fixture.transcript(
            &format!("rollout-2026-08-02T11-12-07-{id}.jsonl"),
            &codex_body(id, "/workspace/site", "codex-tui"),
        );
        let info = process(3804337, vec!["/opt/codex"]);
        let discovered = discover_from_fds(
            "codex",
            &pane("/workspace/site"),
            &[],
            &fixture.root,
            &fds(&path),
        )
        .unwrap()
        .unwrap();

        assert_eq!(discovered.kind, "codex");
        assert_eq!(discovered.session, id);
        assert_eq!(discovered.session_kind, "id");
        assert_eq!(info.foreground_processes[0].pid, 3804337);
    }

    #[test]
    fn native_pid_fd_rejects_transcript_outside_canonical_root() {
        let fixture = Fixture::new();
        let outside = fixture.root.parent().unwrap().join("outside.jsonl");
        fs::write(
            &outside,
            codex_body(
                "019fc187-c231-7d12-b060-beca7f083597",
                "/workspace/site",
                "codex-tui",
            ),
        )
        .unwrap();
        assert!(
            discover_from_fds(
                "codex",
                &pane("/workspace/site"),
                &[],
                &fixture.root,
                &fds(&outside),
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn native_pid_fd_rejects_mismatched_kind_metadata() {
        let fixture = Fixture::new();
        let id = "019fc187-c231-7d12-b060-beca7f083597";
        let path = fixture.transcript(
            &format!("rollout-{id}.jsonl"),
            &codex_body(id, "/workspace/site", "claude"),
        );
        assert!(
            discover_from_fds(
                "codex",
                &pane("/workspace/site"),
                &[],
                &fixture.root,
                &fds(&path),
            )
            .is_err()
        );
    }

    #[test]
    fn native_pid_fd_rejects_mismatched_cwd_metadata() {
        let fixture = Fixture::new();
        let id = "019fc187-c231-7d12-b060-beca7f083597";
        let path = fixture.transcript(
            &format!("rollout-{id}.jsonl"),
            &codex_body(id, "/workspace/other", "codex-tui"),
        );
        assert!(
            discover_from_fds(
                "codex",
                &pane("/workspace/site"),
                &[],
                &fixture.root,
                &fds(&path),
            )
            .is_err()
        );
    }

    #[test]
    fn native_pid_fd_rejects_mismatched_session_metadata() {
        let fixture = Fixture::new();
        let filename_id = "019fc187-c231-7d12-b060-beca7f083597";
        let metadata_id = "019fc189-07ae-7d62-97d2-edf1d102696d";
        let path = fixture.transcript(
            &format!("rollout-{filename_id}.jsonl"),
            &codex_body(metadata_id, "/workspace/site", "codex-tui"),
        );
        assert!(
            discover_from_fds(
                "codex",
                &pane("/workspace/site"),
                &[],
                &fixture.root,
                &fds(&path),
            )
            .is_err()
        );
    }
}
