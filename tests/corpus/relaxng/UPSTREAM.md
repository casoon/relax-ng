# RELAX NG validation testsuite

This directory vendors the unmodified `spectest.xml` test suite from
[`relaxng/jing-trang`](https://github.com/relaxng/jing-trang), path
`mod/rng-validate/test/spectest.xml`.

- Upstream revision: `a6bc0041035988325dfbfe7823ef2c098fc56597`
- Upstream path: `mod/rng-validate/test/spectest.xml`
- License: BSD-3-Clause; see the unmodified upstream `copying.txt` and
  `spectest.xml.license`
- Integrity: SHA-256 is recorded in `spectest.xml.sha256` and verified by
  `xtask/vendor-testsuite.sh`
- Refresh procedure: remove this directory deliberately, then run
  `xtask/vendor-testsuite.sh`; changing the pinned revision or checksum
  requires a corresponding decision entry.

The source is one XML manifest, not a directory-per-case corpus. It contains
385 `testCase` elements at the pinned revision. Each case declares a correct
or incorrect schema and may contain zero or more valid or invalid instance
documents plus referenced resources. Before the validator exists,
`xtask/spectest_inventory.py` assigns document-order IDs of the form
`jing-spectest-0001` and lists every case as `not implemented`.
