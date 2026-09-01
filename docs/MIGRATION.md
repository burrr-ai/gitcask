# Migrating repositories from Gitea

This runbook moves the Git history, branches, tags, and Git LFS objects for every repository owned by one
Gitea user or organization into gitcask. It does not modify or delete the Gitea repositories.

## Prerequisites

- A Gitea access token that can list the owner and read every selected repository. On current Gitea releases,
  grant `read:repository` and `read:user`, plus `read:organization` for an organization owner. The migrator reads
  the token owner's login because Gitea uses the token as the Git HTTPS password. Test the token against private
  repositories before the migration window.
- `git` on `PATH`. Install `git-lfs` too when any source repository uses LFS; the migrator only invokes it after
  finding an LFS pointer.
- A gitcask config file with access to the destination S3 bucket. The migrator uses the same WAL import path as
  `gitcask import`; a gitcask server does not need to be running.
- Local free disk for approximately the largest repositories processed at once. The default concurrency is 2,
  and each worker holds one mirror clone plus gitcask's local pack cache while it is active.
- Network access from the migration host to both Gitea and S3. Keep the state file on durable local storage for
  the duration of the migration.

## 1. Preview the migration

Set the token in the environment so it is not saved in shell history, then run a dry run:

```sh
export GITEA_TOKEN='<token>'
gitcask --config gitcask.toml migrate gitea \
  --url https://git.example.com \
  --owner acme \
  --to-owner acme \
  --dry-run
```

The preview lists the destination repository names and Gitea-reported Git sizes. It does not clone, create the
state file, or write to gitcask. To migrate only selected repositories, repeat `--repo`:

```sh
gitcask --config gitcask.toml migrate gitea \
  --url https://git.example.com \
  --owner acme \
  --repo api --repo web \
  --dry-run
```

Gitea's reported size does not include every temporary pack, local index, or LFS transfer. Do not use it as a
disk quota.

## 2. Run

Use an explicit state path and retain it until verification is complete:

```sh
gitcask --config gitcask.toml migrate gitea \
  --url https://git.example.com \
  --owner acme \
  --to-owner imported-acme \
  --concurrency 2 \
  --state /var/lib/gitcask-migration/acme.json
```

Without `--to-owner`, the destination owner is the Gitea owner. Without `--state`, the state file is
`./gitcask-migrate-state.json`.

The command prints the current repository as `[n/N]`, import and LFS progress, and a final summary. One failed
repository does not stop other workers. Any failure produces a final list with reasons and a nonzero exit code.
Only repositories that finish both Git and LFS are recorded complete.

### Expected time

Transfer time dominates. Estimate it from a representative repository: run that repository alone with
`--repo`, measure clone plus S3 publish time, then scale by total reported size and concurrency. LFS bytes are
additional and may dominate the estimate. Start with the default concurrency of 2; raise it only after checking
Gitea, migration-host disk, and S3 load.

## 3. Resume after interruption or failure

Run the exact same command with the same state path. Completed repositories are skipped. Failed or interrupted
repositories are retried, and the state file is atomically replaced after each success.

If the state file is lost, rerunning is still safe: a destination that already has a committed WAL entry skips
the Git publish, while the migrator clones the source again to discover and finish any LFS transfer. An existing
destination is never overwritten. Use a new destination owner if unrelated repositories already occupy the
target names.

After correcting a per-repository problem, such as missing `git-lfs`, expired credentials, or disk pressure,
rerun the same command. Do not edit the JSON state file by hand.

## 4. Verify before cutover

Keep Gitea read-only or otherwise quiescent while running the final migration and verification. For each
repository, create fresh mirrors from both systems:

```sh
git clone --mirror https://git.example.com/acme/api.git gitea-api.git
git clone --mirror https://gitcask.example.com/imported-acme/api.git gitcask-api.git

git -C gitea-api.git show-ref | sort > /tmp/gitea-api.refs
git -C gitcask-api.git show-ref | sort > /tmp/gitcask-api.refs
diff -u /tmp/gitea-api.refs /tmp/gitcask-api.refs

git -C gitea-api.git rev-list --all --count
git -C gitcask-api.git rev-list --all --count
git -C gitcask-api.git fsck --full
```

The `show-ref` diff should be empty and commit counts should match. The existing import contract publishes
local branches and tags; inspect those counts explicitly when the source also contains pull-request, note, or
remote-tracking refs. For LFS repositories, check out representative branches from the gitcask clone with LFS
enabled and verify that the large files materialize rather than remaining pointer text.

## 5. Cutover and rollback

Change clients or the platform routing only after verification. Keep the original Gitea repositories intact
and read-only through the rollback window. To roll back, route clients back to Gitea; no reverse conversion is
needed. Delete gitcask targets only after diagnosing the failure and confirming that no post-cutover pushes need
to be preserved.

## Known limitations

- This migrates Git data and LFS objects only. Gitea issues, pull requests, reviews, releases, packages, wiki,
  Actions, users, teams, permissions, hooks, and repository settings are outside gitcask's product boundary.
- The existing import filter publishes `refs/heads/*`, `refs/tags/*`, and the symbolic `HEAD` target. Gitea
  pull-request refs, notes, and remote-tracking refs are not migrated.
- Repository names must satisfy gitcask's ASCII naming rules. Name rewriting is not supported.
- An LFS object larger than the destination `lfs.max_object_bytes` limit fails that repository.
- The Gitea HTTP clone URL returned by the API is used. SSH-only access is not supported by this adapter.
- Submodule configuration is copied as Git content, but referenced repositories and external submodule URLs are
  not rewritten or migrated automatically.
- The local state file coordinates one migration process only. Do not run two migrators with the same state file
  or overlapping destination repositories.
