# Linux sandbox driver build

Weekly GitHub Actions job that rebuilds the `linux-sbx-patch` branch of this
fork on top of the newest upstream Cua Driver release, builds the Linux x86_64
archives, and publishes them as a prerelease here.

| File | Role |
| --- | --- |
| `.github/workflows/linux-sbx-weekly.yml` | Orchestration only: schedules, permissions, job wiring |
| `select_upstream_tag.sh` | Highest stable `cua-driver-rs-vX.Y.Z` tag upstream |
| `rebase_onto_tag.sh` | Replay the branch's patch commits onto that tag |
| `next_prerelease_tag.sh` | `cua-driver-rs-vX.Y.Z-linux-sbx.<N>`, one past the highest published |
| `report_conflict.sh` | Conflict issue, or an explicit error when it cannot be opened; always fails the run |
| `apt_bootstrap.sh` | Debian 11 build packages: frozen snapshot mirror, archive mirror as fallback |
| `pack_linux_artifact.sh` | Full and `-binary` archives, `SHA256SUMS`, `--smoke` start of the packaged CLI |
| `sign_release_assets.sh` | cosign keyless signature, `.sig` and `.pem` per asset |
| `publish_prerelease.sh` | Release notes and `gh release create --prerelease --latest=false` |
| `verify_release_assets.sh` | Reads the release back: both archives, `SHA256SUMS`, and all three signature files each |

Everything that decides something lives in these scripts, so each can be run
by hand against a checkout, with `gh` and `apt-get` replaced by stubs on PATH.
The workflow only holds triggers, permissions, env and step order.

## Prerequisite: the default branch

GitHub only reads scheduled workflows and the Run workflow button from the
default branch. The fork's default branch therefore needs to be set to
`linux-sbx-patch` (schedule and workflow_dispatch only see the workflow on the
default branch); until then neither the weekly run nor manual dispatch exists.
Actions and Issues also need to be enabled on the fork, and the workflow token
needs write access so the branch push and the release can succeed.

## What a run does

1. **Select.** Lists `refs/tags/cua-driver-rs-v*` upstream and picks the highest
   `X.Y.Z`. A prerelease suffix (`-rc.1`, `-beta.2`) or another component's tag
   is never picked, and the repository-wide "Latest" release is never consulted.
2. **Name.** Computes `cua-driver-rs-v<version>-linux-sbx.<N>` from the tags that
   already exist in this repository.
3. **Rebase.** The patch commits are the commits on `linux-sbx-patch` that no
   stable upstream release tag reaches. They are replayed with
   `git rebase --onto <tag> <old base>`. A clean replay pushes the branch back
   with `--force-with-lease`; a conflict writes a report, aborts the rebase and
   exits 10 without pushing anything.
4. **Build.** `debian:11` container, x86_64, release profile, then the Node
   runtime, the archives, a `--version` smoke test, and an artifact upload.
   Every step runs under Bash (`defaults.run.shell`): the container's `sh` is
   dash. The build checks out the exact commit the rebase pushed.
5. **Publish.** Checks out that same commit, signs every asset keylessly
   through GitHub OIDC, creates the prerelease with `--latest=false`, and reads
   the release back to confirm every archive and signature is attached.

## Running it by hand

Actions, **Weekly: Linux sandbox driver**, **Run workflow**. Three inputs:

- `upstream_tag` (string, empty by default): rebase onto this exact tag instead
  of the newest one. It must be a stable `cua-driver-rs-vX.Y.Z` tag; the script
  refuses a prerelease.
- `publish` (boolean, on by default): turn it off to rebase and build only.
  The archives and `SHA256SUMS` are kept as the `linux-sbx-x86_64` run artifact,
  **unsigned**: signing happens only in the publish job, which is skipped.
  Nothing is tagged or released.
- `skip_push` (boolean, off by default): rehearse the rebase without pushing
  `linux-sbx-patch`. This also skips the build, because the build job checks out
  the pushed commit rather than the runner-local one.

Publishing is what creates the tag, and the tag is pushed with `GITHUB_TOKEN`,
so a release does not start another workflow run.

## Resolving a conflict

A conflict fails the run and opens an issue titled
`Rebase conflict: linux-sbx-patch onto <tag>`. The issue body is the same
Markdown report that is uploaded as the `rebase-conflict` artifact: the upstream
tag and its commit, the branch and its commit, the patch commit that failed, the
conflicting files, `git status`, and the commands to resolve it.

If the issue cannot be created, the run still fails, with an explicit error
naming the artifact. Issues being disabled on a fork never turns a conflict into
a green run.

To resolve it:

```
git fetch origin linux-sbx-patch
git switch linux-sbx-patch
git rebase cua-driver-rs-vX.Y.Z     # resolve, then git rebase --continue
git push --force-with-lease origin linux-sbx-patch
```

The next weekly run then starts from the resolved branch. The report is not
retracted when the conflict is fixed; close the issue yourself.

## Changing the schedule

The cron lives in the workflow's `on.schedule`:

```yaml
on:
  schedule:
    - cron: "23 5 * * 1"
```

That is Monday 05:23 UTC, after the weekend's release merges and off the hour so
it does not queue behind GitHub's hourly jobs. Scheduled runs use the default
branch's copy of this workflow (see the prerequisite above), so a new cron takes
effect once it is pushed there. To stop the weekly run, delete the `schedule`
block and keep `workflow_dispatch`.

## Verifying a published archive

```
TAG=cua-driver-rs-v0.41.3-linux-sbx.1
gh release download "$TAG"
sha256sum -c SHA256SUMS
for f in *.tar.gz SHA256SUMS; do
  cosign verify-blob --bundle "$f.sigstore.json" \
    --certificate-identity-regexp "https://github.com/<owner>/<repo>/" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com "$f"
done
```

The signing identity is this repository's workflow through GitHub OIDC. No
private key exists anywhere: a public fork cannot hold one, and a keyless
signature is what an installer can verify against the workflow identity.

## Reusing the upstream release workflow

The build here mirrors the `build-linux` job of
`.github/workflows/cd-rust-cua-driver.yml` rather than calling it through
`workflow_call`. Under `workflow_call`, that workflow's Windows and macOS jobs
run unconditionally and need Azure and Apple signing credentials this fork does
not have, so every run would fail before the Linux job reported anything. The
upstream file is left untouched.

The container is `debian:11` for the same reason it is upstream: glibc 2.31 is
the floor, so the binaries run on Debian 11+ and Ubuntu 20.04+.
Building on `ubuntu-latest` would silently raise that floor.
