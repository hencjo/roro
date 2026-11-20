# Development and releases

Use `devenv shell`, then run `cargo fmt --check`,
`cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`,
`./scripts/tests/release.sh`, and `./scripts/tests/artifacts.sh`.
Set `RELEASE_TEST_REAL=1` for the release tests to exercise the installed
cargo-release against temporary Git repositories and a local bare remote.
Tests never push this repository or upload actual GitHub assets.

Run `./scripts/check-release` for the complete gate, including gitleaks scanning
of every Git ref, staged files and nonignored working files. The public RSA test
fixture is exempted only at its exact path and pinned bytes; never use it outside
tests. Secret scanning is not a substitute for reviewing source/history privacy.

## Changelog-driven versions

Use Conventional Commit summaries, but write release notes by hand under
`## [Unreleased]` in `CHANGELOG.md`. The highest populated category determines
the version bump:

- **Breaking changes:** minor on 0.x, major on 1.x and later.
- **Added**, **Changed:** minor.
- **Fixed**, **Security**, **Documentation:** patch.
- **Upgrade notes:** instructions accompanying another populated category.

Empty sections/comments do not count. Unknown/duplicate categories, malformed
fences/comments, and empty releases fail validation. Use nested headings inside
categories. The release hook stamps the UTC date, replaces `{{version}}` and
`{{date}}` in the current notes, and opens a fresh Unreleased skeleton. It never
rewrites historical sections. Do not manually bump Cargo versions.

## Local release

Fransson's local changelog/version/tag flow is the model. Roro also builds and
uploads its artifacts **locally**; GitHub Actions checks changes but never cuts
or publishes a release. No release PRs, PAT automation, or crates.io publication.

Requirements: Linux x86_64 or aarch64, Nix with flakes, Git, cargo-release (1.1.1-compatible),
Rust checks, Python 3.11+, gitleaks, shellcheck, Bash/awk, `file`, GNU tar, binutils/readelf, sha256sum, and an authenticated `gh` with
permission to publish releases. Enter `devenv shell` for the tools. Local Git
credentials must permit atomic pushes of the default branch and annotated release tags.

```sh
./scripts/release                    # read-only preview, even on dirty work
./scripts/release --version 1.0.0     # explicit stable version >= inferred version
# Commit and push normal work first, then use the clean default branch/upstream:
./scripts/release --execute
```

Execution discovers origin's default branch, fetches, and requires an exact upstream match,
including no untracked files. It runs cargo-release's isolated **version, hook,
commit, tag** steps, then local checks and a pinned `nix build .#static`. The
packaged static binary must report the expected version and pass ELF inspection
(no interpreter or shared-library dependencies). All Nix package aliases build musl. Only after the build succeeds does the script atomically push the
release commit/tag and upload binary and exact-source archives/checksums to a
draft GitHub release; it publishes that draft only after all four uploads succeed.

The native-host artifact is `dist/roro-<version>-linux-<arch>-static.tar.gz` plus
`.sha256` (`x86_64` or `aarch64`). Each architecture needs its own native build;
declaring a target is not a claim it has been tested. A matching
`roro-<version>-source.tar.gz` and checksum accompany every binary.
It contains the binary, README, LICENSE, changelog, example configuration, and
acceptance documentation/smoke script, dependency license notices, and
`release.json` recording commit, target and source SHA-256. This is traceability,
not a claim of bit-for-bit reproducible builds. `dist/` is ignored. No GitHub token is
required for building. Nothing is released merely by running tests or pushing
normal work.

## Local-only release (no remote required)

Use `./scripts/release --local` to preview and
`./scripts/release --execute --local` to create the version/hook/commit/annotated
tag and build the static archive/checksum locally. Main/master and a clean tree are
still required; no origin/upstream is required and there are no fetches, pushes,
or uploads. Existing tags are never moved. After a build failure, inspect the
existing tag and retry `./scripts/package roro-v<VERSION>` instead of bumping again.
Releasing does not deploy.

## Retry without moving tags

The script stops on failure; it does not reset work, bump again, or move tags.
Inspect status, recent commits, local tags, and `git ls-remote --tags origin` first.

- Before a release commit/tag: review partial generated edits and repair only
  those changes. Never discard unrelated work.
- Tag exists locally but build/push failed: fix the local toolchain (not tagged
  source), run `./scripts/package roro-v<VERSION>`, then
  `git push --atomic origin HEAD:refs/heads/<default-branch> refs/tags/roro-v<VERSION>`.
  Upload with `./scripts/publish --execute --skip-build roro-v<VERSION>`.
- Tag already pushed but build/upload needs retry: check out the exact tag with
  a clean tree. Run `./scripts/publish roro-v<VERSION>` for a preview, then add
  `--execute` to rebuild locally and upload. `--skip-build` reuses locally verified
  archives/checksums; exact tagged source, provenance, architecture and static ELF
  are rechecked before upload. Only existing **drafts** can be reused and their
  assets replaced. Published releases are immutable. API/auth failures stop publication rather than masquerading as a
  missing release.
- If tagged source is wrong, make a new release; never force-push or recreate a
  published tag. If commit succeeded but tag creation failed, verify the version
  and notes before retrying only `cargo release tag --isolated --config release.toml
  --execute`, followed by package/push/upload above.

Jix-style recovery helpers are preview-first too:

```sh
./scripts/release --build roro-v<VERSION>            # preview
./scripts/release --build roro-v<VERSION> --execute  # checks + static build
./scripts/release --upload roro-v<VERSION> --execute # verify packages and upload
```

`./scripts/release --verify-tag TAG` checks the annotated tag, checkout, manifest,
and release notes. `./scripts/release --notes VERSION` renders just that release.
Do not invoke the internal `--stamp` hook manually.
