# Linux sandbox driver build

Once a week, GitHub Actions rebases the `linux-sbx-patch` branch of this fork
onto the newest upstream Cua Driver release, builds the Linux x86_64 archives,
signs them, and leaves the result as one run artifact. **Actions pushes nothing
and releases nothing.** Publishing (pushing the rebased branch and creating the
prerelease) happens on a maintainer's machine with `publish_local.sh` and that
machine's own `gh` login. No credential for this is stored on GitHub.

Why: the Actions job token may not push a commit that changes files under
`.github/workflows/`, and a weekly rebase onto upstream nearly always does. The
first live run (`publish=false` dispatch, rebase job) was rejected with
"`workflows` scope may be required". Storing a personal token as a secret was
ruled out, so the push moved to the machine that already holds a login.

| File | Where | Role |
| --- | --- | --- |
| `.github/workflows/linux-sbx-weekly.yml` | Actions | Orchestration only: schedule, permissions, job wiring |
| `select_upstream_tag.sh` | Actions | Highest stable `cua-driver-rs-vX.Y.Z` tag upstream |
| `next_prerelease_tag.sh` | Actions | `cua-driver-rs-vX.Y.Z-linux-sbx.<N>` candidate, one past the highest published |
| `rebase_onto_tag.sh` | Actions | Replay the patch commits onto that tag; drops generated commits |
| `prune_upstream_workflows.sh` | Actions | Generated commit that deletes every workflow except `linux-sbx-weekly.yml` |
| `make_source_bundle.sh` | Actions | `git bundle` of the rebased branch (above the tag) and `linux-sbx-metadata.txt` |
| `metadata.sh` | both | Strict reader for the metadata file (never sourced) |
| `report_conflict.sh` | Actions | Conflict issue, or an explicit error; always fails the run |
| `apt_bootstrap.sh` | Actions | Debian 11 build packages: frozen snapshot mirror, archive mirror as fallback |
| `checkout_from_bundle.sh` | Actions | Build source taken from the bundle, checked against the metadata |
| `pack_linux_artifact.sh` | Actions | Full and `-binary` archives, `SHA256SUMS`, `--smoke` start of the packaged CLI |
| `assemble_release.sh` | Actions | Build output + bundle into one directory, cross-checked |
| `sign_release_assets.sh` | Actions | cosign keyless signature, `.sig` and `.pem` per asset |
| `publish_local.sh` | local | Verify a run's artifact, push the branch, create the prerelease |
| `publish_prerelease.sh` | local | Release notes and `gh release create --prerelease --latest=false` |
| `verify_release_assets.sh` | local | Reads the release back: both archives, `SHA256SUMS`, all signature files |

Everything that decides something lives in these scripts, so each can be run
by hand, with `gh` and `apt-get` replaced by stubs on PATH.

## Prerequisites

- GitHub only reads scheduled workflows and the Run workflow button from the
  default branch. The fork's default branch needs to be set to
  `linux-sbx-patch` (schedule and workflow_dispatch only see the workflow on
  the default branch); until then neither the weekly run nor manual dispatch
  exists.
- Actions and Issues enabled on the fork. The job token never needs write
  access to contents.
- On the publishing machine: a checkout of the fork with `origin` pointing at
  it, and `gh` logged in with the `repo` and `workflow` scopes
  (`gh auth refresh -s workflow`). The branch the script pushes deletes workflow
  files, which needs `workflow`.

## What a weekly run does (Actions)

1. **Select.** Lists `refs/tags/cua-driver-rs-v*` upstream and picks the highest
   `X.Y.Z`. A prerelease suffix (`-rc.1`) or another component's tag is never
   picked, and the repository-wide "Latest" release is never consulted.
2. **Name.** Computes the `cua-driver-rs-v<version>-linux-sbx.<N>` candidate from
   the tags already in this repository.
3. **Rebase.** The patch commits are the commits on `linux-sbx-patch` that no
   stable upstream release tag reaches. Commits carrying the
   `Linux-Sbx-Generated:` trailer are dropped; the rest are replayed onto the
   tag. A conflict writes a report, aborts, and fails the job.
4. **Prune.** One generated commit deletes every file under
   `.github/workflows/` except `linux-sbx-weekly.yml`. The fork only runs this
   workflow, and upstream's release and CI workflows must not register here once
   the branch is pushed. The commit is reproducible (bot identity, dates from its
   parent): an unchanged week yields the same sha. It is dropped and regenerated
   by the next rebase, so it never conflicts with upstream's workflow edits.
5. **Bundle.** `git bundle` of the branch above the upstream tag, plus
   `linux-sbx-metadata.txt`: upstream tag and sha, rebased sha, the branch sha the
   run started from, bundle sha256, prerelease tag candidate, run id.
6. **Build.** `debian:11` container, all steps under Bash. The source comes out
   of the bundle (upstream tag fetched publicly, bundle verified, sha checked); the
   fork's remote is not read. Release build, Node runtime, archives, smoke test.
   Uploads the binaries with the metadata.
7. **Sign and attest.** Cross-checks build output against the bundle and
   metadata, signs the archives and `SHA256SUMS` with cosign keyless (OIDC), and
   records a GitHub build provenance attestation for the archives, `SHA256SUMS`,
   the bundle and the metadata. Everything is uploaded as the `linux-sbx-release`
   artifact (kept 30 days), and the run summary prints the publish command.

Permissions: `contents: read` to check out the branch, `issues: write` only
for the conflict report, `id-token: write` + `attestations: write` only for the
signing job. No job has `contents: write`.

## Publishing a run (local)

From a checkout of the fork, with the run id from the run page or
`gh run list --workflow linux-sbx-weekly.yml --status success --limit 1`:

```
.github/scripts/linux-sbx/publish_local.sh <run-id> --dry-run
.github/scripts/linux-sbx/publish_local.sh <run-id>
```

Before writing anything it verifies, in order: the active gh account's
`workflow` scope, and that `origin` is this repository on GitHub; that the run is
a successful run of this workflow; the downloaded metadata (run id, repository,
branch `linux-sbx-patch`, formats); the bundle's sha256; `SHA256SUMS`; every
archive's cosign files; `gh attestation verify` for each archive, `SHA256SUMS`,
the bundle and the metadata (signer: this repository's `linux-sbx-weekly.yml`,
run from `refs/heads/linux-sbx-patch`); `git bundle verify` against the upstream
tag and that the bundle's branch head is the recorded sha.
Then:

- **Push.** Only if `origin/linux-sbx-patch` is still the sha the run started
  from (otherwise someone pushed since, and the script stops instead of
  overwriting it). The push is
  `git push --force-with-lease=refs/heads/linux-sbx-patch:<remote sha now>`.
  Already at the rebased sha: skipped.
- **Release.** `gh release create <tag> --target <rebased sha> --prerelease
  --latest=false` with the archives, `SHA256SUMS` and their signature files, then
  read back to confirm every file is attached. If any linux-sbx prerelease tag
  already points at the rebased sha (an unchanged week, or a re-run), no new
  release is made. If the candidate tag was taken by a different commit meanwhile,
  the script stops; dispatch a new run for a fresh number.

Re-running after a complete publish is a no-op. A publish interrupted inside
`gh release create` (which uploads to a draft and then publishes it) is not
repaired automatically: the read-back fails, and the draft or incomplete release
has to be finished or deleted by hand before re-running.

`--dry-run` runs every verification for real and only prints the push and the
release commands. Any failed check exits non-zero before the first write. The
script never prints a token. It fetches the upstream tag and the bundle into
local refs (`refs/linux-sbx/incoming`); nothing else in the checkout changes.

## Weekly operation

Each week loopx runs `publish_local.sh` on this machine once for the newest
successful run, then writes an operation record (run, upstream tag, branch
old -> new sha, prerelease tag, or the failure) on sandbox-muse Issue #550. If the
machine is offline that week, that week is not published; the next weekly run
rebases from the unpublished branch again and supersedes it. A missed run can
still be published by hand while its artifact lasts (30 days), as long as the
branch has not moved since.

### How the local publish is triggered

- **Recommended: a loopx-d timed thread**, Monday late afternoon (the cron is
  Monday 05:23 UTC and a build takes about 1.5 hours). It looks up the newest
  successful run (`gh run list ... --status success --limit 1`), skips it if that
  run's prerelease is already published, otherwise runs `--dry-run`, then the
  real publish, and writes the Issue record. Reason: loopx-d already runs timed
  work on this machine with this login. It adds no GitHub-side credential, no
  inbound hook, and nothing to configure on the fork, and an offline week
  degrades to "not published", as intended.
- Alternative: listen for `workflow_run` completion and wake the loop through an
  Issue comment. It is faster (no fixed delay), but someone on GitHub has to post
  into sandbox-muse, which needs a token stored there; that is what this design
  avoids.
- Alternative: run it by hand after looking at the run page. Simplest, and the
  fallback when loopx is down, but it depends on someone remembering every week.

Nothing in this repository installs the timer; it belongs to loopx's own
configuration.

## Running the workflow by hand

Actions, **Weekly: Linux sandbox driver**, **Run workflow**. One input:
`upstream_tag` (empty by default) rebases onto this exact stable
`cua-driver-rs-vX.Y.Z` tag instead of the newest one. Every run is build-only on
GitHub; publishing is always the local step above.

## Resolving a conflict

A conflict fails the run and opens an issue titled
`Rebase conflict: linux-sbx-patch onto <tag>`. The issue body is the same
Markdown report that is uploaded as the `rebase-conflict` artifact and shown on
the run summary: the upstream tag and its commit, the branch and its commit, the
patch commit that failed, the conflicting files, `git status`, and the commands
to resolve it. If the issue cannot be created, the run still fails, with an
explicit error naming the artifact. A rebase job that failed for any other reason
(no tag, prune or bundle failure) is reported as such, also red.

To resolve it, locally:

```
git fetch origin linux-sbx-patch
git fetch https://github.com/trycua/cua tag cua-driver-rs-vX.Y.Z
git switch linux-sbx-patch
git rebase -i cua-driver-rs-vX.Y.Z  # drop the Linux-Sbx-Generated commit; resolve, then git rebase --continue
git push --force-with-lease origin linux-sbx-patch
```

The next weekly run regenerates the pruning commit on top.

The next run starts from the resolved branch. Close the issue yourself.

## Changing the schedule

The cron lives in the workflow's `on.schedule`:

```yaml
on:
  schedule:
    - cron: "23 5 * * 1"
```

That is Monday 05:23 UTC, off the hour so it does not queue behind GitHub's
hourly jobs. Scheduled runs use the default branch's copy of this workflow, so a
new cron takes effect once it is pushed there. Move the loopx publish time with
it. To stop the weekly run, delete the `schedule` block and keep
`workflow_dispatch`.

## Verifying a published archive

```
TAG=cua-driver-rs-v0.41.3-linux-sbx.1
gh release download "$TAG"
sha256sum -c SHA256SUMS
for f in *.tar.gz SHA256SUMS; do
  cosign verify-blob --bundle "$f.sigstore.json" \
    --certificate-identity-regexp "https://github.com/<owner>/<repo>/" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com "$f"
  gh attestation verify "$f" --repo <owner>/<repo>
done
```

The signing identity is this repository's workflow through GitHub OIDC. No
private key exists anywhere.

## Reusing the upstream release workflow

The build here mirrors the `build-linux` job of
`.github/workflows/cd-rust-cua-driver.yml` rather than calling it through
`workflow_call`. Under `workflow_call`, that workflow's Windows and macOS jobs
run unconditionally and need Azure and Apple signing credentials this fork does
not have. (Pruning removes that file from the published branch; the build was
already independent of it.)

The container is `debian:11` for the same reason it is upstream: glibc 2.31 is
the floor, so the binaries run on Debian 11+ and Ubuntu 20.04+.
Building on `ubuntu-latest` would silently raise that floor.
