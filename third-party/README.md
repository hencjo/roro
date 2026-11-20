# Supplemental license notices

Some registry crate archives omit their upstream license files. These verbatim
notices were retrieved from the exact commits in their `.cargo_vcs_info.json`:

- `asn1-rs-impl-0.2.0/LICENSE-MIT`:
  https://github.com/rusticata/asn1-rs/blob/a20e5f7319c896737ad0f2557037817b91ad854f/LICENSE-MIT
- `valuable-0.1.1/LICENSE`:
  https://github.com/tokio-rs/valuable/blob/9efc29b6e58cef28f6566a47aa7e142a55fead77/LICENSE

`scripts/licenses` copies these only for the named versions. Other dependencies'
notices are copied from Cargo's checksum-verified source archives (including
`AUTHORS` and `COPYRIGHT`, where some projects put their full license text).
Review dependency upgrades for changed license obligations; this inventory is not
a claim that every resolved crate is linked into every target binary.
