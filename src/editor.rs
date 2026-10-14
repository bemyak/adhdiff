//! Shared open-in-$EDITOR logic for the GUI and TUI.
//!
//! Spawns `$VISUAL`/`$EDITOR` (default `vi`) positioned at `line`, after
//! refusing paths that escape the bound worktree. zed/subl open the
//! worktree as project folder plus `file:line`; code/codium use `--goto`;
//! everything else gets the classic `+line file`.

use std::path::{Path, PathBuf};

/// The configured editor: $VISUAL, else $EDITOR, else vi. Empty values
/// ("VISUAL=") behave as unset — otherwise they'd spawn "".
pub fn resolve_editor() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "vi".to_string())
}

/// Spawn the editor for `file` at `line` inside `workdir`.
/// Returns `(editor, full_path)` on success; the error string is the
/// human-readable reason.
pub fn open_in_editor(file: &str, line: u32, workdir: &Path) -> Result<(String, PathBuf), String> {
    let (editor, args, full) = editor_invocation(file, line, workdir)?;
    let child = std::process::Command::new(&editor)
        .args(args)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{editor}: {e}"))?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok((editor, full))
}

/// Same invocation, but attached to the caller's terminal and waited on —
/// for terminal editors run from a TUI that first suspends raw mode and
/// the alternate screen. GUI editors that daemonize return instantly;
/// terminal editors return when the user quits.
pub fn open_in_editor_wait(
    file: &str,
    line: u32,
    workdir: &Path,
) -> Result<(String, PathBuf), String> {
    let (editor, args, full) = editor_invocation(file, line, workdir)?;
    let status = std::process::Command::new(&editor)
        .args(args)
        .current_dir(workdir)
        .status()
        .map_err(|e| format!("{editor}: {e}"))?;
    if !status.success() {
        return Err(format!("{editor} exited with {status}"));
    }
    Ok((editor, full))
}

/// Resolve path safety + per-editor argument shape shared by both spawns.
fn editor_invocation(
    file: &str,
    line: u32,
    workdir: &Path,
) -> Result<(String, Vec<std::ffi::OsString>, PathBuf), String> {
    let full = workdir.join(file);
    if !full.exists() {
        return Err(format!("{file} not in worktree"));
    }
    // safety: refuse paths that escape the worktree
    let canon_wd = workdir.canonicalize().ok();
    let canon_full = full.canonicalize().ok();
    match (canon_wd, canon_full) {
        (Some(w), Some(f)) if f.starts_with(&w) => {}
        _ => return Err("not in worktree".to_string()),
    }
    let (editor, args) = editor_argv(&resolve_editor(), &full, workdir, line);
    Ok((editor, args, full))
}

/// Split the configured editor command on whitespace (first token is the
/// program, the rest lead the arguments) and append the per-editor jump
/// arguments for `full` at `line`.
fn editor_argv(
    command: &str,
    full: &Path,
    workdir: &Path,
    line: u32,
) -> (String, Vec<std::ffi::OsString>) {
    let mut words = command.split_whitespace();
    let editor = words.next().unwrap_or("vi").to_string();
    let mut args: Vec<std::ffi::OsString> = words.map(Into::into).collect();
    let stem = std::path::Path::new(&editor)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let jump: Vec<std::ffi::OsString> = match stem.as_str() {
        // Open the worktree as project folder so the workspace resolves,
        // then the file positioned at the line.
        "zed" | "zeditor" | "subl" => vec![
            workdir.as_os_str().to_owned(),
            format!("{}:{}", full.display(), line).into(),
        ],
        "code" | "code-insiders" | "codium" => vec![
            "--goto".into(),
            format!("{}:{}", full.display(), line).into(),
        ],
        _ => vec![format!("+{line}").into(), full.as_os_str().to_owned()],
    };
    args.extend(jump);
    (editor, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &str) -> (String, Vec<String>) {
        let (p, a) = editor_argv(command, Path::new("/wd/a.rs"), Path::new("/wd"), 12);
        (
            p,
            a.into_iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn argv_builder_splits_the_command_and_appends_the_jump() {
        assert_eq!(
            argv("vi"),
            ("vi".into(), vec!["+12".into(), "/wd/a.rs".into()])
        );
        assert_eq!(
            argv("code --wait"),
            (
                "code".into(),
                vec!["--wait".into(), "--goto".into(), "/wd/a.rs:12".into()]
            )
        );
        assert_eq!(
            argv("  emacsclient   -n "),
            (
                "emacsclient".into(),
                vec!["-n".into(), "+12".into(), "/wd/a.rs".into()]
            )
        );
        assert_eq!(
            argv("zed"),
            ("zed".into(), vec!["/wd".into(), "/wd/a.rs:12".into()])
        );
        assert_eq!(argv("  ").0, "vi");
    }
}
