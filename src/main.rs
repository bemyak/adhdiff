mod app;
mod check;
mod context;
mod diff;
mod editor;
mod enrich;
mod highlight;
mod hunk_html;
mod outline;
mod prompt;
mod render;
mod state;
mod tui;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand};
use std::io::Read;

mod inbox;

#[derive(Parser)]
#[command(
    name = "adhdiff",
    version,
    about = "Priority-annotated diff maps for AI-assisted review"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse a unified diff and print the raw hunk inventory as JSON.
    ///
    /// This is the agent's input: hunk list with file/line ranges, diff rows
    /// (`row_start`/`row_end`) and bodies.
    Parse {
        /// Read diff from this file instead of stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Render an AdhDiffMap JSON against its diff into a terminal-friendly view.
    ///
    /// Pipe the agent-produced map JSON to stdin; the diff comes from --diff
    /// or --diff-file. Refuses a map whose diff_sha256 does not match the diff.
    #[command(group(ArgGroup::new("diff_source").required(true).args(["diff", "diff_file"])))]
    Render {
        /// Map JSON file (or stdin if omitted).
        #[arg(long)]
        map: Option<String>,
        /// Raw diff text.
        #[arg(long)]
        diff: Option<String>,
        /// Raw diff file.
        #[arg(long)]
        diff_file: Option<String>,
    },
    /// Produce the AGENT_PROMPT to feed to a reviewer agent, embedding the
    /// parsed hunk inventory plus instructions for emitting an AdhDiffMap JSON.
    Prompt {
        /// Read diff from this file instead of stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Validate an AdhDiffMap JSON against its source diff: diff_sha256,
    /// row coverage, duplicates, supports edges, warnings. Exits non-zero
    /// on errors.
    #[command(group(ArgGroup::new("diff_source").required(true).args(["diff", "diff_file"])))]
    Check {
        /// Map JSON file (or stdin if omitted).
        #[arg(long)]
        map: Option<String>,
        /// Raw diff text.
        #[arg(long)]
        diff: Option<String>,
        /// Raw diff file.
        #[arg(long)]
        diff_file: Option<String>,
        /// Treat low-confidence labels on P5+ entries as errors.
        #[arg(long)]
        strict: bool,
    },
    /// Enrich a unified diff with per-hunk language, enclosing symbol, and
    /// git hunk-header context (tree-sitter over the new-side files).
    Enrich {
        #[arg(long)]
        input: Option<String>,
        /// Working tree root to read new-side file contents from
        /// (defaults to current dir).
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Interactive terminal viewer: priority outline + detail panes + comments.
    /// Comments/viewed share the sidecar state with the desktop UI. Refuses a
    /// map whose diff_sha256 does not match the diff.
    Tui {
        /// Map JSON file (or stdin if omitted).
        #[arg(long)]
        map: Option<String>,
        /// Raw diff file.
        #[arg(long)]
        diff_file: String,
        /// Sidecar state (comment/viewed JSON): an existing directory or a
        /// path ending in `/` holds `<sha12>.json`, anything else is the state
        /// file. Default: $XDG_STATE_HOME/adhdiff/<worktree-hash>/<sha12>.json.
        #[arg(long)]
        out: Option<String>,
        /// Worktree lines shown dimmed above and below every hunk (0 = off).
        #[arg(long, default_value_t = 5)]
        context: u32,
    },
    /// Desktop window (Tauri + Leptos): the full-page review surface,
    /// rendered in a local webview shell. Refuses a map whose diff_sha256
    /// does not match the diff.
    Gui {
        /// Map JSON file (or stdin if omitted).
        #[arg(long)]
        map: Option<String>,
        /// Raw diff file.
        #[arg(long)]
        diff_file: String,
        /// Sidecar state (comment/viewed JSON): an existing directory or a
        /// path ending in `/` holds `<sha12>.json`, anything else is the state
        /// file. Default: $XDG_STATE_HOME/adhdiff/<worktree-hash>/<sha12>.json.
        #[arg(long)]
        out: Option<String>,
        /// Worktree lines shown dimmed above and below every hunk (0 = off).
        #[arg(long, default_value_t = 5)]
        context: u32,
    },
    /// Inbox: pending reviewer commands/asks. An agent polls this between
    /// turns to react to human feedback without keeping a socket open.
    Inbox {
        /// Print pending actions as JSON. Non-destructive.
        #[arg(long)]
        peek: bool,
        /// Mark an action resolved by id.
        #[arg(long)]
        done: Option<String>,
        /// Post a new action.
        #[arg(long)]
        post: Option<String>,
        /// On hunk: "file:row" else general.
        #[arg(long)]
        hunk: Option<String>,
    },
}

fn read_input(path: Option<&str>) -> Result<Vec<u8>> {
    match path {
        Some(p) => std::fs::read(p).with_context(|| format!("reading {p}")),
        None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn read_map(path: Option<&str>) -> Result<adhdiff_shared::AdhDiffMap> {
    let bytes = read_input(path)?;
    serde_json::from_slice(&bytes).context("parsing AdhDiffMap JSON")
}

fn read_diff(diff: Option<String>, diff_file: Option<String>) -> Result<Vec<u8>> {
    match (diff, diff_file) {
        (Some(text), _) => Ok(text.into_bytes()),
        (None, Some(f)) => std::fs::read(&f).with_context(|| format!("reading {f}")),
        (None, None) => bail!("--diff or --diff-file required"),
    }
}

/// Read a map and its diff, refusing a pair whose digest does not match.
fn read_pair(
    map: Option<&str>,
    diff: Option<String>,
    diff_file: Option<String>,
) -> Result<(adhdiff_shared::AdhDiffMap, Vec<u8>)> {
    let map = read_map(map)?;
    let diff = read_diff(diff, diff_file)?;
    if let Some(why) = check::sha_mismatch(&map, &diff) {
        bail!(why);
    }
    Ok((map, diff))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Parse { input } => {
            let bytes = read_input(input.as_deref())?;
            let files = diff::parse_unified(&bytes)?;
            println!("{}", serde_json::to_string_pretty(&files)?);
        }
        Cmd::Render {
            map,
            diff,
            diff_file,
        } => {
            let (map, diff) = read_pair(map.as_deref(), diff, diff_file)?;
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            render::render_map(&mut w, &map, &diff)?;
        }
        Cmd::Prompt { input } => {
            let bytes = read_input(input.as_deref())?;
            let files = diff::parse_unified(&bytes)?;
            println!("{}", prompt::render_prompt_for(&files));
        }
        Cmd::Check {
            map,
            diff,
            diff_file,
            strict,
        } => {
            let map = read_map(map.as_deref())?;
            let diff = read_diff(diff, diff_file)?;
            let report = check::check_map(&map, &diff, strict);
            report.print();
            if !report.ok() {
                std::process::exit(1);
            }
        }
        Cmd::Enrich { input, workdir } => {
            let bytes = read_input(input.as_deref())?;
            let files = diff::parse_unified(&bytes)?;
            let wd = workdir
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let out = enrich::enrich(&files, &wd);
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Cmd::Tui {
            map,
            diff_file,
            out,
            context,
        } => {
            let (map, diff) = read_pair(map.as_deref(), None, Some(diff_file))?;
            tui::run_tui(map, &diff, out, context)?;
        }
        Cmd::Gui {
            map,
            diff_file,
            out,
            context,
        } => {
            let (map, diff) = read_pair(map.as_deref(), None, Some(diff_file))?;
            app::run_app(map, &diff, out, context)?;
        }
        Cmd::Inbox {
            peek,
            done,
            post,
            hunk,
        } => {
            let store = std::path::PathBuf::from(".adhdiff");
            let ipath = inbox::inbox_path(&store, "current");
            if peek {
                let items = inbox::load(&ipath)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &items.iter().filter(|a| !a.done).collect::<Vec<_>>()
                    )?
                );
            } else if let Some(id) = done {
                if !inbox::mark_done(&ipath, &id)? {
                    bail!("no action with id {id}");
                }
            } else if let Some(body) = post {
                let a = inbox::post(&ipath, "ask", &body, hunk)?;
                println!(
                    "posted action {} — agents see it via `adhdiff inbox --peek`",
                    a.id
                );
            } else {
                bail!("inbox needs --peek, --done <id>, or --post <text>");
            }
        }
    }
    Ok(())
}
