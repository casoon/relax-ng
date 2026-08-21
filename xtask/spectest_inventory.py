#!/usr/bin/env python3
"""List the vendored RELAX NG validation tests and their shape.

`tests/spectest.rs` exercises every one of these cases (schema
classification, and — for the ones with `<valid>`/`<invalid>` children —
instance validation too, including the `<resource>`/`<dir>` multi-file
cases via an in-memory resolver). This script is a suite-shape/integrity
overview independent of that test, mainly useful for `xtask/
vendor-testsuite.sh`'s CI check that the vendored file's case count
hasn't silently drifted.
"""

from __future__ import annotations

import argparse
import xml.etree.ElementTree as element_tree
from collections.abc import Iterable
from pathlib import Path


def child_elements(test_case: element_tree.Element) -> Iterable[element_tree.Element]:
    return (child for child in test_case if isinstance(child.tag, str))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "suite",
        nargs="?",
        type=Path,
        default=Path("tests/corpus/relaxng/spectest.xml"),
    )
    arguments = parser.parse_args()

    root = element_tree.parse(arguments.suite).getroot()
    test_cases = root.findall(".//testCase")

    for index, test_case in enumerate(test_cases, start=1):
        children = list(child_elements(test_case))
        section = next(
            (child.text.strip() for child in children if child.tag == "section" and child.text),
            "-",
        )
        schema = "incorrect" if any(child.tag == "incorrect" for child in children) else "correct"
        valid_instances = sum(child.tag == "valid" for child in children)
        invalid_instances = sum(child.tag == "invalid" for child in children)
        resources = sum(child.tag in {"resource", "dir"} for child in children)
        print(
            f"jing-spectest-{index:04}\tsection={section}\tschema={schema}"
            f"\tvalid={valid_instances}\tinvalid={invalid_instances}"
            f"\tresources={resources}"
        )

    print(f"{len(test_cases)} test cases in the vendored suite", flush=True)


if __name__ == "__main__":
    main()
