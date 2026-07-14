# Rule fragment for repos that already run meguri

`meguri agent-skills install --project` installs the block below as `.claude/rules/meguri.md`
(re-running it updates in place — safe, idempotent). Without that command, fold it by hand into
this repo's `AGENTS.md` / `CLAUDE.md` instead (as its own short section — don't paste the whole
file, just this). Either way it's meant to keep any coding agent that later works in this repo,
meguri-driven or not, from stepping on the loop's own state, and to nudge it toward using
meguri as its delegation target for independent work instead of doing everything inline.

<!-- meguri:rule-fragment:start -->
## meguri

This repo runs [meguri](https://github.com/kkato1030/meguri) to delegate work to background
coding agents.

- `meguri:*` labels are workflow state owned by meguri's loops — don't remove or repurpose one
  as a side effect of unrelated work.
- `meguri/<issue>-*` branches, and the `.meguri/` directory inside a meguri-managed worktree,
  belong to the loop — don't edit or delete them directly.
- When you notice an independent, self-contained chunk of work while doing something else,
  don't silently implement it yourself — propose filing it as an issue and labeling it
  `meguri:ready` (or `meguri:plan` if it needs a design decision first), and let the user
  decide.
<!-- meguri:rule-fragment:end -->
