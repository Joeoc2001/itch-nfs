#!/usr/bin/env python3
"""Check strace logs of `itch-nfs scan` for writes an object store mount forbids.

    check-archive-writes.py ARCHIVE_DIR TRACE [TRACE...]

Each TRACE is the `strace -f -y` log of one scanner pass, in order. Under
ARCHIVE_DIR the scanner may only create files that do not exist yet and write
each one front to back: no rename, link, removal, truncation, seek or
positioned write, and no file opened for writing twice. The first pass must
write something, and the last (which has nothing new to archive) nothing.
"""

import os
import re
import sys

FORBIDDEN = ("rename", "renameat", "renameat2", "link", "linkat", "symlink", "symlinkat", "truncate", "ftruncate", "fallocate", "pwrite64", "pwritev", "pwritev2", "copy_file_range", "unlink", "unlinkat")
LINE = re.compile(r"^\d+\s+(\w+)\((.*)$")
OPEN_PATH = re.compile(r'"((?:[^"\\]|\\.)*)"')
FD_PATH = re.compile(r"^\d+<([^>]*)>")


def check(archive, traces):
    problems = []
    written = set()
    per_trace = []
    prefix = archive.rstrip("/") + "/"
    for trace in traces:
        count = 0
        with open(trace, errors="replace") as f:
            for raw in f:
                m = LINE.match(raw)
                if not m or prefix not in raw:
                    continue
                call, rest = m.groups()
                if call in FORBIDDEN:
                    problems.append(f"{call} in the archive: {raw.strip()}")
                elif call in ("openat", "open", "creat"):
                    path = OPEN_PATH.search(rest)
                    if not path or not path.group(1).startswith(prefix):
                        continue
                    path = path.group(1)
                    writing = call == "creat" or "O_WRONLY" in rest or "O_RDWR" in rest
                    if not writing or rest.rstrip().endswith("(No such file or directory)"):
                        continue
                    count += 1
                    if "O_APPEND" in rest:
                        problems.append(f"append in the archive: {raw.strip()}")
                    if "O_CREAT" not in rest and call != "creat":
                        problems.append(f"an existing file opened for writing: {raw.strip()}")
                    if path in written:
                        problems.append(f"{path} was opened for writing twice")
                    written.add(path)
                elif call == "lseek":
                    fd = FD_PATH.match(rest)
                    # Seeking in a file being read is fine; in one being written it is not.
                    if fd and fd.group(1) in written:
                        problems.append(f"seek in a file being written: {raw.strip()}")
        per_trace.append(count)
    on_disk = set()
    for base, _dirs, names in os.walk(archive):
        on_disk.update(os.path.join(base, n) for n in names)
    if written != on_disk:
        problems.append(f"files written ({len(written)}) are not the files in the archive ({len(on_disk)}): {sorted(written ^ on_disk)}")
    if per_trace[0] == 0:
        problems.append("the first pass wrote nothing: the check checked nothing")
    if len(per_trace) > 1 and per_trace[-1] != 0:
        problems.append(f"the last pass wrote {per_trace[-1]} file(s) although nothing was new")
    return problems, per_trace


def main():
    archive, traces = os.path.realpath(sys.argv[1]), sys.argv[2:]
    problems, per_trace = check(archive, traces)
    for p in problems:
        print(f"::error::{p}")
    print(f"files opened for writing in the archive, per pass: {per_trace}; {len(problems)} problem(s)")
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
