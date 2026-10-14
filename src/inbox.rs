//! Reviewer inbox: a file-backed queue where asks live until an agent
//! processes them. Deliberately not a socket/queue/daemon — the
//! anti-glue principle: the file is the interface.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{short_id, write_atomic};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Action {
    pub id: String,
    /// "comment", "ask", "relabel", "drop-hunk", "focus", "summarize"
    pub kind: String,
    /// Optional "file:row" the action is anchored on.
    pub hunk: Option<String>,
    /// The body of the ask.
    pub text: String,
    pub ts_unix: u64,
    /// True once an agent has handled this and reported.
    pub done: bool,
}

pub fn inbox_path(store_dir: &Path, scope: &str) -> PathBuf {
    store_dir.join(format!("inbox-{scope}.json"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn load(p: &Path) -> Result<Vec<Action>> {
    if !p.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(p).context("reading inbox")?;
    serde_json::from_str(&text).with_context(|| format!("parsing inbox {}", p.display()))
}

pub fn save(p: &Path, items: &[Action]) -> Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(p, serde_json::to_string_pretty(items)?.as_bytes())?;
    Ok(())
}

pub fn post(p: &Path, kind: &str, text: &str, hunk: Option<String>) -> Result<Action> {
    let mut items = load(p)?;
    let a = Action {
        id: short_id(),
        kind: kind.into(),
        hunk,
        text: text.into(),
        ts_unix: now_unix(),
        done: false,
    };
    items.push(a.clone());
    save(p, &items)?;
    Ok(a)
}

pub fn mark_done(p: &Path, id: &str) -> Result<bool> {
    let mut items = load(p)?;
    let mut found = false;
    for a in items.iter_mut().filter(|a| a.id == id) {
        a.done = true;
        found = true;
    }
    if !found {
        return Ok(false);
    }
    save(p, &items)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("adhdiff-inbox-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mark_done_reports_whether_the_id_exists() {
        let dir = scratch("done");
        let p = inbox_path(&dir, "t");
        let a = post(&p, "ask", "why?", None).unwrap();
        assert!(!mark_done(&p, "nope").unwrap());
        assert!(!load(&p).unwrap()[0].done);
        assert!(mark_done(&p, &a.id).unwrap());
        assert!(load(&p).unwrap()[0].done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_inbox_is_an_error_and_is_not_overwritten() {
        let dir = scratch("corrupt");
        let p = inbox_path(&dir, "t");
        std::fs::write(&p, "{not json").unwrap();
        let err = format!("{:#}", load(&p).unwrap_err());
        assert!(err.contains("inbox-t.json"), "{err}");
        assert!(post(&p, "ask", "x", None).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
