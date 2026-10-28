#!/usr/bin/env python3
"""Point a side module's imports of libc functions at vnhost's wrappers.

wasm-ld's --wrap crashes when linking a side module, so the Wine unix side is linked plainly
and its imports are renamed here instead: env.mmap -> env.__wrap_mmap, GOT.func.mmap ->
GOT.func.__wrap_mmap (address-taken functions), same for every name given.

usage: wrap-imports.py module.so name...
"""
import sys


def uleb(b, i):
    v = s = 0
    while True:
        c = b[i]
        i += 1
        v |= (c & 0x7f) << s
        s += 7
        if c < 0x80:
            return v, i


def enc(v):
    out = bytearray()
    while True:
        c = v & 0x7f
        v >>= 7
        out.append(c | (0x80 if v else 0))
        if not v:
            return bytes(out)


def name(b, i):
    n, i = uleb(b, i)
    return b[i:i + n].decode(), i + n


def wname(s):
    s = s.encode()
    return enc(len(s)) + s


def limits(b, i):
    flags = b[i]
    i += 1
    _, i = uleb(b, i)
    if flags & 1:
        _, i = uleb(b, i)
    return i


def rewrite_imports(body, wrap):
    count, i = uleb(body, 0)
    out = bytearray(enc(count))
    renamed = 0
    for _ in range(count):
        mod, i = name(body, i)
        field, i = name(body, i)
        start = i
        kind = body[i]
        i += 1
        if kind == 0:
            _, i = uleb(body, i)
        elif kind == 1:
            i = limits(body, i + 1)
        elif kind == 2:
            i = limits(body, i)
        elif kind == 3:
            i += 2
        elif kind == 4:
            _, i = uleb(body, i + 1)
        else:
            raise SystemExit(f"unknown import kind {kind}")
        if field in wrap and (mod == "env" and kind == 0 or mod == "GOT.func"):
            field = "__wrap_" + field
            renamed += 1
        out += wname(mod) + wname(field) + body[start:i]
    return bytes(out), renamed


def rewrite_dylink(body, wrap):
    i = 0
    out = bytearray()
    while i < len(body):
        sub = body[i]
        size, j = uleb(body, i + 1)
        data = body[j:j + size]
        if sub == 4:  # WASM_DYLINK_IMPORT_INFO
            n, k = uleb(data, 0)
            nd = bytearray(enc(n))
            for _ in range(n):
                mod, k = name(data, k)
                field, k = name(data, k)
                flags, k = uleb(data, k)
                if field in wrap and mod in ("env", "GOT.func"):
                    field = "__wrap_" + field
                nd += wname(mod) + wname(field) + enc(flags)
            data = bytes(nd)
        out += bytes([sub]) + enc(len(data)) + data
        i = j + size
    return bytes(out)


def main():
    path, wrap = sys.argv[1], set(sys.argv[2:])
    b = open(path, "rb").read()
    assert b[:4] == b"\0asm"
    out = bytearray(b[:8])
    i = 8
    total = 0
    while i < len(b):
        sid = b[i]
        size, j = uleb(b, i + 1)
        body = b[j:j + size]
        if sid == 2:
            body, total = rewrite_imports(body, wrap)
        elif sid == 0:
            cname, k = name(body, 0)
            if cname == "dylink.0":
                body = body[:k] + rewrite_dylink(body[k:], wrap)
        out += bytes([sid]) + enc(len(body)) + body
        i = j + size
    open(path, "wb").write(out)
    print(f"{path}: {total} imports wrapped")


main()
