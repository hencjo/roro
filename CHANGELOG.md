# Changelog

## [Unreleased]

### Breaking changes

- Require an explicit `gitlabIssuer`; no company-specific identity provider is
  selected implicitly. Existing configurations with an explicit issuer are unchanged.

### Added

- Static Linux release archives with exact-tag source archives, SHA-256 checksums,
  embedded commit/target metadata and bundled dependency license notices.
- Jix-style preview-first build/upload recovery commands and GitHub CI secret checks.

### Changed

- Prepare public-source documentation and synthetic GitLab job fixtures without
  deployment hostnames or real-job identifiers.
- Harden local GitHub releases with source/provenance checks, immutable published
  assets, static artifact verification, and a secret-scanning gate.

### Fixed

### Security

### Documentation

- Lead with automatic GitLab project namespaces and role-based write access;
  simplify startup/TLS examples, move developer workflows to CONTRIBUTING and
  preserve detailed deployment guidance in an operations guide.
- Explain Roro's GitLab CI workflow and low-overhead deployment model up front,
  with prominent public-pull and automatic tag-expiry defaults.
- Diagram multiple TLS-only instances sharing one data directory, including
  load balancing, filesystem-locking requirements and shared-storage limitations.

### Upgrade notes

## [0.2.1] - 2026-10-22

### Breaking changes

### Added

### Changed

### Fixed

- Accept streamed blob PATCHes without Content-Type from legacy Docker pushers,
  while continuing to reject explicitly unsupported types and invalid paths.
- Include method, query-free path, HTTP version, and safe error code/message in
  request completion logs; retain errors wrapped by authentication challenges.
- Exercise real classic Docker pushes of five 64-MiB layers over TLS HTTP/1.1
  and an explicitly verified HTTP/2 backend, with direct h1/h2 socket regressions.

### Security

### Documentation

### Upgrade notes

## [0.2.0] - 2026-10-22

### Breaking changes

- Require HTTPS for every endpoint and default to `0.0.0.0:443` instead of
  loopback HTTP. Update probes/proxies to HTTPS and explicitly trust generated
  certificates, or configure a CA-signed `tlsCert`/`tlsKey` pair.

### Added

- Introduce Roro (Roll On Roll Off): a Rust OCI/Docker registry with
  anonymous pulls and GitLab CI OIDC-authenticated, project-scoped publishing.
- Add shared-disk atomic publication, resumable uploads, filesystem locking,
  online garbage collection, health checks, and metrics.

### Changed

- Rename the app, binary, Nix package, and release artifacts to `roro`.
- Accept a supplied PEM certificate/key pair or persist a generated self-signed
  certificate without introducing an OpenSSL runtime dependency.
- Enforce static musl binaries for all Nix packages and inspect ELF linkage in
  Nix, release packaging, and CI.

- Support `scripts/release --local` for versioned static artifacts without a
  configured remote, fetching, pushing, or uploading.

- Use maintainer-driven local releases: infer versions from this changelog,
  build and verify static Linux artifacts locally, then push annotated tags and
  upload archives/checksums to GitHub. No release PR automation or CI publisher.

### Fixed

- Accept GitLab CI role-name access levels (including `owner`) while retaining
  numeric compatibility; keep unused GitLab claims permissive.
- Log safe authentication refusal reasons without exposing tokens or signatures.

### Upgrade notes

- Update service commands and log filters to `roro`; keep the existing `dataRoot`
  when upgrading so stored images, signing keys, and TLS identity remain in use.

- All images are public. Retention preserves `latest`, expires other tags after
  30 days, and reclaims unreachable content after a seven-day grace period.
