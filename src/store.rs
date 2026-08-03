//! Where stashes live.
//!
//! One file per stash under `$HERDR_PLUGIN_STATE_DIR/stashes`, which is the
//! directory Herdr creates for exactly this and hands every plugin process. A
//! directory rather than one index file: two stashes taken from two panes at the
//! same moment would otherwise race for the same write, and the loser would
//! vanish along with the workspace it described.
//!
//! Writes land through a temporary file and a rename, so a stash is either
//! entirely on disk before the workspace closes or not written at all. That
//! ordering is the whole safety story of this plugin: [`crate::capture`] closes
//! nothing until [`save`] has returned.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::handoff::Handoff;
use crate::record::{Stash, VERSION};

const HANDOFF_VERSION: u32 = 1;

/// The stash directory, created if it does not exist.
///
/// `HERDR_PLUGIN_STATE_DIR` when Herdr set it — it does for plugin processes —
/// and an XDG-shaped path otherwise, so the binary is still usable by hand
/// outside a pane, which is how it gets tested.
pub fn dir() -> Result<PathBuf> {
    let root = root()?;
    let dir = root.join("stashes");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn root() -> Result<PathBuf> {
    match std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        Some(path) => Ok(PathBuf::from(path)),
        None => {
            let home =
                std::env::var_os("HOME").context("neither HERDR_PLUGIN_STATE_DIR nor HOME")?;
            Ok(PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("herdr-stash"))
        }
    }
}

/// Write a stash, and return the file it landed in.
pub fn save(stash: &Stash) -> Result<PathBuf> {
    let dir = dir()?;
    let path = dir.join(format!("{}.json", stash.id));
    let staging = dir.join(format!(".{}.json.tmp", stash.id));

    let body = serde_json::to_vec_pretty(stash).context("encoding the stash")?;
    std::fs::write(&staging, &body).with_context(|| format!("writing {}", staging.display()))?;
    std::fs::rename(&staging, &path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(path)
}

/// Every readable stash, newest first.
///
/// A file this version does not understand is skipped rather than fatal: a
/// picker that refuses to open because one record is from the future is a picker
/// that has locked the operator out of every other stash they took.
pub fn list() -> Vec<Stash> {
    let Ok(dir) = dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut found: Vec<Stash> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| read(&path).ok())
        .collect();

    found.sort_by_key(|stash| std::cmp::Reverse(stash.stashed_at));
    found
}

pub fn read(path: &Path) -> Result<Stash> {
    let body = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let stash: Stash =
        serde_json::from_slice(&body).with_context(|| format!("decoding {}", path.display()))?;
    if stash.version > VERSION {
        bail!(
            "{}: written by a newer herdr-stash (format {} > {VERSION})",
            path.display(),
            stash.version
        );
    }
    Ok(stash)
}

pub fn delete(id: &str) -> Result<()> {
    let path = dir()?.join(format!("{id}.json"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

#[derive(Debug, Serialize, Deserialize)]
struct HandoffFile {
    version: u32,
    #[serde(default)]
    entries: Vec<Handoff>,
}

fn handoff_path() -> Result<PathBuf> {
    Ok(root()?.join("handoffs.json"))
}

fn read_handoffs() -> Result<HandoffFile> {
    let path = handoff_path()?;
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HandoffFile {
                version: HANDOFF_VERSION,
                entries: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let file: HandoffFile =
        serde_json::from_str(&body).with_context(|| format!("decoding {}", path.display()))?;
    if file.version > HANDOFF_VERSION {
        bail!(
            "{}: written by a newer herdr-stash handoff format {} > {HANDOFF_VERSION}",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

fn write_handoffs(file: &HandoffFile) -> Result<()> {
    let path = handoff_path()?;
    let staging = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(file).context("encoding handoffs")?;
    std::fs::create_dir_all(root()?).context("creating the handoff directory")?;
    std::fs::write(&staging, body).with_context(|| format!("writing {}", staging.display()))?;
    std::fs::rename(&staging, &path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Persist the exact session that a successful restore started in one pane.
/// Existing entries for that pane id are discarded because a reused live pane
/// must not inherit an older workspace's session.
pub fn save_handoff(handoff: Handoff) -> Result<()> {
    handoff.validate()?;
    let mut file = read_handoffs()?;
    file.version = HANDOFF_VERSION;
    file.entries
        .retain(|entry| entry.pane_id != handoff.pane_id);
    file.entries.push(handoff);
    write_handoffs(&file)
}

/// Find a handoff only when the live pane's full scope and agent kind match.
/// Stale entries for the same pane id are removed while doing so.
pub fn find_handoff(
    workspace_id: &str,
    tab_id: &str,
    pane_id: &str,
    kind: &str,
) -> Result<Option<Handoff>> {
    let mut file = read_handoffs()?;
    let mut found = None;
    let mut changed = false;
    let mut mismatch = None;
    let mut invalid = None;
    let mut retained = Vec::with_capacity(file.entries.len());

    for entry in file.entries.drain(..) {
        if entry.pane_id != pane_id {
            retained.push(entry);
            continue;
        }

        if entry.workspace_id != workspace_id || entry.tab_id != tab_id {
            changed = true;
            continue;
        }
        if entry.agent.kind != kind {
            changed = true;
            mismatch = Some(format!("handoff agent kind does not match pane {pane_id}"));
            continue;
        }
        if let Err(error) = entry.validate() {
            changed = true;
            invalid = Some(error);
            continue;
        }
        if found.is_some() {
            changed = true;
        } else {
            found = Some(entry.clone());
            retained.push(entry);
        }
    }

    file.entries = retained;
    if changed {
        write_handoffs(&file)?;
    }
    if let Some(error) = mismatch {
        bail!("{error}");
    }
    if let Some(error) = invalid {
        return Err(error);
    }
    Ok(found)
}

/// Consume all handoffs belonging to a workspace after its stash closed it.
/// The workspace boundary keeps an ID from surviving into another workspace.
pub fn clear_workspace_handoffs(workspace_id: &str) -> Result<()> {
    let mut file = read_handoffs()?;
    let before = file.entries.len();
    file.entries
        .retain(|entry| entry.workspace_id != workspace_id);
    if file.entries.len() != before {
        write_handoffs(&file)?;
    }
    Ok(())
}
