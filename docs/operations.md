# Operating Roro

For the minimal configuration and start command, see [the README](../README.md#run).
All image pulls are public; namespace ownership controls writes, not secrecy.

## Installing a release

For a packaged release, download the static Linux archive for your architecture
and its `.sha256` file from GitHub Releases, then verify before extracting:

```sh
sha256sum --check roro-<version>-linux-<arch>-static.tar.gz.sha256
tar -xzf roro-<version>-linux-<arch>-static.tar.gz
./roro-<version>-linux-<arch>-static/roro --version
```

The checksum detects corruption, not a compromised download source. Keep the
bundled license notices when redistributing. No Nix installation, container,
OpenSSL or glibc is needed on the target host.
`--help` and `--version` need no configuration. Terminate with SIGINT or SIGTERM
for graceful shutdown. JSON logs go to stdout; `RUST_LOG=roro=debug` adjusts
verbosity. Credentials and query strings are never logged.

## HTTPS

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

