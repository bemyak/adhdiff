//! Shared open-in-$EDITOR logic for the GUI and TUI.
//!
//! Spawns the first usable editor positioned at `line` (GUI: `$VISUAL`,
//! `$EDITOR`; TUI: `$EDITOR`, `$VISUAL`, `vi`), after refusing paths that escape the bound worktree. zed/subl open the
//! worktree as project folder plus `file:line`; code/codium use `--goto`;
//! everything else gets the classic `+line file`.

use std::path::{Path, PathBuf};

/// Which front end is asking: the GUI expects a windowed editor, the TUI a
/// terminal one.
#[derive(Clone, Copy)]
pub enum Surface {
    Gui,
    Tui,
}

/// The configured editor command. GUI: $VISUAL, else $EDITOR. TUI:
/// $EDITOR, else $VISUAL, else vi. Empty values ("VISUAL=") behave as
/// unset — otherwise they'd spawn "". None when there is no candidate or
/// the chosen command is unusable (see [`usable_editor`]).
pub fn resolve_editor(surface: Surface) -> Option<String> {
    usable_editor(
        surface,
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// Agent harnesses pin VISUAL/EDITOR to `true`-style no-ops so spawned
/// git never blocks; a GUI launched under one would "edit" into the void.
/// Those pins, and programs missing from `path`, are unusable.
pub(crate) fn usable_editor(
    surface: Surface,
    visual: Option<&str>,
    editor: Option<&str>,
    path: Option<&std::ffi::OsStr>,
) -> Option<String> {
    let (order, default) = match surface {
        Surface::Gui => ([visual, editor], None),
        Surface::Tui => ([editor, visual], Some("vi")),
    };
    order
        .into_iter()
        .flatten()
        .chain(default)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .find(|v| is_usable(v, path))
        .map(str::to_string)
}

fn is_usable(command: &str, path: Option<&std::ffi::OsStr>) -> bool {
    let Some(program) = command.split_whitespace().next() else {
        return false;
    };
    let noop = Path::new(program)
        .file_name()
        .is_some_and(|n| matches!(n.to_string_lossy().as_ref(), "true" | "false" | ":"));
    !noop && find_program(program, path)
}

/// `which`-style lookup: a program containing a path separator is checked
/// as is, anything else is searched over `path`.
fn find_program(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    path.is_some_and(|p| std::env::split_paths(p).any(|dir| is_executable(&dir.join(program))))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The path of `file` inside `workdir` when it exists and, once symlinks
/// are resolved, stays under the worktree.
fn contained_path(file: &str, workdir: &Path) -> Option<PathBuf> {
    let full = workdir.join(file);
    let canon_wd = workdir.canonicalize().ok()?;
    let canon_full = full.canonicalize().ok()?;
    canon_full.starts_with(&canon_wd).then_some(full)
}

/// Whether `file` is a regular file under the bound worktree.
pub fn openable(file: &str, workdir: &Path) -> bool {
    contained_path(file, workdir).is_some_and(|f| f.is_file())
}

/// Spawn `command` (the resolved editor) for `file` at `line` inside `workdir`.
/// Returns `(editor, full_path)` on success; the error string is the
/// human-readable reason.
pub fn open_in_editor(
    command: &str,
    file: &str,
    line: u32,
    workdir: &Path,
) -> Result<(String, PathBuf), String> {
    let (editor, args, full) = editor_invocation(command, file, line, workdir)?;
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
    command: &str,
    file: &str,
    line: u32,
    workdir: &Path,
) -> Result<(String, PathBuf), String> {
    let (editor, args, full) = editor_invocation(command, file, line, workdir)?;
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
    command: &str,
    file: &str,
    line: u32,
    workdir: &Path,
) -> Result<(String, Vec<std::ffi::OsString>, PathBuf), String> {
    let full = contained_path(file, workdir).ok_or_else(|| format!("{file} not in worktree"))?;
    let (editor, args) = editor_argv(command, &full, workdir, line);
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

    fn bin_dir(tag: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("adhdiff-editor-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            let bin = dir.join(name);
            std::fs::write(&bin, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        dir
    }

    #[test]
    fn noop_pins_and_absent_programs_are_unusable() {
        let dir = bin_dir("usable", &["zeditor", "true"]);
        let path = Some(dir.as_os_str());
        for surface in [Surface::Gui, Surface::Tui] {
            let u = |v, e, p| usable_editor(surface, v, e, p);
            assert_eq!(u(Some("true"), Some("true"), path), None);
            assert_eq!(u(None, Some("/bin/false"), path), None);
            assert_eq!(u(Some(":"), None, path), None);
            assert_eq!(u(Some("true --x"), None, path), None);
            assert_eq!(u(Some("nosuchprog -w"), None, path), None);
            assert_eq!(u(Some("zeditor"), None, None), None);
            assert_eq!(
                u(Some(""), Some("zeditor -w"), path),
                Some("zeditor -w".to_string())
            );
            let abs = dir.join("zeditor");
            let abs = abs.to_string_lossy();
            assert_eq!(u(Some(&abs), None, None), Some(abs.to_string()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gui_prefers_visual_and_tui_prefers_editor() {
        let dir = bin_dir("order", &["zeditor", "hx", "vi"]);
        let path = Some(dir.as_os_str());
        let gui = |v, e| usable_editor(Surface::Gui, v, e, path);
        let tui = |v, e| usable_editor(Surface::Tui, v, e, path);
        assert_eq!(gui(Some("zeditor"), Some("hx")), Some("zeditor".into()));
        assert_eq!(tui(Some("zeditor"), Some("hx")), Some("hx".into()));
        assert_eq!(gui(None, Some("hx")), Some("hx".into()));
        assert_eq!(tui(Some("zeditor"), None), Some("zeditor".into()));
        assert_eq!(gui(Some("true"), Some("zeditor")), Some("zeditor".into()));
        assert_eq!(tui(Some("zeditor"), Some("true")), Some("zeditor".into()));
        assert_eq!(tui(Some("hx"), Some("true")), Some("hx".into()));
        assert_eq!(tui(Some("true"), None), Some("vi".into()));
        assert_eq!(gui(Some("true"), None), None);
        assert_eq!(gui(None, None), None);
        assert_eq!(tui(None, None), Some("vi".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn openable_requires_a_regular_file_inside_the_worktree() {
        let dir = bin_dir("openable", &["a.rs"]);
        let wd = dir.join("wd");
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        std::fs::write(wd.join("sub/x.rs"), "").unwrap();
        assert!(openable("sub/x.rs", &wd));
        assert!(!openable("sub/missing.rs", &wd));
        assert!(!openable("sub", &wd));
        assert!(!openable("../a.rs", &wd));
        assert!(!openable("/dev/null", &wd));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
