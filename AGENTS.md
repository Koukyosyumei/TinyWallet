
## h5i Integration

This repository uses **h5i** (a Git sidecar for AI-era version control).

Codex should use `h5i context` as shared cross-session memory and `h5i commit` to record AI provenance on code commits.

### Workflow

**At the start of a non-trivial task:**
```bash
h5i codex prelude
# If no workspace exists yet, initialize it once:
h5i context init --goal "<one-line task summary>"
```

**While working:**
```bash
h5i context relevant <file>   # before editing — surfaces prior reasoning + claims that mention this file
h5i codex sync                # after a burst of reads/edits — backfills OBSERVE/ACT traces
h5i context trace --kind THINK "<chosen approach> over <rejected alternative> because <reason>"
h5i context trace --kind NOTE "TODO: … / LIMITATION: … / RISK: …"
```

**After a logical milestone:**
```bash
h5i codex finish --summary "<milestone summary>"
```

### Claims — pin reusable facts

After establishing a non-obvious fact a future session would otherwise re-derive
(where a helper lives, which module owns a concern, a subtle invariant), record
a content-addressed claim pointing at the files that back it. Live claims are
injected into `h5i codex prelude` / `h5i context prompt`, so the next session
treats them as pre-verified — trust them; don't re-read the files.

**Two flavors:**

Cross-cutting fact (~30 tokens, multiple paths):
```bash
h5i claims add "HTTP only src/api/client.py: fetch_user, create_post, delete_post." \
  --path src/api/client.py
```

Per-file orientation (~80 tokens, single path) — replaces the deprecated `h5i summary`:
```bash
h5i claims add "src/api/client.py | HTTP. fetch_user(id: int)→dict GET, create_post(...)→dict POST, delete_post(id: int)→bool DELETE. Logger \`log\` top." \
  --path src/api/client.py
```

Inspect:
```bash
h5i claims list                    # live / stale badges
h5i claims list --group-by-path    # claims grouped by file ("what's known about each file")
h5i claims prune                   # drop stale claims
```

**Caveman style.** Drop articles, copulas, fluff. Keep paths, identifier names, types, numbers exact. Pick the *minimum* evidence-path set: most good claims cite 1 file; >3 is a red flag you're confusing "files I read" with "files that back the claim". Live claim text is re-read on every cached-prefix turn forever — every word costs forever.

### Code commits

```bash
git add <exact paths>
h5i commit -m "…" --agent codex --prompt "…"
```

Add flags when relevant:
- `--tests`  — tests were added or modified
- `--audit`  — security-sensitive or high-risk changes

### Sharing h5i Data

```bash
h5i push   # push all h5i refs to origin
h5i pull   # pull h5i refs from origin
```

