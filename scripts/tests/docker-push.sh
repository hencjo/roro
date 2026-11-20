#!/usr/bin/env bash
# Disposable test fixture and rootless daemon only. Never uses the host Docker socket.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${DOCKER_BIN_DIR:?Set the directory containing docker, dockerd, containerd, and runc test binaries}"
store=${RORO_DOCKER_STORE:-classic}
transport=${RORO_DOCKER_TRANSPORT:-http1}
layer_mib=${RORO_DOCKER_LAYER_MIB:-64}
[[ $store == classic || $store == containerd ]] || { echo 'invalid store' >&2; exit 1; }
[[ $transport == http1 || $transport == http2 ]] || { echo 'invalid transport' >&2; exit 1; }
[[ $layer_mib =~ ^[1-9][0-9]*$ && $layer_mib -le 512 ]] || { echo 'layer MiB must be 1..512' >&2; exit 1; }
export PATH="$DOCKER_BIN_DIR:$PATH"
for tool in cargo python3 rootlesskit docker dockerd; do command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }; done
work=$(mktemp -d /tmp/roro-docker-acceptance.XXXXXX)
fixture_pid=''; daemon_pid=''
cleanup() {
    if [[ -f $work/rootlesskit/child_pid ]]; then kill -TERM "$(cat "$work/rootlesskit/child_pid")" 2>/dev/null || true; fi
    if [[ -n $daemon_pid ]]; then wait "$daemon_pid" 2>/dev/null || true; fi
    if [[ -n $fixture_pid ]]; then kill "$fixture_pid" 2>/dev/null || true; wait "$fixture_pid" 2>/dev/null || true; fi
    # Keep logs and image data for diagnosis; they contain only public fixture credentials.
    printf 'Acceptance workspace: %s\n' "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$work/registry" "$work/run" "$work/home" "$work/client" "$work/anonymous" "$work/build"
cd "$root"
cargo test --locked --lib --no-run --message-format=json > "$work/build.json"
exe=$(python3 -c 'import json,sys; print([x["executable"] for l in open(sys.argv[1]) if (x:=json.loads(l)).get("executable") and x.get("target",{}).get("kind")==["lib"]][-1])' "$work/build.json")
bridge=(); if [[ $transport == http2 ]]; then bridge=(RORO_DOCKER_HTTP2_BRIDGE=1); fi
env "${bridge[@]}" RORO_DOCKER_FIXTURE_DIR="$work/registry" "$exe" --ignored --exact http::protocol_tests::docker_acceptance_fixture --nocapture > "$work/registry.log" 2>&1 &
fixture_pid=$!
for _ in {1..100}; do [[ -f $work/registry/fixture.json ]] && break; kill -0 "$fixture_pid" 2>/dev/null || { cat "$work/registry.log"; exit 1; }; sleep 0.1; done
[[ -f $work/registry/fixture.json ]] || { echo 'fixture startup timeout' >&2; exit 1; }
readarray -t metadata < <(python3 -c 'import json,sys;v=json.load(open(sys.argv[1])); print(v["origin"]);print(v["certificate"])' "$work/registry/fixture.json")
origin=${metadata[0]}; cert=${metadata[1]}; host=${origin#https://}
feature=false; driver=vfs
if [[ $store == containerd ]]; then feature=true; driver=native; fi
printf '{"features":{"containerd-snapshotter":%s}}\n' "$feature" > "$work/daemon.json"
# Host networking avoids firewall changes; copied-up /etc and /run isolate daemon state.
# Disable Go h2 on the frontend explicitly; the optional bridge asserts h2 to Roro.
env HOME="$work/home" XDG_RUNTIME_DIR="$work/run" SSL_CERT_FILE="$cert" GODEBUG=http2client=0 \
    rootlesskit --net=host --copy-up=/etc --copy-up=/run --pidns --state-dir="$work/rootlesskit" \
    dockerd --rootless --storage-driver="$driver" --bridge=none --iptables=false --ip-forward=false --ip-masq=false \
    --config-file "$work/daemon.json" --data-root "$work/docker-data" --exec-root "$work/docker-exec" \
    --pidfile "$work/daemon.pid" --host "unix://$work/docker.sock" > "$work/daemon.log" 2>&1 &
daemon_pid=$!
export DOCKER_HOST="unix://$work/docker.sock" DOCKER_CONFIG="$work/client"
unset DOCKER_CONTEXT DOCKER_TLS_VERIFY DOCKER_CERT_PATH
for _ in {1..150}; do docker info > "$work/info" 2>/dev/null && break; kill -0 "$daemon_pid" 2>/dev/null || { tail -40 "$work/daemon.log"; exit 1; }; sleep 0.2; done
docker info --format '{{.Driver}} {{json .DriverStatus}}' | tee "$work/store"
if [[ $store == classic ]]; then grep -q '^vfs ' "$work/store"; else grep -q 'io.containerd.snapshotter.v1' "$work/store"; fi
# No shell tracing, no credential command-line arguments, no real GitLab credentials.
python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["token"])' "$work/registry/fixture.json" | docker login "$host" -u gitlab-ci-token --password-stdin
image="$host/team/project/acceptance:$(date +%s)-$store-$transport"
python3 "$root/scripts/tests/docker-image.py" "$work/build" "$layer_mib" "$image" "$store"
if [[ $store == classic ]]; then DOCKER_BUILDKIT=0 docker build -t "$image" "$work/build"; else docker load -i "$work/build/image.tar"; fi
docker push "$image"
DOCKER_CONFIG="$work/anonymous" docker pull "$image"
python3 - "$work/registry.log" "$transport" "$store" "${RORO_DOCKER_REQUIRE_HEADERLESS:-0}" <<'PY'
import json,sys
entries=[json.loads(l)['fields'] for l in open(sys.argv[1]) if l.startswith('{')]
patches=[e for e in entries if e.get('method')=='PATCH']
assert patches or sys.argv[3]=='containerd','classic test did not exercise PATCH'
if sys.argv[4]=='1':
    assert any(not e['has_content_type'] and not e['has_content_length'] and not e['has_content_range'] for e in patches),'no legacy headerless PATCH observed'
commits=[e for e in entries if e.get('method')=='PUT' and '/blobs/uploads/' in e.get('path','')]
assert len(commits)>=6,'expected five layers plus config commits'
assert all(e['status']==201 for e in commits),commits
assert all(e['status']==202 for e in patches),patches
expected='HTTP/2.0' if sys.argv[2]=='http2' else 'HTTP/1.1'
assert all(e['http_version']==expected for e in patches+commits),patches+commits
assert all('?' not in e['path'] for e in entries if 'path' in e)
print(f'Passed: real Docker push/pull, {expected}, {len(patches)} PATCHes, {len(commits)} commits; PATCH content-type presence: {sorted(set(e["has_content_type"] for e in patches))}')
PY
