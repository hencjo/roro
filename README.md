# roro

**Roll On Roll Off.**

A small Rust OCI/Docker registry: one binary, one shared directory, no database.

**All images are publicly pullable.** GitLab CI identities authorize publishing;
Maintainer-or-higher jobs can delete. There are no local users or stored CI secrets.

## Run

For a packaged release, download the static Linux archive for your architecture
and its `.sha256` file from GitHub Releases, then verify before extracting:

```sh
sha256sum --check roro-<version>-linux-<arch>-static.tar.gz.sha256
tar -xzf roro-<version>-linux-<arch>-static.tar.gz
./roro-<version>-linux-<arch>-static/roro --version
```

The checksum detects corruption, not a compromised download source. Keep the
bundled license notices when redistributing. No Nix installation, container,
OpenSSL or glibc is needed on the target host. Building from source:

```sh
devenv shell
nix build .#roro
cp config.example.yaml config.yaml
# Set dataRoot, publicUrl, and your GitLab HTTPS gitlabIssuer explicitly.
./result/bin/roro serve --config config.yaml
```

Rust 1.89+ on Linux is required. `--help` and `--version` need no configuration.
Terminate with SIGINT or SIGTERM for graceful shutdown. JSON logs go to stdout;
`RUST_LOG=roro=debug` adjusts verbosity. Credentials and query strings
are never logged.

## Mandatory HTTPS

Roro listens on **`0.0.0.0:443`** by default and never serves plaintext HTTP,
including health checks, metrics, and the token endpoint. TLS uses rustls; there
is no OpenSSL runtime dependency. Port 443 may require a service capability such
as systemd `AmbientCapabilities=CAP_NET_BIND_SERVICE`; do not run the service as
root just to bind a port. Use an explicit high-port `listen` value when needed,
and include that port in `publicUrl` if clients connect to it directly.

Provide your own PEM certificate chain and matching unencrypted private key:

```yaml
tlsCert: /etc/roro/fullchain.pem
tlsKey: /etc/roro/private-key.pem
```

Both fields must be supplied together. Invalid, missing, or mismatched supplied
credentials fail startup; Roro never falls back to plaintext or silently
substitutes another certificate. Restart instances after replacing supplied
certificates (live certificate reloading/ACME is not implemented).

Omit both fields to generate a **self-signed** server certificate for the hostname
or IP in `publicUrl`, valid for one year. Its private key and certificate are
persisted atomically as `dataRoot/gc/tls-identity.json` with mode 0600, under a
cross-instance file lock. Replicas sharing storage reuse exactly the same identity.
The public certificate is exported to `dataRoot/gc/tls-cert.pem`; install that
certificate in Docker/client trust stores before sending credentials. For Docker,
copy only the public certificate to `/etc/docker/certs.d/<registry-host[:port]>/ca.crt`
on the client host. Never distribute the identity JSON or disable TLS verification
as the production solution. A self-signed certificate is not publicly trusted.

Roro does not silently rotate this identity, which would break installed client
trust. Renew before expiry using a CA-signed pair, or stop all replicas, back up
storage, explicitly remove the generated identity/public-certificate files, restart,
and install the new public certificate on clients. Expired or corrupt stored
identities fail startup rather than being silently replaced. Supplied certificate
lifecycle management remains the operator's responsibility.

A reverse proxy is optional, but the upstream is **also HTTPS**: validate Roro's
certificate against its issuer/public certificate and set SNI to the `publicUrl`
hostname. Forward Authorization unchanged, allow all registry HTTP methods, and
avoid upload buffering/short timeouts. There is no plaintext backend mode.
The token service uses the configured public origin, not untrusted Host headers.

`/livez`, `/readyz`, and Prometheus-format `/metrics` are available over HTTPS.
Restrict monitoring paths at your proxy if they must not be public. Readiness
checks shared storage. Startup requires reachable GitLab OIDC discovery; cached
keys permit subsequent authentication during short outages.

## Publish from GitLab CI

```yaml
publish:
  image: docker:cli
  services:
    - docker:dind
  id_tokens:
    REGISTRY_TOKEN:
      aud: https://registry.example.com
  script:
    - echo "$REGISTRY_TOKEN" | docker login registry.example.com -u gitlab-ci-token --password-stdin
    - docker build -t "registry.example.com/$CI_PROJECT_PATH:$CI_COMMIT_SHA" .
    - docker push "registry.example.com/$CI_PROJECT_PATH:$CI_COMMIT_SHA"
```

Runner/Docker service configuration remains your existing CI setup. This is not
a runner registration token, `CI_JOB_TOKEN`, access token, or user password: the
password is an audience-bound **CI OIDC ID token**. The username is ignored.
Do not print the token or enable shell tracing around credentials.

Project `team/app` may publish `team/app:tag` and `team/app/worker:tag`, but not
`team/application` or another project's path. The registry checks GitLab-signed
`job_project_id`, `job_project_path`, source-project identity, and user access level.
Developer (30) or higher may publish. Maintainer (40) or higher may delete.
Merge-request/external-pull-request pipelines are denied, even when the triggering
user has higher privileges. Protected refs are **not** required in v1.

All GET/HEAD image reads and tag lists are anonymous. The `/v2/` discovery endpoint
uses the standard Bearer challenge; anonymous clients receive pull-only tokens.
Registry tokens last at most five minutes and never outlive their CI ID token.
An ID token can remain usable after its job finishes, until its signed expiry.
GitLab API credentials and OAuth applications are not required. Supported GitLab
versions must provide the job-project claims (18.4+). See [GitLab CI identity documentation](https://docs.gitlab.com/ci/secrets/id_token_authentication/).

## Retention — read before storing production images

An elected instance runs cleanup at startup and hourly:

- Keep the tag `latest` indefinitely.
- Expire every other tag **30 days after its last successful manifest PUT**.
- Keep unreachable content for at least seven continuously observed days.
- Remove upload sessions idle for 24 hours, skipping active locks.

This also expires version/release tags. No tag except `latest` is exempt. Reading
an image does not refresh retention. Re-pushing a tag does. Deleting by tag removes
that tag; deleting by digest removes its manifest and associated tags. A manifest
referenced by another stored manifest cannot be explicitly deleted until that
reference is removed. GC traverses retained image indexes and child manifests.

Cleanup never requires stopping serving instances. It pauses metadata commits and
new content opens behind a filesystem gate while marking/sweeping; already open
downloads continue. Large registries can see lock waits or retryable 503 responses.
This favors simple correctness over zero-pause collection. Untagged manifests act
as temporary roots during their grace period. Corrupt/incomplete graphs abort
sweeping rather than guessing what to delete.

## Shared-disk deployment

Run multiple instances with the **same dataRoot, publicUrl, GitLab issuer, Unix
UID, and storage format**. Each can listen on a different address and sit behind
a load balancer without sticky sessions. Their generated signing secret and
project/path bindings live on the shared disk. Do not mix incompatible versions.

Use a local Linux filesystem or a properly configured **NFSv4 hard mount** with
cross-client advisory locking. `soft`, `softerr`, `softreval`, `nolock`, and
`local_lock=all/flock` are rejected when detected. FUSE, SMB/CIFS, and 9p are not
supported. All storage subdirectories must stay on the same filesystem. The
startup lock probe checks only this client; it does **not** certify cross-host
semantics. Follow [the acceptance checklist](docs/acceptance.md) on your real share.

Never remove lock files, replace their directory, or steal locks based on age.
Transient upload, active-transfer, and staging locks use separate fixed hash buckets (4096 per lock class), so
lock files do not accumulate per request. Bucket collisions only serialize work.
Other locks remain persistent. File descriptors release locks after process death.
Storage-server lease recovery and fencing during partitions are infrastructure
responsibilities; stop unhealthy writers instead of forcing a lock takeover.

Release packaging/uploads (including reused archives), CI, and Nix installation inspect ELF headers and reject any
dynamic loader (`PT_INTERP`) or shared-library dependency (`DT_NEEDED`). Native
Cargo development builds are not release artifacts; use the musl target or Nix.

Only registry processes may write the dataRoot. Symlinks in storage are rejected;
this is not a defense against a hostile process running as the service's own UID.
The service checks digests before publication, uses immutable canonical content,
and acknowledges upload checkpoints only after data/metadata synchronization.
A retry truncates unacknowledged upload tails. Disk-full, lock, or I/O errors must
be treated as failed requests, not successful pushes.

### Backup and migration

Back up the **entire dataRoot**, including `gc/` (signing secret, format marker,
GitLab ID/path bindings, reachability checkpoints). Use a storage snapshot or stop
all instances for a consistent backup. Never copy just blobs and tags while writes
are running. Keep private signing material readable only by the service account.

Project-path reuse by a different GitLab project ID is denied. Renames, transfers,
registry-origin changes, and GitLab-issuer changes require stopping all instances,
a consistent backup, and an explicit metadata migration; there is no automatic
adoption or built-in migration command in v1. Do not delete binding files to
silently give a new project somebody else's images.

## Development and compatibility

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
# Static Linux binary (musl toolchain/linker required):
cargo build --release --locked --target x86_64-unknown-linux-musl
./scripts/check-static target/x86_64-unknown-linux-musl/release/roro
# All exported Nix packages (default, roro, static) are static musl builds:
nix build .#static
```

Implements Distribution `/v2/` image pull/push, SHA-256 blobs, resumable uploads,
blob mounts, ranges, tag pagination, OCI/Docker manifests and indexes, and manifest
deletion. Incoming request bodies are streamed to staging files (100 GiB maximum
per upload request, 60-second idle timeout); manifest bodies are limited to 10 MiB.
Staging and immutable publication temporarily require additional disk space.
Only canonical lowercase SHA-256 digests are accepted. Schema-1 images, catalog
listing, private pulls, artifact-referrer discovery, replication, quotas, scanning,
and GitLab's deprecated external-registry integration are not implemented.
No full OCI certification or Docker/BuildKit acceptance claim is made without the
external checks in [docs/acceptance.md](docs/acceptance.md).

`tests/fixtures/ci-test-private.pem` is deliberately public, test-only key material;
it is never accepted by the production GitLab verifier.

## Releases

Release locally with `./scripts/release` (preview) or `./scripts/release --execute`.
Versions come from the hand-written Unreleased changelog; the script stamps,
commits, tags, builds static artifacts locally, pushes the release refs atomically,
and uploads static binary and source archives with SHA-256 checksums using `gh`.
Packages include dependency notices and commit/source provenance; uploaded ELF
files must have neither an interpreter nor shared-library dependencies.
CI does not publish releases. Published assets and tags are never overwritten.
See [CONTRIBUTING.md](CONTRIBUTING.md) for prerequisites and safe recovery.

## License and security

Roro is MIT-licensed; dependencies retain their own licenses. See [LICENSE](LICENSE),
[SECURITY.md](SECURITY.md), and the [contribution guide](CONTRIBUTING.md).
All pulls are public and retention is enabled automatically—read the retention
section before storing images. Proxy caching and rebuild retention are not implemented.
