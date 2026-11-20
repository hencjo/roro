#!/usr/bin/env bash
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME='Artifact Test' GIT_AUTHOR_EMAIL='release@example.invalid'
export GIT_COMMITTER_NAME=$GIT_AUTHOR_NAME GIT_COMMITTER_EMAIL=$GIT_AUTHOR_EMAIL
repo="$tmp/repo"
remote="$tmp/remote.git"
mkdir -p "$repo/scripts" "$repo/docs" "$tmp/bin"
cp "$root/scripts/"{release,changelog.awk,package,publish,check-static,check-package,licenses} "$repo/scripts/"
printf 'smoke\n' > "$repo/scripts/ci-smoke.sh"
printf 'acceptance\n' > "$repo/docs/acceptance.md"
for name in README.md CONTRIBUTING.md LICENSE SECURITY.md config.example.yaml; do printf 'fixture\n' > "$repo/$name"; done
printf '#!/usr/bin/env bash\nset -eu\ncargo fmt --check\ncargo clippy --all-targets --locked -- -D warnings\ncargo test --locked\n' > "$repo/scripts/check-release"
printf '#!/usr/bin/env python3\n' > "$repo/scripts/check-secrets"
chmod +x "$repo/scripts/check-release"
printf '[package]\nname = "roro"\nversion = "0.1.0"\n' > "$repo/Cargo.toml"
printf '# Changelog\n\n## [0.1.0] - 2026-10-15\n\n### Fixed\n\nArtifact test release notes.\n' > "$repo/CHANGELOG.md"
printf '/dist/\n' > "$repo/.gitignore"
git init -q --bare --initial-branch=master "$remote"
git init -q -b master "$repo"
git -C "$repo" add .
git -C "$repo" commit -qm 'test: fixture'
git -C "$repo" tag -a roro-v0.1.0 -m 'test release'
git -C "$repo" remote add origin "$remote"
git -C "$repo" push -qu --atomic origin master refs/tags/roro-v0.1.0
export ARTIFACT_TRACE="$tmp/trace"
cat > "$tmp/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ ${FAKE_CARGO_FAIL:-false} != true ]]
printf 'cargo %s\n' "$*" >> "$ARTIFACT_TRACE"
if [[ $1 == metadata ]]; then printf '{"packages":[]}\n'; fi
STUB
cat > "$tmp/bin/nix" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $* == 'build .#static --no-update-lock-file --out-link dist/build' ]]
[[ ${FAKE_NIX_FAIL:-false} != true ]]
printf 'nix build\n' >> "$ARTIFACT_TRACE"
mkdir -p dist/build/bin
printf '#!/usr/bin/env bash\nprintf "roro %s\\n"\n' "${FAKE_BUILT_VERSION:-0.1.0}" > dist/build/bin/roro
chmod +x dist/build/bin/roro
if [[ ${FAKE_DIRTY_BUILD:-false} == true ]]; then printf 'unexpected build edit\n' >> README.md; fi
STUB
cat > "$tmp/bin/file" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${FAKE_DYNAMIC:-false} == true ]]; then echo 'ELF dynamically linked'; else echo 'ELF static-pie linked'; fi
STUB
cat > "$tmp/bin/readelf" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$2" in
    --file-header) printf '  Type: DYN (Position-Independent Executable file)\n  Machine: %s\n' "${FAKE_MACHINE:-Advanced Micro Devices X86-64}" ;;
    --program-headers)
        printf '  LOAD 0x0000\n'
        if [[ ${FAKE_INTERPRETER:-false} == true ]]; then printf '  INTERP 0x0000\n'; fi ;;
    --dynamic)
        if [[ ${FAKE_NEEDED:-false} == true ]]; then printf '  0x00000001 (NEEDED) Shared library: [libssl.so]\n'; fi ;;
    *) exit 1 ;;
esac
STUB
cat > "$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'gh %s\n' "$*" >> "$ARTIFACT_TRACE"
case "$1 $2" in
    'repo view') echo test/roro ;;
    'api --paginate') [[ ${FAKE_API_FAIL:-false} != true ]]; if [[ -n ${FAKE_EXISTING_TAG:-} ]]; then printf '%s\t%s\n' "$FAKE_EXISTING_TAG" "${FAKE_DRAFT:-true}"; fi ;;
    'release create') [[ $* == *--verify-tag* && $* == *--draft* ]]; [[ ${FAKE_CREATE_FAIL:-false} != true ]] ;;
    'release upload') [[ $* == *--clobber* ]]; [[ ${FAKE_UPLOAD_FAIL:-false} != true ]] ;;
    'release edit') [[ $* == *--draft=false* ]] ;;
    *) exit 1 ;;
esac
STUB
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH"
count=0
pass() { count=$((count+1)); printf 'ok %s - %s\n' "$count" "$*"; }
fail() { printf 'not ok - %s\n' "$*" >&2; exit 1; }
reject() { if "$@" > "$tmp/output" 2>&1; then fail "unexpected success: $*"; fi; }
cd "$repo"
archive=dist/roro-0.1.0-linux-x86_64-static.tar.gz
./scripts/package roro-v0.1.0 > "$tmp/output"
(cd dist && sha256sum --check "$(basename "$archive").sha256")
tar -tzf "$archive" > "$tmp/members"
for member in roro README.md CONTRIBUTING.md LICENSE SECURITY.md THIRD_PARTY_NOTICES.md release.json CHANGELOG.md config.example.yaml docs/acceptance.md scripts/ci-smoke.sh; do
    grep -Fxq "roro-0.1.0-linux-x86_64-static/$member" "$tmp/members" || fail "missing $member"
done
[[ -z $(git status --porcelain) ]] || fail 'packaging changed tracked source'
pass 'local checks/build, version/static inspection, archive contents and checksum'
: > "$ARTIFACT_TRACE"
./scripts/publish roro-v0.1.0 > "$tmp/output"
[[ ! -s $ARTIFACT_TRACE ]] || fail 'preview built or contacted GitHub'
pass 'publish preview has no build or GitHub side effects'
./scripts/publish --execute --skip-build roro-v0.1.0 > "$tmp/output"
create=$(grep -n '^gh release create ' "$ARTIFACT_TRACE" | cut -d: -f1)
upload=$(grep -n '^gh release upload ' "$ARTIFACT_TRACE" | cut -d: -f1)
edit=$(grep -n '^gh release edit ' "$ARTIFACT_TRACE" | cut -d: -f1)
((create < upload && upload < edit)) || fail 'draft/upload/publish order'
grep '^gh release upload ' "$ARTIFACT_TRACE" | grep -Fq 'dist/roro-0.1.0-source.tar.gz dist/roro-0.1.0-source.tar.gz.sha256' || fail 'source assets not uploaded'
pass 'draft created, four assets uploaded, release published last'
: > "$ARTIFACT_TRACE"
FAKE_EXISTING_TAG=roro-v0.1.0 ./scripts/publish --execute --skip-build roro-v0.1.0 > "$tmp/output"
if grep -q '^gh release create ' "$ARTIFACT_TRACE"; then fail 'retry recreated release'; fi
pass 'existing release/draft reused on retry'
: > "$ARTIFACT_TRACE"
FAKE_EXISTING_TAG=roro-v0.1.0 FAKE_DRAFT=false reject ./scripts/publish --execute --skip-build roro-v0.1.0
if grep -q '^gh release ' "$ARTIFACT_TRACE"; then fail 'published release was overwritten'; fi
pass 'published assets are immutable; only drafts can be retried'
: > "$ARTIFACT_TRACE"
export FAKE_NEEDED=true
reject ./scripts/publish --execute --skip-build roro-v0.1.0
unset FAKE_NEEDED
if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'dynamic archived binary reached GitHub'; fi
pass 'reused archives are ELF-checked before upload'
: > "$ARTIFACT_TRACE"
export FAKE_API_FAIL=true
reject ./scripts/publish --execute --skip-build roro-v0.1.0
unset FAKE_API_FAIL
if grep -q '^gh release ' "$ARTIFACT_TRACE"; then fail 'API failure mutated release'; fi
pass 'GitHub API failures never treated as absent releases'
: > "$ARTIFACT_TRACE"
export FAKE_UPLOAD_FAIL=true
reject ./scripts/publish --execute --skip-build roro-v0.1.0
unset FAKE_UPLOAD_FAIL
if grep -q '^gh release edit ' "$ARTIFACT_TRACE"; then fail 'failed upload published draft'; fi
pass 'failed upload stops before draft publication'
: > "$ARTIFACT_TRACE"
FAKE_MACHINE=AArch64 reject ./scripts/publish --execute --skip-build roro-v0.1.0
if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'wrong architecture reached GitHub'; fi
pass 'wrong ELF architecture rejected before GitHub access'
cp "$archive" "$tmp/original.tar.gz"
cp "$archive.sha256" "$tmp/original.sha256"
source=dist/roro-0.1.0-source.tar.gz
cp "$source" "$tmp/source.tar.gz"
cp "$source.sha256" "$tmp/source.sha256"
for mutation in provenance source; do
    python3 - "$mutation" "$archive" "$source" <<'PYTHON'
import hashlib, io, json, pathlib, sys, tarfile
mutation, binary, source = sys.argv[1:]
def rewrite(path, update):
    with tarfile.open(path, 'r:gz') as archive:
        members = [(m, archive.extractfile(m).read() if m.isfile() else None) for m in archive.getmembers()]
    with tarfile.open(path, 'w:gz') as archive:
        for member, data in members:
            if data is not None:
                data = update(member.name, data)
                member.size = len(data)
            archive.addfile(member, io.BytesIO(data) if data is not None else None)
    p = pathlib.Path(path)
    p.with_name(p.name + '.sha256').write_text(hashlib.sha256(p.read_bytes()).hexdigest() + '  ' + p.name + '\n')
if mutation == 'source':
    rewrite(source, lambda name, data: b'not the tagged README\n' if name.endswith('/README.md') else data)
def metadata(name, data):
    if name.endswith('/release.json'):
        value = json.loads(data)
        if mutation == 'provenance': value['commit'] = '0' * 40
        else: value['sourceSha256'] = hashlib.sha256(pathlib.Path(source).read_bytes()).hexdigest()
        return json.dumps(value).encode()
    return data
rewrite(binary, metadata)
PYTHON
    : > "$ARTIFACT_TRACE"
    reject ./scripts/publish --execute --skip-build roro-v0.1.0
    if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'tampered artifact reached GitHub'; fi
    cp "$tmp/original.tar.gz" "$archive"
    cp "$tmp/original.sha256" "$archive.sha256"
    cp "$tmp/source.tar.gz" "$source"
    cp "$tmp/source.sha256" "$source.sha256"
done
pass 'tampered provenance and source rejected even with matching checksums'
printf 'corruption' >> "$archive"
: > "$ARTIFACT_TRACE"
reject ./scripts/publish --execute --skip-build roro-v0.1.0
if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'corrupt archive reached GitHub'; fi
pass 'checksum mismatch stops before GitHub access'
export FAKE_CARGO_FAIL=true
reject ./scripts/publish --execute roro-v0.1.0
unset FAKE_CARGO_FAIL
[[ ! -s $ARTIFACT_TRACE ]] || fail 'failed checks reached GitHub'
export FAKE_NIX_FAIL=true
reject ./scripts/publish --execute roro-v0.1.0
unset FAKE_NIX_FAIL
if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'failed build reached GitHub'; fi
pass 'failed local checks/build never uploads'
export FAKE_BUILT_VERSION=9.9.9
reject ./scripts/package roro-v0.1.0
unset FAKE_BUILT_VERSION
export FAKE_DYNAMIC=true
reject ./scripts/package roro-v0.1.0
unset FAKE_DYNAMIC
pass 'wrong binary version and dynamic linkage rejected'
export FAKE_INTERPRETER=true
reject ./scripts/package roro-v0.1.0
unset FAKE_INTERPRETER
export FAKE_NEEDED=true
reject ./scripts/package roro-v0.1.0
unset FAKE_NEEDED
pass 'ELF interpreter and shared-library dependencies rejected even if file reports static'
export FAKE_DIRTY_BUILD=true
reject ./scripts/package roro-v0.1.0
unset FAKE_DIRTY_BUILD
git restore README.md
pass 'source/lockfile changes during build abort packaging'
./scripts/package roro-v0.1.0 > "$tmp/output"
git --git-dir="$remote" update-ref -d refs/tags/roro-v0.1.0
: > "$ARTIFACT_TRACE"
reject ./scripts/publish --execute --skip-build roro-v0.1.0
if grep -q '^gh ' "$ARTIFACT_TRACE"; then fail 'unpushed tag reached GitHub'; fi
pass 'remote tag must match the exact annotated local tag'
printf 'dirty\n' >> README.md
reject ./scripts/package roro-v0.1.0
reject ./scripts/publish --execute --skip-build roro-v0.1.0
pass 'dirty checkout rejected before building or uploading'
printf '\n%s artifact checks passed (mock builds/GitHub; real local Git and archives).\n' "$count"
