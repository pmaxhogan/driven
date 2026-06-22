# Blockers

Per ROADMAP "How the agent should drive itself": when a milestone cannot
complete, do not stub it out and claim done. Surface the blocker here and
stop.

## M1 Phase 3 (integrate) - blocked 2026-06-21

The Phase 3 task assumed three Phase-2 commits had landed on `origin/main`
(or on three worktree-specific branches) from three parallel agents. None
of those commits exist anywhere reachable from this workspace.

Verified absent:
- `git log origin/main` stops at `935c838` (the Phase 1 interface-only
  commit). No Phase 2 commits past it.
- `git worktree list` shows only the main worktree.
- `git branch -a` shows only `main` / `origin/main`. No
  worktree-specific branches.
- `git reflog --all` has no Phase 2 entries.
- `gh api repos/pmaxhogan/driven/branches` returns only `main`.
- `gh pr list --state all` returns `[]`.
- `git remote -v` shows only `origin` (no second remote that could
  carry the commits).

Concretely missing on disk:
- `crates/driven-core/src/migrations/` (no SQL files; no `0001_*.sql`,
  no `0002_*.sql`).
- A `sqlx`-backed `StateRepo` impl behind the trait in
  `crates/driven-core/src/state.rs`.
- `crates/driven-drive/src/fake/` (no `InMemoryRemoteStore`, no
  fault-injection builders).
- A contract-test suite for the `RemoteStore` trait.
- `.sqlx/` offline-cache directory at the repo root.
- `driven-test-fixtures` is still a bare doc comment - no `tree!()`
  macro, no `FakeClock`, no `assert_remote_eq!()`, no fake-network
  harness.

## Unblock

Re-run M1 Phase 2 across three parallel agents so the three commits land
on `origin/main` (or on branches this workspace can fast-forward from).
Then re-run M1 Phase 3 (this task) against that state.

This task did not write any Phase 2 code itself. Doing so would have
silently expanded "integrate" into "implement all of M1" without the
parallel-work isolation Phase 2 was designed to get, and would have
risked colliding with the Phase 2 work if it lands later.
