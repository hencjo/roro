# Security

All image pulls are anonymous. Do not store confidential images in Roro.
Treat the complete data directory as sensitive: it contains signing secrets and,
when generated TLS is used, the private server key. Never publish runtime data,
CI ID tokens, Docker credentials, private certificates, or debug packet captures.

For suspected vulnerabilities, use the repository's private GitHub vulnerability
reporting facility when enabled. If it is unavailable, contact the maintainer
privately before disclosing exploit details; do not put secrets in public issues.
Only the latest release receives fixes. Upgrade before reporting an issue and
include the version, redacted logs, filesystem/mount type, and reproduction.

The key in `tests/fixtures/ci-test-private.pem` is intentionally public and used
only by test builds. Production verification trusts the explicitly configured
GitLab issuer's signing keys, not this fixture. Never reuse the fixture key.

Shared-disk deployments assume trustworthy storage, working cross-host advisory
locks, and the same service UID across replicas. A compromised service UID or
storage server is outside the filesystem path-validation threat model. See
README.md and docs/acceptance.md for TLS, retention, backup, and NFS requirements.
