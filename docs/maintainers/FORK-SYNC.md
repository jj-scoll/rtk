# Fork Sync Runbook

How to pull `rtk-ai/rtk` into `jj-scoll/rtk` without losing fork changes.

Fork-local file: upstream does not have it, so it never conflicts.

## Upstream branch model

Upstream runs two branches, and picking the wrong one is the most common mistake:

| Branch | Contains | `Cargo.toml` version |
|---|---|---|
| `master` | released code only; release-please bumps the version here | current release (e.g. `0.49.0`) |
| `develop` | integration branch, ~175 commits ahead of master | lags one release behind (e.g. `0.48.0`) |

The version bump lands on `master` and flows back to `develop` later. So a
`develop` merge gives you *more* code but an *older* version string. Decide which
you want before merging:

- **Track releases** — `git merge v0.49.0`. Conservative, version stays coherent.
- **Track develop** — `git merge upstream/develop`. All unreleased work; expect
  `Cargo.toml` to still show the previous version until upstream merges the bump back.
- **Both** — merge `develop`, then `upstream/master` on top. Conflicts only in
  `Cargo.toml` / `CHANGELOG.md` / `Cargo.lock`.

## Procedure

```bash
git remote add upstream https://github.com/rtk-ai/rtk.git   # once
git push origin develop                                     # remote copy before touching anything
git branch backup/pre-<version>-sync                        # cheap undo point
git fetch upstream --tags
git merge upstream/develop                                  # or v0.<N>.0 — see above
```

Merge, never rebase. Fork history is already merge-based, and rebasing ~10 fork
commits over 300 upstream commits redoes every conflict on every sync.

Recover with `git merge --abort`, or `git reset --hard backup/pre-<version>-sync`.

## Resolving conflicts

Most conflicts are "both sides appended to the same region", not real disagreements.

**Read all three sides before deciding.** Extract them with:

```bash
for s in 1 2 3; do git show :$s:<path> > /tmp/f.$s; done   # 1=base 2=ours 3=theirs
diff -u /tmp/f.1 /tmp/f.2                                   # what the fork actually changed
```

Bulk-resolve a file by taking upstream for every hunk, keeping the
auto-merged parts intact (`git checkout --theirs` would discard those):

```bash
awk '
/^<<<<<<< /{inc=1;side="ours";next}
/^=======$/{if(inc){side="theirs";next}}
/^>>>>>>> /{inc=0;side="";next}
{ if(!inc || side=="theirs") print }
' <path> > /tmp/resolved && mv /tmp/resolved <path>
```

Then re-append the fork-only additions by hand. Verify a known fork marker
survived, e.g. `grep -c localtime src/core/tracking.rs`.

### Which side wins

| Situation | Take |
|---|---|
| Both appended distinct tests to `mod tests` | Both |
| Upstream built a general mechanism where the fork had a narrow one | Upstream — then check the fork's behavior is still covered (see below) |
| Upstream deliberately *removed* something the fork extended | Upstream; their removal is usually a bug fix with an issue number in the comment |
| Pure ordering or formatting difference | Upstream |
| Doc comment contradicts the auto-merged code body | Upstream |

Prefer upstream's abstraction over reinstating the fork's parallel one. Where the
fork's *behavior* is then missing, re-add it by calling upstream's helper, not by
reintroducing the fork's implementation.

## Known traps

- **`automod::dir!` → explicit `pub mod`.** Upstream replaced directory auto-modules
  with explicit declarations plus a `build.rs` guard. Fork-added modules silently
  vanish from the module tree; the build panics naming the file. Add the `pub mod`
  line alphabetically.
- **Duplicate tests.** When upstream independently implements a fork feature,
  identically-named tests collide (`E0428: defined multiple times`). Delete the
  fork's copy — upstream's usually asserts the newer flag spelling.
- **Classify/rewrite asymmetry.** `src/discover/registry.rs` has two paths that must
  agree. Upstream has landed changes in `rewrite_command` without the matching
  `classify_command` change; a fork test failing on `Unsupported { base_command: ... }`
  is this. Fix in `classify_command` by calling the helper `rewrite` already uses.

## Gate

Non-negotiable before committing the merge:

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```

`cargo build` alone catches the module-declaration trap; the test run catches
duplicate tests and lost fork behavior. Zero clippy warnings.

Then verify the fork's own features still route end to end, e.g.:

```bash
cargo test --all duckdb
```
