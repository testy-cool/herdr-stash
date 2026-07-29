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

use crate::record::{Stash, VERSION};

/// The stash directory, created if it does not exist.
///
/// `HERDR_PLUGIN_STATE_DIR` when Herdr set it — it does for plugin processes —
/// and an XDG-shaped path otherwise, so the binary is still usable by hand
/// outside a pane, which is how it gets tested.
pub fn dir() -> Result<PathBuf> {
    let root = match std::env::var_os("HERDR_PLUGIN_STATE_DIR") {
        Some(path) => PathBuf::from(path),
        None => {
            let home =
                std::env::var_os("HOME").context("neither HERDR_PLUGIN_STATE_DIR nor HOME")?;
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("herdr-stash")
        }
    };
    let dir = root.join("stashes");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
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
