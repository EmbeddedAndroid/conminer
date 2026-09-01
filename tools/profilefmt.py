#!/usr/bin/env python3
"""Canonical formatter for profiles.d/*.toml.

TOML binds a bare key to whatever table precedes it, so a top-level array written
after an `[[triggers]]` block silently becomes a trigger field and the profile
fails to load. Rather than rely on care, this rewrites every profile into one
canonical order: scalars, then top-level arrays, then array-of-tables, then
plain tables.

    python3 tools/profilefmt.py            # format in place
    python3 tools/profilefmt.py --check    # fail if a profile is unsafe
"""
import glob
import io
import sys
import tomllib

SCALARS = ["name", "stage", "stage_rank", "overlay", "record_timeout_s",
           "ansi_strip", "use_generic", "mine_resync"]
ARRAYS = ["overlay_match", "context", "backtrace_headers", "backtrace_frames",
          "terminators", "continuations", "lookback", "reset_markers", "mine_strip"]
AOT = ["banners", "severity", "triggers", "prompts", "version_banners",
       "structured_lines"]
TABLES = ["extract", "tokenizer"]
KNOWN = set(SCALARS) | set(ARRAYS) | set(AOT) | set(TABLES)


def enc(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if "'''" in v:
        raise SystemExit(f"cannot encode literal containing ''': {v!r}")
    return f"'''{v}'''" if "'" in v else f"'{v}'"


def enc_array(name, xs):
    if len(xs) == 1 and len(xs[0]) < 70:
        return f"{name} = [{enc(xs[0])}]\n"
    return f"{name} = [\n" + "".join(f"  {enc(x)},\n" for x in xs) + "]\n"


def render(path):
    raw = open(path).read()
    d = tomllib.loads(raw)

    unknown = set(d) - KNOWN
    if unknown:
        raise SystemExit(f"{path}: unknown top-level keys {sorted(unknown)}")

    header = []
    for line in raw.split("\n"):
        if line.startswith("#") or line.strip() == "":
            header.append(line)
        else:
            break
    while header and header[-1].strip() == "":
        header.pop()

    out = io.StringIO()
    out.write("\n".join(header) + "\n\n")
    for k in SCALARS:
        if k in d:
            out.write(f"{k} = {enc(d[k])}\n")
    first = True
    for k in ARRAYS:
        if d.get(k):
            if first:
                out.write("\n")
                first = False
            out.write(enc_array(k, d[k]))
    # Known fields first, so the order stays canonical, then whatever else the
    # entry carries. Listing only the known ones dropped every other field on
    # the way through, which is a formatter that eats data: `component` on a
    # version_banners entry disappeared, and only the round-trip assertion
    # below noticed.
    ordered = ("pattern", "stage", "rank", "level", "kind", "severity",
               "lookback", "component")
    for k in AOT:
        for item in d.get(k, []):
            out.write(f"\n[[{k}]]\n")
            for kk in ordered:
                if kk in item:
                    out.write(f"{kk} = {enc(item[kk])}\n")
            for kk in sorted(set(item) - set(ordered)):
                out.write(f"{kk} = {enc(item[kk])}\n")
    for k in TABLES:
        if d.get(k):
            out.write(f"\n[{k}]\n")
            for kk, vv in d[k].items():
                out.write(f"{kk} = {enc(vv)}\n")
    text = out.getvalue()
    tomllib.loads(text)          # the output must still parse
    assert tomllib.loads(text) == d, f"{path}: formatting changed the meaning"
    return text


GROUPS = [("scalar", SCALARS), ("array", ARRAYS), ("table array", AOT),
          ("table", TABLES)]


def group_of(key):
    for i, (_, keys) in enumerate(GROUPS):
        if key in keys:
            return i
    return None


# Names that are legitimately both a top-level key and a field inside a
# section: a profile has a `stage` and so does a banner, a profile has a
# `lookback` array and so does a trigger. They cannot be judged by name, so
# they are not judged. Everything else in SCALARS or ARRAYS belongs only at
# the top level.
#
# Derived from the file rather than declared, this list would defeat itself:
# the parser has already bound a stray key into the preceding table, so it
# would look like a legitimate field of it.
AMBIGUOUS = {"stage", "lookback"}


def validate(path):
    """The hazard this tool exists for, checked without rewriting the file.

    TOML binds a bare key to whatever table precedes it, so a top-level scalar
    or array written after a table header silently becomes a field of that
    table and the profile fails to load.

    Byte-comparing against a re-render would also catch it, but the renderer
    keeps only the leading comment block: every explanation written next to a
    pattern would be dropped, and those explanations are why the patterns can
    be read at all.
    """
    raw = open(path).read()
    d = tomllib.loads(raw)
    unknown = sorted(set(d) - KNOWN)
    if unknown:
        return f"{path}: unknown top-level keys {unknown}"
    top_only = (set(SCALARS) | set(ARRAYS)) - AMBIGUOUS
    table = None
    for n, line in enumerate(raw.split("\n"), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("["):
            table = line.split("]")[0].lstrip("[")
            continue
        if table is not None and "=" in line:
            key = line.split("=")[0].strip()
            if key in top_only:
                return (f"{path}:{n}: '{key}' is a top-level key written after "
                        f"[{table}], so TOML binds it to that table")
    return None


def main():
    check = "--check" in sys.argv
    bad = []
    for path in sorted(glob.glob("profiles.d/*.toml")):
        problem = validate(path)
        if problem:
            print(problem)
            bad.append(path)
            continue
        if check:
            continue
        has_comments = any(l.lstrip().startswith("#")
                           for l in open(path).read().split("\n\n", 1)[-1].split("\n"))
        if has_comments:
            # Rewriting would drop them, and they carry the reasoning behind
            # the patterns.
            continue
        text = render(path)
        if open(path).read() != text:
            open(path, "w").write(text)
            print("formatted", path)
    if bad:
        print("profiles rejected:", " ".join(bad), file=sys.stderr)
        sys.exit(1)
    print("profiles ok")


if __name__ == "__main__":
    main()
