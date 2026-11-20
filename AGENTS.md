# Registry development

- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked`.
- Use Conventional Commit summaries and maintain user-facing Unreleased entries in CHANGELOG.md.
- Use scripts/release for changelog-driven versions, local builds, annotated tags, and artifact uploads; preview by default. Do not manually bump Cargo versions or move published tags.
- Never delete lock files, steal locks by age, or publish partial content.
- Lock order: GC coordinator (GC only), publication gate, upload lock, repository lock, namespace binding lock.
- Metadata and canonical object publication must remain crash-durable; new mutable state needs explicit recovery tests.
- All pulls are public. Do not introduce private data under the assumption that repository names provide secrecy.
- Do not claim cross-host NFS or real GitLab CI validation from local tests.
- Active-transfer leases may surround the publication gate; GC only tries these leases nonblockingly and skips busy uploads, so it never waits in the reverse order.
- Every listener is TLS-only; default bind is 0.0.0.0:443. Never add a plaintext fallback for invalid/missing supplied credentials.
- Keep all Nix/release artifacts static and enforce scripts/check-static (no PT_INTERP or DT_NEEDED).
- Run scripts/check-release before packaging; scan every Git ref and publishable working file for secrets. Keep examples and fixtures synthetic.
- Upload only verified exact-tag source/static packages; published GitHub releases are immutable, draft retries only.
