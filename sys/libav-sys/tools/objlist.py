"""Print the objects FFmpeg's make would build into libavutil.a and libavcodec.a.

A small evaluator of the make subset FFmpeg's library Makefiles use (assignments,
+=, ifdef/ifndef/ifeq/ifneq, include, $(VAR) expansion), so the source list comes
from the Makefiles without running make over the tree. Mirrors Makefile DOSUBDIR,
ffbuild/arch.mak and ffbuild/common.mak for a static build.

usage: python3 -I objlist.py <ffmpeg src> <configure build dir>

cargo xtask libav-config passes the submodule (sys/libav-sys/ffmpeg) as <ffmpeg src>.
"""
import os
import re
import sys

REF = re.compile(r"\$\(([^()$]*)\)")


def call(body, env):
    """A variable reference or one of the two make functions these Makefiles use."""
    if " " not in body:
        return env.get(body, "")
    fn, args = body.split(" ", 1)
    if fn == "if":
        parts = args.split(",")
        then = parts[1] if len(parts) > 1 else ""
        other = parts[2] if len(parts) > 2 else ""
        return then if parts[0].strip() else other
    if fn == "addprefix":
        prefix, words = args.split(",", 1)
        return " ".join(prefix.strip() + w for w in words.split())
    raise SystemExit(f"unsupported make function: $({body})")


def expand(s, env):
    # Innermost references first, so $(X-$(CONFIG_Y)) resolves.
    while True:
        new = REF.sub(lambda m: call(m.group(1), env), s)
        if new == s:
            return new
        s = new


def logical_lines(path):
    with open(path, encoding="utf-8") as f:
        raw = f.read().split("\n")
    out, cur = [], ""
    for line in raw:
        if line.startswith("\t"):
            if not cur:
                continue  # a recipe line
        stripped = line.split("#", 1)[0] if not cur else line.split("#", 1)[0]
        if stripped.endswith("\\"):
            cur += stripped[:-1] + " "
            continue
        out.append(cur + stripped)
        cur = ""
    if cur:
        out.append(cur)
    return out


def cond_true(kind, arg, env):
    if kind in ("ifdef", "ifndef"):
        v = env.get(expand(arg.strip(), env), "")
        return bool(v) == (kind == "ifdef")
    m = re.match(r"\((.*),(.*)\)$", arg.strip())
    if not m:
        raise SystemExit(f"unsupported conditional: {kind} {arg}")
    a, b = expand(m.group(1), env).strip(), expand(m.group(2), env).strip()
    return (a == b) == (kind == "ifeq")


ASSIGN = re.compile(r"^([^:+?=\s]+)\s*(\+=|:=|\?=|=)\s*(.*)$")


def run(path, env, src, optional=False):
    if not os.path.exists(path):
        if optional:
            return
        raise SystemExit(f"missing {path}")
    stack = []  # (active, taken)
    for line in logical_lines(path):
        t = line.strip()
        if not t:
            continue
        word = t.split(None, 1)
        head, rest = word[0], (word[1] if len(word) > 1 else "")
        active = all(a for a, _ in stack)
        if head in ("ifdef", "ifndef", "ifeq", "ifneq"):
            ok = active and cond_true(head, rest, env)
            stack.append((ok, ok))
            continue
        if head == "else":
            a, taken = stack.pop()
            parent = all(x for x, _ in stack)
            if rest:
                k, r = rest.split(None, 1)
                ok = parent and not taken and cond_true(k, r, env)
                stack.append((ok, taken or ok))
            else:
                stack.append((parent and not taken, True))
            continue
        if head == "endif":
            stack.pop()
            continue
        if not active:
            continue
        if head in ("include", "-include"):
            p = expand(rest, env).strip()
            if p.endswith("library.mak") or p.endswith("common.mak"):
                continue
            run(p, env, src, optional=head == "-include")
            continue
        if head.startswith("define") or head == "endef":
            raise SystemExit(f"unsupported define in {path}")
        m = ASSIGN.match(t)
        if not m or ":" in m.group(1):
            continue  # a rule
        name, op, value = expand(m.group(1), env), m.group(2), m.group(3)
        value = expand(value, env).strip()
        if op == "+=":
            env[name] = (env.get(name, "") + " " + value).strip()
        elif op == "?=":
            env.setdefault(name, value)
        else:
            env[name] = value


def config_env(build):
    env = {}
    with open(os.path.join(build, "ffbuild/config.mak"), encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("#"):
                continue
            # `!CONFIG_X=yes` is how configure names a disabled component.
            m = re.match(r"^(!?[A-Za-z0-9_\-]+)=(.*)$", line)
            if m:
                env[m.group(1)] = m.group(2)
    return env


RESET = (
    "OBJS SHLIBOBJS STLIBOBJS X86ASM-OBJS HEADERS ARCH_HEADERS BUILT_HEADERS "
    "SKIPHEADERS FFLIBS HOSTPROGS TESTPROGS TOOLS TESTOBJS CLEANFILES "
    "ARMV5TE-OBJS ARMV6-OBJS ARMV8-OBJS VFP-OBJS NEON-OBJS ALTIVEC-OBJS VSX-OBJS "
    "MIPSFPU-OBJS MIPSDSPR2-OBJS MIPSDSP-OBJS MSA-OBJS MMI-OBJS LSX-OBJS LASX-OBJS "
    "RV-OBJS RVV-OBJS RVVB-OBJS SIMD128-OBJS SVE-OBJS SVE2-OBJS SME-OBJS SME2-OBJS"
).split()


def lib_objects(src, base, lib):
    env = dict(base)
    env["SRC_PATH"] = src
    for v in RESET:
        env[v] = ""
        env[v + "-yes"] = ""
    env["SUBDIR"] = lib + "/"
    run(os.path.join(src, lib, "Makefile"), env, src)
    run(os.path.join(src, lib, env["ARCH"], "Makefile"), env, src, optional=True)
    run(os.path.join(src, lib, env.get("INTRINSICS", "none"), "Makefile"), env, src, optional=True)
    run(os.path.join(src, "ffbuild/arch.mak"), env, src)
    objs = env["OBJS"].split() + env["OBJS-yes"].split()
    objs += env["STLIBOBJS"].split() + env["STLIBOBJS-yes"].split()
    return sorted({lib + "/" + o for o in objs})


def main():
    src, build = sys.argv[1], sys.argv[2]
    base = config_env(build)
    for lib in ("libavutil", "libavcodec"):
        for o in lib_objects(src, base, lib):
            stem = o[:-2]
            for ext in (".c", ".asm", ".S"):
                if os.path.exists(os.path.join(src, stem + ext)):
                    print(stem + ext)
                    break
                if os.path.exists(os.path.join(build, stem + ext)):
                    print(stem + ext)
                    break
            else:
                raise SystemExit(f"no source for {o}")


main()
