# SPDX-License-Identifier: AGPL-3.0-or-later
"""Mutations for measuring how strong a conformance test is.

    mutate.py list  SRC                 list every mutation, one per line
    mutate.py apply SRC INDEX OUT       write SRC with mutation INDEX to OUT
    mutate.py test                      self-test

Each mutation is one small, typical driver mistake at one place in the
source: a comparison boundary moved, an equality or logical operator
flipped, a `+ 1` or `- 1` dropped (off by one), a register write removed,
or a wait shortened to nothing. String literals, comments, and everything
from `#[cfg(test)]` on are left alone, so a mutation changes behaviour, not
a message. Some mutants will not compile; harness/mutants.sh counts those
separately.

Listing is deterministic: the same source gives the same numbered list.
"""

import re
import sys

# (name, regex on the masked line, replacement). Matches must not touch
# string contents (masked) or comments (cut).
OPERATORS = [
    ("boundary <= to <", re.compile(r" <= "), " < "),
    ("boundary < to <=", re.compile(r" < "), " <= "),
    ("boundary >= to >", re.compile(r" >= "), " > "),
    ("boundary > to >=", re.compile(r" > "), " >= "),
    ("== to !=", re.compile(r" == "), " != "),
    ("!= to ==", re.compile(r" != "), " == "),
    ("&& to ||", re.compile(r" && "), " || "),
    ("|| to &&", re.compile(r" \|\| "), " && "),
    ("drop + 1", re.compile(r" \+ 1\b"), ""),
    ("drop - 1", re.compile(r" - 1\b"), ""),
    ("no wait", re.compile(r"sleep_us\((?!0\))[^()]*\)"), "sleep_us(0)"),
]
# A whole statement that writes a device register: removed.
REGISTER_WRITE = re.compile(
    r"^\s*(?:mmio::)?(?:w8|w16|w32|write8|write16|write32|write)\(.*\)\??;\s*$"
)


def mask(line):
    """The line with string and char literal contents replaced by '_', and
    any // comment cut, so positions still line up."""
    out = []
    i, n = 0, len(line)
    quote = None
    while i < n:
        c = line[i]
        if quote:
            if c == "\\" and i + 1 < n:
                out.append("__")
                i += 2
                continue
            if c == quote:
                quote = None
                out.append(c)
            else:
                out.append("_")
            i += 1
            continue
        if c == "/" and line.startswith("//", i):
            break
        if c == '"':
            quote = '"'
        elif c == "'" and i + 2 < n and (line[i + 2] == "'" or line[i + 1] == "\\"):
            # A char literal, not a lifetime.
            quote = "'"
        out.append(c)
        i += 1
    return "".join(out)


def mutations(src):
    """Every mutation: (line number, operator name, original, mutated)."""
    found = []
    for no, line in enumerate(src.split("\n"), 1):
        if line.strip().startswith("#[cfg(test)]"):
            break
        stripped = line.lstrip()
        if stripped.startswith("//") or stripped.startswith("///"):
            continue
        masked = mask(line)
        if REGISTER_WRITE.match(masked) and "fn " not in masked:
            indent = line[: len(line) - len(stripped)]
            found.append((no, "remove register write", line, indent + "// mutant: register write removed"))
        for name, rx, repl in OPERATORS:
            for m in rx.finditer(masked):
                mutated = line[: m.start()] + repl + line[m.end():]
                found.append((no, name, line, mutated))
    return found


def apply(src, index):
    lines = src.split("\n")
    no, _, _, mutated = mutations(src)[index]
    lines[no - 1] = mutated
    return "\n".join(lines)


def selftest():
    assert mask('let s = "a < b"; // x < y') == 'let s = "_____"; '
    assert mask("let c = '<'; a < b") == "let c = '_'; a < b"
    src = "\n".join([
        "fn f(a: u32, b: u32) -> bool {",
        '    log("a < b && c");',
        "    // a < b",
        "    write(reg::RDT, \"RDT\", slot)?;",
        "    clock::sleep_us(10);",
        "    a < b && a + 1 == b",
        "}",
        "#[cfg(test)]",
        "fn t() { assert!(1 < 2); }",
    ])
    names = [(no, name) for no, name, _, _ in mutations(src)]
    assert (4, "remove register write") in names, names
    assert (5, "no wait") in names, names
    for want in ["boundary < to <=", "&& to ||", "drop + 1", "== to !="]:
        assert (6, want) in names, (want, names)
    assert not any(no in (2, 3, 9) for no, _ in names), names
    out = apply(src, [n for n, _ in names].index(6))
    assert "a <= b && a + 1 == b" in out, out
    print("mutate.py: self-test passed")


def main(argv):
    if len(argv) == 2 and argv[1] == "test":
        selftest()
        return 0
    if len(argv) == 3 and argv[1] == "list":
        for i, (no, name, before, after) in enumerate(mutations(open(argv[2]).read())):
            print(f"{i}\t{no}\t{name}\t{before.strip()}\t=>\t{after.strip()}")
        return 0
    if len(argv) == 5 and argv[1] == "apply":
        src = open(argv[2]).read()
        open(argv[4], "w").write(apply(src, int(argv[3])))
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
