#!/usr/bin/env python3
"""Line, function and branch coverage of the code a set of commits wrote, per crate (001 FR-014):
the coverage of the changes of a story, such as 007 T067 asks for, where the crate's own total also
counts code that other stories wrote.

    scripts/coverage-of-commits.py <coverage dir> <commits file> <crate>...

<coverage dir> is the output of scripts/coverage.sh for those crates (`target/coverage`): its
show.txt gives the lines and its export/ the functions and the branches, summed over every binary
the way coverage.sh sums them. <commits file> holds one full commit hash per line. A line counts
when `git blame` gives it to one of those commits; a branch or a function counts when it starts on
such a line. Prints one row per crate and, under it, each line, branch side and function of those
commits that never ran. Run it from the root of the repository whose code coverage.sh measured.
"""

import json
import os
import subprocess
import sys


def owned_lines(root: str, crate: str, commits: set) -> dict:
    """The lines of each source of `crate` that `git blame` gives to one of `commits`."""
    owned = {}
    for folder, _, names in os.walk(os.path.join(root, "crates", crate, "src")):
        for name in sorted(names):
            if not name.endswith(".rs"):
                continue
            path = os.path.relpath(os.path.join(folder, name), root)
            blame = subprocess.run(
                ["git", "-C", root, "blame", "--line-porcelain", "--", path],
                capture_output=True,
                text=True,
                check=True,
            ).stdout
            lines = set()
            for line in blame.splitlines():
                parts = line.split(" ")
                # A header line: `<hash> <line before> <line now> [<lines in group>]`.
                if len(parts) >= 3 and len(parts[0]) == 40 and parts[0] in commits:
                    lines.add(int(parts[2]))
            if lines:
                owned[path] = lines
    return owned


def line_counts(out: str) -> dict:
    """How many times each line ran, from show.txt: `{(path, line): count}`."""
    counts, path = {}, None
    with open(os.path.join(out, "show.txt"), encoding="utf-8", errors="replace") as show:
        for line in show:
            if "|" not in line and line.endswith(":\n"):
                path = line[:-2]
            elif path is not None and "|" in line:
                number, count, _ = line.split("|", 2)
                if count.strip():
                    counts[(path, int(number))] = int(count)
    return counts


def branches_and_functions(out: str, root: str) -> tuple:
    """Branch sides `{(path, line, column): (taken, not taken)}` and functions
    `{(path, line, column): count}`, summed over every export."""
    real_root = os.path.realpath(root)

    def relative(filename: str) -> str:
        # llvm-cov writes real paths: both sides without symbolic links.
        return os.path.relpath(os.path.realpath(filename), real_root)

    branches, functions = {}, {}
    folder = os.path.join(out, "export")
    for name in sorted(os.listdir(folder)):
        with open(os.path.join(folder, name), encoding="utf-8") as f:
            data = json.load(f)["data"][0]
        for file in data["files"]:
            path = relative(file["filename"])
            for b in file["branches"]:
                key = (path, b[0], b[1])
                taken, not_taken = branches.get(key, (0, 0))
                branches[key] = (taken + b[4], not_taken + b[5])
        for fn in data["functions"]:
            if not fn["regions"]:
                continue
            key = (relative(fn["filenames"][0]), fn["regions"][0][0], fn["regions"][0][1])
            functions[key] = functions.get(key, 0) + fn["count"]
    return branches, functions


def percent(done: int, total: int) -> float:
    return 100.0 * done / total if total else 100.0


def main(argv: list) -> int:
    if len(argv) < 4:
        print(__doc__.strip().splitlines()[3].strip(), file=sys.stderr)
        return 2
    out, commits_file, crates = argv[1], argv[2], argv[3:]
    root = os.getcwd()
    with open(commits_file, encoding="utf-8") as f:
        commits = {line.strip() for line in f if line.strip()}
    lines = line_counts(out)
    branches, functions = branches_and_functions(out, root)
    for crate in crates:
        owned = owned_lines(root, crate, commits)

        def mine(path: str, line: int, owned: dict = owned) -> bool:
            return line in owned.get(path, ())

        ran_lines = [(k, n) for k, n in lines.items() if mine(*k)]
        sides = [(k, s) for k, s in branches.items() if mine(k[0], k[1])]
        fns = [(k, n) for k, n in functions.items() if mine(k[0], k[1])]
        line_done = sum(n > 0 for _, n in ran_lines)
        side_done = sum((t > 0) + (f > 0) for _, (t, f) in sides)
        fn_done = sum(n > 0 for _, n in fns)
        print(
            f"{crate}  lines {line_done}/{len(ran_lines)} {percent(line_done, len(ran_lines)):.2f} %"
            f"  functions {fn_done}/{len(fns)} {percent(fn_done, len(fns)):.2f} %"
            f"  branches {side_done}/{2 * len(sides)} {percent(side_done, 2 * len(sides)):.2f} %"
        )
        missed = [f"{p}:{l}: line" for (p, l), n in ran_lines if not n]
        missed += [f"{p}:{l}:{c}: true" for (p, l, c), (t, _) in sides if not t]
        missed += [f"{p}:{l}:{c}: false" for (p, l, c), (_, f) in sides if not f]
        missed += [f"{p}:{l}:{c}: function never run" for (p, l, c), n in fns if not n]
        for m in sorted(missed):
            print(f"  {m}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
