# Releasing gitcask

The release image is `ghcr.io/burrr-ai/gitcask:<version>`, built from the root `Dockerfile` for
`linux/amd64` and `linux/arm64`. Releases use `0.0.x` patch versions and Git tags named `v0.0.x`.
Deploy an exact patch tag or digest; no floating `latest`, `0`, or `0.0` image tags are published.

Every protected `main` push also builds amd64 and arm64 images in the immutable ECR repository
`188382150131.dkr.ecr.ap-northeast-2.amazonaws.com/gitcask`, scans both platforms, and uploads
`gitcask-build-evidence.json` plus the Trivy report to the successful **Release** workflow run.
Set the non-secret repository variables `GITCASK_RELEASE_ROLE_ARN` (GitHub OIDC role) and
`GITCASK_ECR_REPOSITORY_URI` (that ECR URI) before publishing.

## Prepare and publish

1. Change `[workspace.package].version` in `Cargo.toml` to the next unused patch version and run
   `cargo update --workspace` with the pinned toolchain to update the workspace entries in `Cargo.lock`.
   Update the image version shown in both READMEs.
2. Merge the release preparation PR after CI passes. Wait for the protected-`main` **Release** push run
   to succeed and upload its ECR evidence for the exact merge commit.
3. Create a lightweight `v0.0.N` tag directly on that qualified `main` commit and push it, for example:

   ```sh
   git tag v0.0.1 <merged-commit>
   git push origin v0.0.1
   ```

4. Watch the **Release** workflow. It validates tag/manifest/lockfile agreement and reuses the complete
   CI workflow, including the rustfs smoke test. Native amd64 and arm64 runners build the image with its
   source SHA and version labels, push untagged candidates to GHCR, and run `scripts/smoke-image.sh` against
   each candidate digest. That test checks both binaries, JWT authentication, Git and LFS push/clone,
   file API reads, and a new instance recovering from an empty cache against the same rustfs bucket.
5. Only after both images pass does the workflow publish the versioned multi-platform manifest and create
   a GitHub release containing the immutable image digest. An existing version tag is never overwritten;
   if publication fails after the manifest was created, finish the missing release metadata manually instead
   of rebuilding that version.

The workflow authenticates with `GITHUB_TOKEN` and job-scoped `packages: write`; no registry PAT is needed.
On the first publication, set the organization package's visibility to **Public** in GitHub package settings
and verify an anonymous pull. GHCR package visibility is separate from repository visibility. Keep the
`org.opencontainers.image.source` label so workflow access is linked to this repository.

## Local image verification

```sh
docker build --build-arg GITCASK_VERSION=0.0.1 --build-arg GITCASK_BUILD_SHA=local-smoke \
  -t gitcask:smoke .
scripts/smoke-image.sh gitcask:smoke local-smoke
```

The script needs Docker and Python 3. It creates an isolated Docker network, throwaway keys, rustfs, and
cache volumes, and removes only those resources on exit. It never writes the user's global Git config.
Run it on each architecture through the release workflow; a local run exercises the host architecture.

Runtime configuration and credentials are supplied by the operator as described in the README. The binary's
`--version` and health response retain the source SHA; the image label and tag carry the `0.0.x` version.
Rollback means selecting a previous image digest and its matching configuration.
