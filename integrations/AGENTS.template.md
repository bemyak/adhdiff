# AGENTS.md TEMPLATE — copy me to AGENTS.md in your repo

Review it, then drop it in as `<your-repo>/AGENTS.md`. omp, codex and
gemini-cli auto-discover `AGENTS.md`; Claude Code reads `CLAUDE.md`, not
`AGENTS.md`, so add a `CLAUDE.md` that includes it (`@AGENTS.md`) or points
to it. Keep other agent-specific files as one-line pointers back here.

---

## When the user asks you to review a change locally

All intermediate files go in a fresh scratch directory, never in the repo:
`WORK=$(mktemp -d "${TMPDIR:-/tmp}/adhdiff.XXXXXX")`.

1. Produce the diff yourself (adhdiff has no VCS knowledge; detect:
   `.jj/` exists or `jj root` succeeds → jj, else git), canonical
   commands verbatim:
   jj:  `jj diff --git -r "<BASE>..@" > "$WORK/change.diff"`
   git: `git diff -M -U3 --no-ext-diff --no-color --src-prefix=a/ --dst-prefix=b/ "<BASE>...HEAD" > "$WORK/change.diff"`
   BASE = the head of the previous review from this session, else the main
   line (`trunk()` / `main` / `master` / remote HEAD); when ambiguous, ask
   the human. Full strict recipe: the adhdiff skill.
2. Build the prompt: `adhdiff prompt --input "$WORK/change.diff" > "$WORK/AGENT_PROMPT.md"`
3. Read `AGENT_PROMPT.md`, emit an AdhDiffMap JSON, save it as
   `$WORK/map.json`. Coordinates are diff rows, as printed in the prompt's
   `<row> | ` gutter. Set `diff_sha256` to
   `sha256sum "$WORK/change.diff" | cut -d' ' -f1` (macOS: `shasum -a 256`).
4. Validate: `adhdiff check --map "$WORK/map.json" --diff-file "$WORK/change.diff"`
   If check fails, fix the map (never the diff) and re-check.
5. Hand back: `adhdiff gui --map "$WORK/map.json" --diff-file "$WORK/change.diff"`
   opens the desktop UI; `adhdiff tui …` is the interactive TUI. Plain
   terminal: `adhdiff render --map "$WORK/map.json" --diff-file "$WORK/change.diff" | $PAGER`

## When the user asks you to draft annotations for a remote MR/PR

Never touch the user's checkout — fetch the forge head, use a temp worktree:
```
git fetch origin merge-requests/<id>/head:refs/remotes/origin/mr-<id>   # GitLab (GitHub: pull/<id>/head)
HEAD_SHA=$(git rev-parse origin/mr-<id>)
BASE=$(git merge-base origin/<target-branch> "$HEAD_SHA")
git worktree add --detach "$WORK/wt" "$HEAD_SHA"   # jj: jj workspace add "$WORK/wt" --revision "$HEAD_SHA"
```
Then the local workflow above with cwd = `$WORK/wt` for every adhdiff
command (subshell `(cd "$WORK/wt" && …)`); remove the worktree when done
(`git worktree remove --force`; jj: `jj workspace forget <name>` then delete
the directory). Never publish to the forge unless the user says so —
publishing goes through your own forge tooling (MCP, glab, REST), never
adhdiff, following the adhdiff skill's protocol (present the list
and ask first, verify the remote head sha equals the head you reviewed,
anchor to the same lines).

## Output contract

`map.json` must:
- Cover every raw hunk exactly: entries tile each hunk by rows, no gaps,
  no overlaps, none spanning two hunks (check enforced).
- Use valid tiers P0–P7.
- Carry a `rationale` on every P0. Recommended on P1, empty below.
- Set `confidence: "low"` when you're unsure — these get promoted.
- Set `diff_sha256` to the hash of the exact diff you labeled.
- Be valid JSON per the schema embedded in `AGENT_PROMPT.md`.

Failures are almost always a hash mismatch, a hunk not tiled by rows, or
rows outside a hunk; `adhdiff check` names them explicitly. Read the error,
fix the JSON, re-run check. Do not silently substitute fabricated
coordinates.

## Style

- Summary: 2–3 sentences.
- Labels: one line, ≤ 80 chars, no trailing period, no filler ("changed
  file", "updated functions"). Say what it IS, not that it changed.
- Rationales: 1–2 sentences. Reference concrete identifiers
  (`retry_loop`, `Parser::parse`), not abstractions.
- P0s are usually one entry per file, almost never >3 in a whole MR.
- If a hunk spans >150 rows of real code, split it into segments by rows and
  tier each by its own intent.
