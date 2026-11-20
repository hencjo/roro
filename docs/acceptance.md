# Deployment acceptance

Local automated tests are not evidence of cross-host NFS behavior or a real CI
identity. Complete these checks before production images are admitted; retention
is automatic from startup, not gated behind a separate enable switch.

## 1. GitLab CI identity and real clients

Check your configured GitLab issuer's discovery and keys:

- `https://gitlab.example.com/.well-known/openid-configuration`
- `https://gitlab.example.com/oauth/discovery/keys`

Use GitLab 18.4 or later and verify that a real job issues `job_project_id`,
`job_project_path`, `project_id`, `project_path`, `user_access_level`, and
`pipeline_source`. The configured issuer must match the token exactly.
`user_access_level` is a lowercase role name (`minimal_access`, `guest`,
`planner`, `reporter`, `developer`, `maintainer`, or `owner`); unsigned numbers
and numeric strings are also accepted. Developer or higher may push. Unknown
names and role names with different case are rejected. Unused claims, including
string-valued `ref_protected`, are ignored.

Authentication refusals log a safe reason at INFO (for example `unknown_kid`,
`invalid_claims_json`, `invalid_audience`, or `insufficient_access_level`).
Never log ID tokens, Basic credentials, or signatures, and do not send
credentials to an external JWT decoder.

Deploy the registry at its real HTTPS origin, with either a supplied trusted
certificate/key or explicitly installed trust for its generated public certificate.
Plain HTTP to the same listener must not produce an HTTP response. Default bind
is 0.0.0.0:443; use a high port for unprivileged smoke tests. In a non-merge-request GitLab
job with `REGISTRY_TOKEN` configured via `id_tokens`, run:

```sh
REGISTRY_ORIGIN=https://registry.example.com sh scripts/ci-smoke.sh
```

The script builds a tiny scratch image, uses the ephemeral credential to push a
unique tag under `CI_PROJECT_PATH`, pulls with an empty Docker credential config,
and checks a cross-project-path push is rejected. It leaves the smoke image for
automatic retention. Keep CI logs free of credentials.

Also push an image with an incompressible layer of at least 320 MiB, using both
Docker schema 2 and OCI manifests where the runner supports them. Verify its
anonymous pull by digest. The local protocol regression uses a signed test key
and streamed bytes; it is not proof of real GitLab/Docker acceptance.

Registry tokens last at most five minutes; token exchange requires at least
60 seconds remaining on the GitLab ID token. A continuous upload is authorized
at request start, but later requests need a fresh registry token after expiry.
Uploads have a 60-second **idle** timeout and a 100-GiB per-request limit;
manifests are limited to 10 MiB. Disk staging/finalization can temporarily need
approximately three copies of an uploaded layer, plus existing content and
other concurrent uploads. Slow storage can also trigger a 30-second lock wait.

Additionally use `docker buildx build --platform linux/amd64,linux/arm64 --push`
with the usual attestation defaults; pull each platform and pull by index digest.
Check simultaneous uploads through two different backend instances and kill one
mid-upload; resume its returned upload URL through the other backend.

## Legacy Docker streaming regression

From a source checkout, with Rust, Python 3.11+, RootlessKit, configured subordinate
UID/GID mappings (`newuidmap`/`newgidmap`), and disposable Docker test binaries:

```sh
DOCKER_BIN_DIR=/path/to/docker-24.0.5 RORO_DOCKER_REQUIRE_HEADERLESS=1 \
  RORO_DOCKER_TRANSPORT=http1 ./scripts/tests/docker-push.sh
DOCKER_BIN_DIR=/path/to/docker-24.0.5 RORO_DOCKER_REQUIRE_HEADERLESS=1 \
  RORO_DOCKER_TRANSPORT=http2 ./scripts/tests/docker-push.sh
# Optional modern containerd-store pusher (Docker 29+):
DOCKER_BIN_DIR=/path/to/docker-29 RORO_DOCKER_STORE=containerd \
  RORO_DOCKER_TRANSPORT=http1 ./scripts/tests/docker-push.sh
```

The harness starts its own rootless daemon with private data/config/run paths,
no bridge or firewall changes, and a TLS registry on random loopback ports.
It never connects to an existing Docker daemon. Classic runs explicitly disable
containerd snapshots and use `DOCKER_BUILDKIT=0`; modern runs import a five-layer
Docker archive into the containerd store before pushing. Five independent 64-MiB
random layers are used by default (`RORO_DOCKER_LAYER_MIB` can reduce local tests).
Certificate verification stays enabled. The registry uses only the existing
public test signing key, compiled out of production binaries. No live CI token
is needed, and these tests do not prove real GitLab identity acceptance.

Classic Docker's custom Go transport uses HTTP/1.1 even when Roro offers h2.
`http2` therefore uses a test-only TLS bridge which preserves the actual Docker
pusher's streaming requests and asserts HTTP/2 on its connection to Roro. A
separate socket test directly exercises both protocols, missing and present
Content-Type, absent Content-Length/Content-Range, and a bodyless PUT commit:

```sh
cargo test --locked --lib \
  http::protocol_tests::legacy_streamed_patch_over_tls_http1_and_http2 \
  -- --ignored --exact
```

Reproduced before the compatibility fix with Docker 24.0.5: all five layer
PATCHes returned 400 in 0 ms with `BLOB_UPLOAD_INVALID` / `PATCH requires
application/octet-stream`. Those requests omitted **Content-Type as well as
Content-Length and Content-Range**. Docker 29's classic pusher supplies the type
and did not reproduce the rejection. See the [upstream client bug](https://github.com/distribution/distribution/issues/3965).
Only the missing Content-Type check is relaxed; path validation, explicit media
validation, streaming size/idle limits, offset checks, and digest verification
remain enforced. Percent-encoded digest query values are accepted unchanged.

Completion events now include method, query-free path, HTTP version, and
code/message on 4xx/5xx, including wrapped authentication failures. Header
presence is logged as booleans, never raw Authorization or query values.
The harness asserts actual upload status/protocol from those logs, performs an
anonymous pull, and stops only its own test services. It prints and retains its
disposable workspace for diagnosis; remove that directory once no longer needed.
CI runs both legacy transport cases without publishing production images.

Local verification on 2026-10-22 (test signing key, not a real GitLab job):

| Daemon/store | Direct TLS h1 | Verified TLS h2 backend | Upload shape |
|---|---|---|---|
| Docker 24.0.5, classic vfs | Passed | Passed | Six headerless PATCHes and bodyless commits |
| Docker 29.8.1, containerd native | Passed | Passed | Six monolithic PUT commits |

Each case used five independently generated 64-MiB random filesystem layers plus
image config. The h2 rows use the documented bridge, not native Docker h2.

## 2. OCI tests

Use the [official conformance suite](https://github.com/opencontainers/distribution-spec/tree/v1.1.1/conformance)
against a disposable project namespace with a suitable GitLab ID token. Run the
Pull and Push categories, and the implemented tag-list/deletion cases individually.
The suite must not be used to claim unsupported content-discovery/management
categories or referrers support. CI credentials expire: regenerate them as needed
rather than weakening expiry validation for tests. Never run destructive tests
against production namespaces.

## 3. Actual shared filesystem

First exercise the normal storage suite on the mounted filesystem on each host:

```sh
DISK_REGISTRY_TEST_ROOT=/mnt/registry-test cargo test --locked --test storage
```

Use a **new empty dedicated directory** on the shared filesystem for each two-host
probe. Start the holder on host A, wait until `probe-ready` appears, then start the
contender on host B:

```sh
# Host A
DR_PROBE_ROOT=/mnt/registry-test/probe-unique \
  cargo test --locked --test storage cross_host_holder -- --ignored --exact
# Host B, same directory as seen from its independent NFS client
DR_PROBE_ROOT=/mnt/registry-test/probe-unique \
  cargo test --locked --test storage cross_host_contender -- --ignored --exact
```

The contender must fail to acquire the held lock, then acquire it after release.
Repeat with host A's test process forcibly terminated. After that, verify
cross-client metadata visibility, upload resume, concurrent duplicate blob
publication, and atomic tag replacement through two running registry instances.
Exercise storage-server restart/lease recovery with the infrastructure team; do
not simulate recovery by deleting locks. Record the NFS server, Linux kernels,
mount options, and results.

## 4. Failure and operational checks

On disposable storage, inject ENOSPC and filesystem I/O failure during staging,
checkpoint replacement, CAS publication, and repository metadata replacement.
No failed operation may be acknowledged successfully. After recovery, verify all
reachable bytes against their digest and retry uploads at the acknowledged offset.

Create old tags and shared-layer/multi-platform graphs in an isolated test setup.
Verify `latest` survives, stale tags expire, and only unreachable content is swept
after grace. Kill/restart the GC owner during sweeping; rerun cleanup and verify
reachable content. Test slow active downloads during collection and monitor lock
waits/503s at the anticipated repository count before accepting the global GC gate.

## Status

The repository contains automated protocol, JWT/security, concurrency, GC, upload
recovery, and subprocess lock-release tests. Actual GitLab CI issuance, real
Docker/BuildKit/OCI suite execution, cross-host NFS, and storage-level failure
injection require the environments above; they are not implied by `cargo test`.

## Workspace verification performed

- Formatting, strict Clippy, and the local automated suite passed; the TLS socket
  test was run explicitly with permission to open local sockets. Cross-host
  probes remain environment-dependent.
- The optimized Linux musl executable built and ran; `file` reported a stripped,
  static-PIE executable with no dynamic loader or shared-library dependencies.
- Local test fixtures exercise shared-disk TLS identities, token exchange, and
  anonymous/authenticated registry operations. These fixtures use deliberately
  public test keys; they do not validate any deployment's GitLab identity.

These checks do not replace the environment-dependent tests above.

## TLS and static-link checks

```sh
cargo test --locked --lib tls::tests::https_works_and_plaintext_is_rejected -- --ignored --exact
./scripts/check-static target/x86_64-unknown-linux-musl/release/roro
```

The socket test uses a generated certificate, explicitly trusts its public part,
checks HTTPS, and rejects plaintext on that same listener. Release ELF checks
reject both a dynamic interpreter and any shared-library dependencies.
