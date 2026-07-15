# Setting up meguri in this repo

Follow these steps in order. Steps marked **STOP — confirm with the user** are hard gates:
don't decide these for them, ask explicitly and wait for an answer before moving on.

1. **Check meguri is installed.** Run `meguri --version`. If it's missing, point the user at
   the README's install section (`cargo install`, or a prebuilt binary) — don't install it on
   their behalf without saying so.

2. **Look at the repo to propose a `check_command`.** Find how this repo already runs its
   tests/lint (`package.json` scripts, a `Makefile`, `cargo test`, etc.) and propose one
   command meguri can run after every turn to independently verify a claimed success. Prefer
   the fastest command that still gives real confidence — unit tests, not a slow full E2E
   suite, unless that's genuinely all there is.

3. **Add the project with one command.** `meguri init` writes `~/.meguri/config.toml` if it
   doesn't exist yet (with zero live projects — the `[[projects]]` stub is commented out). Then
   add this repo in one step instead of hand-editing the file:

   ```
   meguri add-project owner/repo            # appends a [[projects]] entry and materializes the clone
   meguri add-project owner/repo --create   # also `gh repo create`s a brand-new repo (initial commit incl.)
   ```

   `add-project` appends to the config (comments and hand edits are preserved), clones the repo
   into `~/.meguri/repos/<id>`, and runs the environment checks in-line. Add `--id <id>` to
   override the derived id. The `check_command` from step 2 is optional (meguri still verifies
   the git conditions without it) — add it to the `[[projects]]` entry afterwards if you want it.

   If this repo has no GitHub remote, or the user doesn't want an agent touching issue labels,
   use `meguri add-project --local /abs/path/to/this/checkout` instead — a local-mode project:
   `repo_slug` isn't needed, `gh` isn't required, and work is queued with `meguri add` instead of
   labels (see the README's "Local mode" section).

   `--create` creates a real GitHub repo and **cannot be rolled back automatically** — meguri
   never deletes a repo it created. Only use it when the user actually wants a new repo.

4. **STOP — confirm with the user: yolo mode vs. gated permissions.** The default `[agent]`
   profile runs `--dangerously-skip-permissions` (fully unattended). Ask explicitly whether
   that's acceptable here, or whether they'd rather set
   `args = ["--permission-mode", "acceptEdits"]` — which still runs unattended-ish but leaves
   permission dialogs that need answering by attaching to the pane. Don't default to yolo
   silently.

5. **STOP — confirm with the user: which machine runs this.** meguri and the agent it drives
   need real shell/network access wherever they run. Confirm this is a machine, VM, or
   container whose blast radius the user actually accepts — don't assume "here, right now" is
   fine just because that's where the conversation is happening.

6. **Run `meguri doctor`.** It checks `gh` auth, the multiplexer, and the agent CLI. Resolve
   whatever it flags before moving on.

7. **Prove it out on one small issue.** Find, or ask the user to point at, one small,
   low-risk, well-scoped issue — or file one yourself if none exists (e.g. a one-line doc
   fix). On a GitHub-backed project, label it `meguri:ready`, then either run
   `meguri run --project <id> --issue <n>` or let `meguri watch` pick it up. On a local-mode
   project, `meguri add "..."` queues it and `meguri watch` is what picks it up —
   `meguri run --issue` only works for GitHub-backed projects. Either way, the goal is for the
   user to see one full loop succeed end to end — worktree, live pane, completion contract, PR
   (or local branch) — before trusting it with a bigger backlog.

8. **Report back.** Summarize what got configured, which label(s)/task(s) now exist, and
   point at `references/operate.md` for ongoing use.
