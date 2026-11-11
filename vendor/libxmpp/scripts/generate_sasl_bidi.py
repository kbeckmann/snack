#!/usr/bin/env python3
"""Generate frozen SASLprep bidi tables from a local copy of RFC 3454.

Download https://www.rfc-editor.org/rfc/rfc3454.txt, then run this script with
that file's path. --check verifies the checked-in data without rewriting it.
"""

import argparse
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rfc", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    document = args.rfc.read_text(encoding="utf-8")
    output = [
        "// Generated from RFC 3454 Appendix D (Unicode 3.2); do not edit.",
        "// https://www.rfc-editor.org/rfc/rfc3454.txt",
        "// Regenerate with scripts/generate_sasl_bidi.py <local-rfc3454.txt>.",
        "",
    ]
    for name, number, expected in [("RANDAL", "D.1", 34), ("LCAT", "D.2", 360)]:
        start = f"----- Start Table {number} -----"
        end = f"----- End Table {number} -----"
        if document.count(start) != 1 or document.count(end) != 1:
            parser.error(f"missing or duplicate {number} table boundary")
        section = document.split(start, 1)[1].split(end, 1)[0]
        ranges = []
        for line in section.splitlines():
            match = re.fullmatch(r"\s*([0-9A-F]{4,6})(?:-([0-9A-F]{4,6}))?\s*", line)
            if match:
                lo = int(match[1], 16)
                hi = int(match[2] or match[1], 16)
                if lo > hi or hi > 0x10FFFF or (ranges and lo <= ranges[-1][1]):
                    parser.error(f"invalid range in {number}: {line!r}")
                ranges.append((lo, hi))
        if len(ranges) != expected:
            parser.error(f"expected {expected} ranges for {number}, got {len(ranges)}")
        output.append(f"pub(super) const {name}: &[(u32, u32)] = &[")
        output.extend(f"    (0x{lo:X}, 0x{hi:X})," for lo, hi in ranges)
        output.extend(["];", ""])
    target = Path(__file__).resolve().parents[1] / "src/stanza/saslprep/bidi.rs"
    rendered = "\n".join(output)
    if args.check:
        if target.read_text(encoding="utf-8") != rendered:
            parser.error(f"{target} differs from RFC tables")
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(rendered, encoding="utf-8")


if __name__ == "__main__":
    main()
