#!/usr/bin/env bash
set -euo pipefail

# Vendors the upstream RELAX NG validation test suite without modifying the
# original test data.
#
# Source: https://github.com/relaxng/jing-trang
# Revision: a6bc0041035988325dfbfe7823ef2c098fc56597
# License: BSD-3-Clause (upstream copying.txt)

readonly upstream_repository='https://github.com/relaxng/jing-trang.git'
readonly upstream_revision='a6bc0041035988325dfbfe7823ef2c098fc56597'
readonly upstream_test_path='mod/rng-validate/test/spectest.xml'
readonly destination='tests/corpus/relaxng'

if [[ -e "$destination/spectest.xml" ]]; then
  echo "refusing to overwrite $destination/spectest.xml" >&2
  exit 1
fi

temporary_directory=$(mktemp -d)
trap 'rm -rf "$temporary_directory"' EXIT

git clone --quiet "$upstream_repository" "$temporary_directory/jing-trang"
git -C "$temporary_directory/jing-trang" checkout --quiet --detach "$upstream_revision"

if [[ $(git -C "$temporary_directory/jing-trang" rev-parse HEAD) != "$upstream_revision" ]]; then
  echo 'checked out revision does not match the pinned upstream revision' >&2
  exit 1
fi

mkdir -p "$destination"
cp "$temporary_directory/jing-trang/$upstream_test_path" "$destination/spectest.xml"
cp "$temporary_directory/jing-trang/copying.txt" "$destination/copying.txt"
printf '%s  %s\n' \
  '3812289f941d4a1aa8ad0ab0f5e16f85f59d912eec710feb86b6b14a6e942a96' \
  'spectest.xml' > "$destination/spectest.xml.sha256"
cat > "$destination/spectest.xml.license" <<'EOF'
SPDX-FileCopyrightText: 2001-2003 Thai Open Source Software Center Ltd
SPDX-License-Identifier: BSD-3-Clause
EOF
cat > "$destination/copying.txt.license" <<'EOF'
SPDX-FileCopyrightText: 2001-2003 Thai Open Source Software Center Ltd
SPDX-License-Identifier: BSD-3-Clause
EOF

(
  cd "$destination"
  shasum -a 256 --check spectest.xml.sha256
)
