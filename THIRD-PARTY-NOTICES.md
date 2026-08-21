# Third-party notices

This project is MIT-licensed (see [LICENSE](LICENSE)) and vendors one
external test suite. Full [REUSE](https://reuse.software/)-compliant
per-file license metadata lives in [`REUSE.toml`](REUSE.toml) and the
`.license` sidecars under `tests/corpus/relaxng/`; this file is a
human-readable summary. License texts referenced below are in
[`LICENSES/`](LICENSES).

## Vendored: RELAX NG validation test suite

- **Source:** [`relaxng/jing-trang`](https://github.com/relaxng/jing-trang),
  path `mod/rng-validate/test/spectest.xml`
- **Vendored revision:** `a6bc0041035988325dfbfe7823ef2c098fc56597`
- **License:** BSD-3-Clause, copyright 2001–2003 Thai Open Source Software
  Center Ltd (unmodified upstream `copying.txt`, vendored alongside it)
- **Location:** `tests/corpus/relaxng/spectest.xml`
- **Integrity:** SHA-256 pinned in `tests/corpus/relaxng/spectest.xml.sha256`
  and checked in CI; see `tests/corpus/relaxng/UPSTREAM.md` for the full
  provenance record and refresh procedure.

The vendored files are used unmodified, solely to drive this crate's own
tests against the official RELAX NG conformance suite — they are not
distributed as part of the published `relax-ng` crate.

## Cargo dependencies

Checked via `cargo deny check licenses` (configuration in
[`deny.toml`](deny.toml)) against an explicit, justified SPDX allow-list.
At the time of writing: `regex`, `regex-automata`, `regex-syntax`, and
`roxmltree` (MIT OR Apache-2.0); `aho-corasick` and `memchr` (Unlicense OR
MIT); `unicode-ident` ((MIT OR Apache-2.0) AND Unicode-3.0, for its
bundled Unicode character-property tables). None require attribution
beyond what `cargo metadata`/`Cargo.lock` already record; see `deny.toml`
for the allow-list and reasoning per license.
