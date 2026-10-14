---
name: adhdiff
description: Use when the user asks you to review or annotate a code change (MR, PR, local diff). You produce the diff via the canonical VCS commands below and drive the adhdiff CLI to validate, enrich, and render a priority-tiered AdhDiffMap.
---

# adhdiff review workflow

You produce the diff; `adhdiff` parses, validates, enriches, renders, and
hosts the review. adhdiff has **no VCS knowledge** — diff production is
your job, and the recipe in this skill is a strict contract. Run the
commands verbatim; never improvise flags or tooling.

All intermediate files (`change.diff`, `map.json`, `AGENT_PROMPT.md`, the
inventory) live in a fresh scratch directory, never in the repo:

```bash
WORK=$(mktemp -d "${TMPDIR:-/tmp}/adhdiff.XXXXXX") && echo "$WORK"
```

Each shell call starts fresh: substitute the printed path (or re-export
`WORK`) in every later command.

## 1. Determine BASE

Resolve BASE in this order and stop at the first step that answers
unambiguously:

a. **Session memory.** If you remember the head revision of the previous
   review from this session, use it as BASE.
b. **Main line.** jj → `trunk()`; git → `main` if it exists, else
   `master`, else the remote default
   (`git symbolic-ref refs/remotes/origin/HEAD`).
c. **Ambiguity → ASK the human.** Ask when the remembered head no longer
   resolves (rebased away, garbage-collected), or when both `main` and
   `master` exist and have diverged. Never guess silently.

## 2. Detect the VCS

`.jj/` exists or `jj root` succeeds → **jj**, never git. In colocated
repos git cannot see jj working-copy state, so `git diff` silently
produces a different, incomplete diff. Otherwise → git.

## 3. Produce the diff — canonical commands, verbatim

```bash
# jj
jj diff --git -r "<BASE>..@" > "$WORK/change.diff"
# git
git diff -M -U3 --no-ext-diff --no-color --src-prefix=a/ --dst-prefix=b/ "<BASE>...HEAD" > "$WORK/change.diff"
```

These exact flags are contractual: the git-format output and rename
detection (`--git`, `-M`) plus 3 lines of context (`-U3`) are what
`adhdiff parse` and `adhdiff check` expect — a standard git-format
unified diff. Any other invocation (jj without `--git`, a custom `-U<n>`,
piped-in third-party diff) is a violation, not a stylistic choice.

`@` / `HEAD` is the reviewed state. Under jj, uncommitted working-copy
changes ride along in `@`. Under git the review covers exactly `HEAD`:
uncommitted work is invisible — have the author commit it first.

Record the reviewed head as an immutable revision in `HEAD_SHA` (session
memory; it is BASE for the next review and the race guard in "Publishing
review comments"):

```bash
HEAD_SHA=$(jj log -r @ -T commit_id --no-graph)    # jj
HEAD_SHA=$(git rev-parse HEAD)                     # git
```

## 4. Author and validate the map

Run these with cwd = the checkout (§5):

```bash
adhdiff parse --input "$WORK/change.diff" > "$WORK/hunks.json"        # raw hunk inventory (ground truth for rows)
adhdiff enrich --input "$WORK/change.diff" --workdir . > "$WORK/enriched.json"  # + per-hunk language / enclosing symbol / header context
adhdiff prompt --input "$WORK/change.diff" > "$WORK/AGENT_PROMPT.md"  # full labeling prompt: rubric + schema + diff with row gutter
# ... you author "$WORK/map.json" — the human never does ...
adhdiff check --map "$WORK/map.json" --diff-file "$WORK/change.diff"           # validation; exits non-zero on errors
adhdiff check --map "$WORK/map.json" --diff-file "$WORK/change.diff" --strict  # additionally errors on low-confidence P5–P7 labels
```

Label with the whole checkout in front of you, never from the diff text
alone (for the remote-MR recipe, that checkout is the temp worktree): read
the changed files, their neighbors, callers and callees, and the project's
`AGENTS.md` before choosing tiers.

### Coordinates

A **row** is a 1-based line number in the raw diff text, exactly what an
editor or `sed -n` shows. `adhdiff prompt` prints every diff line with a
`<row> | ` gutter; copy rows from there (or from `adhdiff parse`), never
compute them from file line numbers. A raw hunk's range is `row_start..=row_end`:
the first row after its `@@` line through the last row before the next
`@@`, `diff --git`, or EOF (`\ No newline at end of file` rows included).

### Map schema

```jsonc
{
  "title": "one line",
  "summary": "2–3 sentences",
  "diff_sha256": "hex sha256 of the raw diff bytes",
  "hunks": [
    {
      "file": "repo/relative/path",          // readability only, identity is the row
      "row_start": 12, "row_end": 40,          // inclusive, within one raw hunk
      "priority": "P0" | ... | "P7",
      "label": "one line",
      "rationale": "1–2 sentences (P0 required, P1 recommended, else empty)",
      "mechanical": false,                     // default false
      "confidence": "high" | "medium" | "low", // default high
      "supports": 12                           // optional: row_start of the entry this supports
    }
  ]
}
```

`diff_sha256` is the hash of `change.diff`:

```bash
sha256sum "$WORK/change.diff" | cut -d' ' -f1        # Linux
shasum -a 256 "$WORK/change.diff" | cut -d' ' -f1    # macOS
```

There are no provenance fields. Entry identity is `row_start`, unique across
the map; `supports` is the `row_start` of another entry.

### Rubric

- Tiers: P0 gist, P1 supporting, P2 error handling, P3 tests of intent,
  P4 docs, P5 mechanical, P6 structural move, P7 chore.
- `summary`: 2–3 sentences. `label`: one line, ≤ 80 characters, no
  trailing period. `rationale`: 1–2 sentences, mandatory on P0 (name
  concrete identifiers), recommended on P1, empty below.
- Every raw hunk is tiled by rows: 1..N entries whose
  `[row_start, row_end]` spans cover it exactly — first entry starts at the
  hunk's first row, last ends at its last row, no gaps, no overlaps. One
  whole-hunk entry is the valid degenerate tiling.
- Decompose by range, not per line: wide spans for mechanical runs, narrow
  spans for load-bearing logic. A span over 150 rows needs
  `mechanical: true`.
- Roots (no `supports`) are the independent change axes of the change;
  every other entry points at the one entry it supports. No cycles, no
  dangling targets.

`check` errors on: a `diff_sha256` mismatch, a non-empty diff that parses to
zero files, `row_start` 0 or `row_end` < `row_start`, an entry spanning more
than one raw hunk, a hunk not tiled exactly, a duplicate `row_start`, a
dangling or cyclic `supports`, a P0 without rationale. It warns on spans
> 150 rows without `mechanical`, tiny segments inside large hunks, more
than a quarter of the entries being P0, and low-confidence P5–P7 entries
(an error under `--strict`). When check fails, fix `map.json` and
re-check; `change.diff` is content-addressed input — never edit it.

- `parse`, `enrich`, `prompt`, and `check --map` all read stdin when their
  file flag is omitted; `check`/`render` also accept `--diff` for inline diff
  text.

## 5. cwd / workspace discipline

- Run **every** adhdiff invocation with cwd = the target checkout. Use a
  subshell — `(cd <checkout> && adhdiff gui …)` — never `cd` your own
  shell out of the user's repo for good.
- Pass `enrich --workdir <checkout>` explicitly; it reads new-side file
  contents from that tree.
- Launch `gui` and `tui` the same way. Open-in-editor jumps and the
  review-state sidecar key off the git/jj worktree that contains cwd;
  the sidecar lives at
  `$XDG_STATE_HOME/adhdiff/<worktree-hash>/<diff-sha12>.json` (override
  with `--out`). Launching from the wrong cwd binds comments and
  viewed state to the wrong worktree hash.
- After the review closes, dispose of temp worktrees (see the remote
  recipe).

## Human-facing surfaces

Your job ends at a valid `map.json`; the human picks the surface:

- `adhdiff gui --map map.json --diff-file change.diff [--out PATH]` —
  desktop UI (Tauri + Leptos webview): the full-page review surface.
- `adhdiff tui --map map.json --diff-file change.diff [--out PATH]` —
  interactive TUI: priority outline, detail panes, comments.
- `adhdiff render --map map.json --diff-file change.diff` — one-shot
  terminal render, good for piping to `$PAGER`.

`gui`, `tui` and `render` refuse to start when the map's `diff_sha256` does
not match the diff. `gui` and `tui` require `--diff-file` (map may come via
stdin). `--out PATH`: if PATH is an existing directory or ends with `/`, the
state file is `PATH/<sha12>.json`; otherwise PATH is the state file itself.
Open-in-editor jumps resolve `$VISUAL`, then `$EDITOR` in the GUI and `$EDITOR`, then `$VISUAL`, else `vi` in the TUI; the GUI button is hidden when no usable editor is configured or the file is not in the worktree.

Agent harnesses typically pin `VISUAL`, `EDITOR` and `GIT_EDITOR` to `true`
in subprocess environments, which adhdiff treats as "no editor". When you
launch `gui`, pass the user's real editor explicitly:

```bash
VISUAL=<editor> adhdiff gui --map "$WORK/map.json" --diff-file "$WORK/change.diff"
```

Find `<editor>` from the user's login environment (for example the value a
login shell reports with `sh -lc 'echo $VISUAL $EDITOR'`), or ask; never
guess a program that may not be installed.
Comments and viewed state persist to the sidecar described in §5.

## Local feedback loop: UI comments are feedback for you

When the user reviews a change YOU just made, every comment they leave
in the UI is actionable feedback on your work — your turn is not over
until the sidecar is quiet. (When they review someone else's MR/PR, the
comments are forge drafts instead — see "Publishing review comments to
the forge" below.)

1. Launch the surface with a session-controlled state dir, from the
   target checkout (the §5 cwd rule stands):

   ```bash
   (cd <checkout> && VISUAL=<editor> adhdiff gui --map "$WORK/map.json" --diff-file "$WORK/change.diff" \
     --out "$WORK/review/")
   ```

2. Poll `$WORK/review/*.json` between turns — no socket, no watch; the
   file is the interface. Layout: `$WORK/review/<diff-sha12>.json`, one
   JSON object keyed by `comment_key` (`"file:row"`, or
   `"file:row-end_row"` for a range); each value is a thread of comment
   objects `{ id, file, row, end_row?, label_at_time, author, body,
   ts_unix }` (`author` is `"human"`, `"agent"`, or a tool name).
   Reserved keys start with `@`: `"@version"` (number), `"@diff_sha"`
   (string) and `"@viewed"` (viewed state) are not comments. E.g.:

   ```bash
   jq -r 'to_entries[] | select(.key | startswith("@") | not) | .value[]
          | select(.author == "human") | [.id, .file, .row, .body] | @tsv' \
     "$WORK"/review/*.json
   ```

   Do not repost these into the inbox queue — the inbox is for
   agent-to-agent asks; the sidecar is the comment interface.

3. "Unresolved" is yours to track: sidecar comments carry no done bit
   (that exists only on inbox actions), so remember the comment `id`s
   you have acted on and treat every other human-authored comment as
   pending. Never write replies into the sidecar — the GUI persists it
   whole-file from in-memory state and clobbers foreign writes. Your
   reply is the regenerated map.

4. Act per comment: fix the code → regenerate `change.diff` with §3's
   canonical commands → re-author and re-validate the map per §4
   (prompt → author → `adhdiff check`) → relaunch the gui on the new map
   (same `--out`; the new diff keys a new sha12 sidecar).

5. Diff drift invalidates coordinates. Comments stay pinned to the
   `(file, row)` of the diff that existed when the user wrote them and
   DON'T follow the code as it moves. After your edits move lines,
   re-locate each comment against the new `adhdiff parse --input
   change.diff` output before acting: recover the target from the
   comment's `body` + `label_at_time`, and say which hunk you matched it
   to.

### Finishing: wait for close, then read REVIEW_DONE

- At launch the gui prints `adhdiff: review state: <abs sidecar path>`
  to stderr — that path is the sidecar you poll.
- Prefer launching attached and retaining the pid: on clean
  last-window close the gui writes `REVIEW_DONE` (any marker left by
  a previous run is removed at launch) next to the sidecar
  (`{"diff_sha12", "closed_at_unix", "comments", "viewed"}`), prints
  `adhdiff: review closed: <sidecar path> (comments=N, viewed=M)`,
  and the process EXITS. Waiting on the pid is the whole protocol.
- Detached launch: poll for `REVIEW_DONE` in the sidecar's directory
  instead — no socket, no watch, same as the sidecar loop.
- No marker = crash or window still open; never read its absence as
  "no feedback". When it appears, report the counts; the comments
  themselves come from the sidecar loop above.

## Recipe: review a remote MR/PR without touching the user's checkout

If the target branch is already checked out locally, §1–§4 apply
as-is. Otherwise fetch the forge head into the repo and create a TEMP
worktree — never modify the user's checkout. In a colocated jj repo the
fetch is still done with git:

```bash
# fetch the forge head (GitLab / GitHub)
git fetch origin merge-requests/<id>/head:refs/remotes/origin/mr-<id>
git fetch origin pull/<id>/head:refs/remotes/origin/mr-<id>
HEAD_SHA=$(git rev-parse origin/mr-<id>)
BASE=$(git merge-base origin/<target-branch> "$HEAD_SHA")

# git-only: detached worktree + diff
git worktree add --detach "$WORK/wt" "$HEAD_SHA"
git -C "$WORK/wt" diff -M -U3 --no-ext-diff --no-color --src-prefix=a/ --dst-prefix=b/ "$BASE...$HEAD_SHA" > "$WORK/change.diff"

# jj: workspace + diff (jj imports the fetched ref on its next command)
jj workspace add "$WORK/wt" --revision "$HEAD_SHA"
jj diff --git --from "$BASE" --to "$HEAD_SHA" > "$WORK/change.diff"
```

`<target-branch>` is the base the MR advertises on the forge (e.g.
`main`). Run every adhdiff invocation per §5 with cwd = `$WORK/wt`. When
the review closes, dispose of the worktree: `git worktree remove --force
"$WORK/wt"` for git; for jj `jj workspace forget <workspace-name>` (the
name defaults to the directory basename) and then `rm -rf "$WORK/wt"`.

## Publishing review comments to the forge (remote MR/PR)

For a remote review the user's sidecar comments are DRAFTS for the
forge. adhdiff never publishes; you do, through `gh` / `glab` / MCP,
gated like this:

**Gate 1 — ASK before touching the forge.** Comments stay local until
the user explicitly says to publish ("post these", "put them on the
MR"…). Even then, present the pending list (file:line + body, one per
line) and ask for confirmation before the first API call. The comments
land in someone else's mailbox — your own sense of "finished" is never
permission.

**Gate 2 — head-sha race guard (REQUIRED).** The forge head may have
moved since the diff was produced. Before posting anything, fetch the
live head sha and require it to equal the head you recorded in §3
(`HEAD_SHA`, recorded in §3 or by the remote recipe):

```bash
# GitHub
gh api repos/{owner}/{repo}/pulls/<n> --jq .head.sha
# GitLab (also captures diff_refs.base_sha / start_sha / head_sha for below)
glab api "projects/:id/merge_requests/<iid>" --output json \
  | jq -r '{head: .sha, refs: .diff_refs}'
```

On mismatch, STOP: your coordinates belong to a stale head. Re-review
the fresh head first — BASE per §1, temp worktree per the remote
recipe, new map — and publish against the new head. (`:id` expands from
the repo remote exactly like gh's `{owner}/{repo}`; glab has no `:iid`
placeholder — substitute the MR number literally.)

**Gate 3 — anchor each comment to the SAME line the user highlighted.**
A sidecar comment carries a diff `file` and `row`; the forge wants a file
line. Convert the row with the reviewed diff: a `+` or context row maps to
its new-side line (the hunk's new start plus the count of `+`/context rows
before it); a `-` row has no new-side line and anchors to the old side
instead. Plain `file:line + body` in the pending list is what the user
confirms.

GitHub — one call per comment (`<line>` is the new-side file line;
`side=RIGHT` is the GitHub API's own parameter, use `LEFT` for a removed
line):

```bash
gh api repos/{owner}/{repo}/pulls/<n>/comments \
  -F body="<body>" -F path="<file>" -F line=<line> \
  -F side=RIGHT -F commit_id="<head_sha>"
```

or batched as one review:

```bash
gh api repos/{owner}/{repo}/pulls/<n>/reviews \
  -F event=COMMENT -F commit_id="<head_sha>" \
  -F "comments[][path]=<file>" -F "comments[][line]=<line>" \
  -F "comments[][side]=RIGHT" -F "comments[][body]=<body>" \
  # …repeat the four comments[] fields per comment…
```

GitLab — one discussion per comment:

```bash
glab api --method POST "projects/:id/merge_requests/<iid>/discussions" \
  --form body="<body>" \
  --form "position[position_type]=text" \
  --form "position[new_path]=<file>" \
  --form "position[new_line]=<line>" \
  --form "position[base_sha]=<base_sha>" \
  --form "position[start_sha]=<start_sha>" \
  --form "position[head_sha]=<head_sha>"
```

Use `--form` (multipart), not plain `-F`: GitLab expands the bracketed
`position[…]` keys from form encoding, while `--field`/`--raw-field`
serialize into a JSON body where `position[position_type]` stays a
literal flat key. (If your GitLab only accepts a JSON body, send one
`-F 'position={"position_type":"text","new_path":"<file>",…}'` — `-F`
parses leading-`{` values as JSON.) A comment on a removed line anchors
with `position[old_path]` / `position[old_line]` instead of the
`new_*` pair.

**After posting.** Report: count posted, one URL per discussion/review
comment, and every failure verbatim — a 4xx on comment 3 leaves
comments 1–2 posted, so state exactly what landed and what didn't.
v1 does NO write-back: the sidecar entries keep no published marker,
your report is the record of what went out.

## inbox: the reviewer feedback queue

File-backed queue at `.adhdiff/inbox-current.json`, relative to cwd — no
daemon, no socket; the file is the interface. Run `adhdiff inbox` from the
same checkout the UI was launched from. Poll it between turns to react to
human feedback:

```bash
adhdiff inbox --peek                                        # pending actions as JSON (non-destructive)
adhdiff inbox --post "re-check the P2 labels" --hunk src/main.rs:40   # post an ask, optionally anchored at FILE:ROW
adhdiff inbox --done <id>                                   # mark an action resolved
```

Actions are JSON objects `{ id, kind, hunk?, text, ts_unix, done }`.
The only kind produced is `ask`, posted by `adhdiff inbox --post`
(with an optional `--hunk FILE:ROW` anchor, `ROW` a diff row). `--peek` prints only unresolved (`done: false`) actions.

## Common pitfalls

- **Do not invent coordinates.** Rows come from `adhdiff prompt` /
  `adhdiff parse`; your entries live inside those hunk ranges — one
  whole-hunk entry, or segments tiling the hunk's rows exactly (contained,
  no gaps, no overlaps), which `check` enforces.
- **Do not skip hunks.** The check enforces full coverage — if a hunk
  seems trivial, label it P5 mechanical with `mechanical: true`,
  don't drop it.
- **Do not label yourself `confidence: "high"` on everything.**
  The point of the field is to surface your uncertainty. Use
  `"medium"` or `"low"` whenever you'd describe the labeling as
  "probably right but I'm guessing."
- **P0 count > 1/4 of entries** means your gist-reading is too broad.
  Narrow it.
