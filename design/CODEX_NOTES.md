# Codex review notes - residual / deferred items

Durable record of approximations and deferred policy decisions surfaced during
the codex rechecks. Each entry is a known, accepted residual (not a bug), with
the milestone that resolves it.

## M2 nested-gitignore fidelity

The exclude matcher flattens nested `.gitignore` / `.ignore` files into ONE
`GitignoreBuilder` rooted at the source root - an approximation of git's
true per-directory scoping (where a rule in a nested ignore file applies only
under that directory, and a no-slash pattern matches at any depth below its
own file). This was accepted for M2.

Mitigated for data-safety by disabling directory-pruning when the matcher has
any negation (P1-1 fix): a nested `!keep.txt` under an excluded parent dir
would otherwise be classified INCLUDED by the flattened matcher while the
pruned (never-walked) directory left the file un-seen, so the orphan split
would false-classify the file as `deleted` and trash a file that still exists.
With pruning disabled whenever negations exist, the walk filter and the orphan
split decide every path through the same matcher, so they stay consistent and
no still-present file is ever trashed.

True per-directory matching is deferred: it would need the `ignore` crate's
native per-directory matcher stack mirrored for the orphan classification path
(the orphan split must reach the identical decision the walk did for a path
that is NOT currently on disk, so it cannot simply reuse the walker's per-dir
state).

## M2 NFC collision policy

NFC collisions (two byte-distinct raw on-disk paths that normalise onto one
`RelativePath` key, DESIGN s5.2.3 / SPEC s24 `local.unicode_collision`) flow
from the scanner through to `Plan.collisions` (P1-3 plumbing). The scanner
keeps the first-seen file and drops the later collider; M2 does NOT block or
fail on a collision.

The M3 orchestrator owns the user-surfacing + fail-closed policy: it must
surface `local.unicode_collision` as an activity error and decide fail-closed
(block the whole source) vs skip-the-colliding-file-with-an-error.

Recommended: skip the colliding file and surface an error - do NOT block the
whole source. Blocking an entire source's backup over one ambiguous filename is
a disproportionate failure mode; the rest of the source should continue to back
up while the single colliding file is reported for the user to rename.
