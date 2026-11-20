#!/bin/sh
set -eu
# Never enable shell tracing: REGISTRY_TOKEN is a credential.
: "${REGISTRY_ORIGIN:?Set the registry HTTPS origin}"
: "${CI_PROJECT_PATH:?Run this inside a GitLab project job}"
: "${REGISTRY_TOKEN:?Configure an audience-bound GitLab CI id_tokens entry}"
case "$REGISTRY_ORIGIN" in https://*) ;; *) echo 'REGISTRY_ORIGIN must use HTTPS' >&2; exit 1;; esac
host=${REGISTRY_ORIGIN#https://}
case "$host" in */*|*\?*|*\#*|*@*) echo 'REGISTRY_ORIGIN must be an origin without a trailing slash' >&2; exit 1;; esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
mkdir "$work/auth" "$work/anonymous" "$work/build"
printf 'registry smoke test\n' > "$work/build/hello"
printf 'FROM scratch\nCOPY hello /hello\n' > "$work/build/Dockerfile"
tag="smoke-${CI_JOB_ID:-$(date +%s)}"
image="$host/$CI_PROJECT_PATH:$tag"
echo "$REGISTRY_TOKEN" | DOCKER_CONFIG="$work/auth" docker login "$host" -u gitlab-ci-token --password-stdin
DOCKER_CONFIG="$work/auth" docker build -t "$image" "$work/build"
DOCKER_CONFIG="$work/auth" docker push "$image"
DOCKER_CONFIG="$work/anonymous" docker pull "$image"
denied="$host/${CI_PROJECT_PATH}-denied:$tag"
docker tag "$image" "$denied"
if DOCKER_CONFIG="$work/auth" docker push "$denied" > "$work/denied.log" 2>&1; then
    echo 'FAIL: cross-project push unexpectedly succeeded' >&2
    exit 1
fi
printf 'Passed: authenticated push, anonymous pull, cross-project denial (%s)\n' "$image"
