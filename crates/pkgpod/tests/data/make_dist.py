# SPDX-License-Identifier: AGPL-3.0-only
"""Writes a wheel or an sdist for the tests: make_dist.py OUT_DIR NAME VERSION KIND [REQUIRES_PYTHON] [PAD_BYTES]"""
import io, os, sys, tarfile, zipfile

out, name, version, kind = sys.argv[1:5]
requires = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] else None
pad = int(sys.argv[6]) if len(sys.argv) > 6 else 0
mod = name.lower().replace("-", "_").replace(".", "_")
meta = f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nSummary: a test package\n"
if requires:
    meta += f"Requires-Python: {requires}\n"
if kind == "wheel":
    dist = f"{mod}-{version}"
    path = os.path.join(out, f"{dist}-py3-none-any.whl")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr(f"{mod}/__init__.py", f"VALUE = {version!r}\n")
        if pad:
            z.writestr(f"{mod}/pad.bin", os.urandom(pad), compress_type=zipfile.ZIP_STORED)
        z.writestr(f"{dist}.dist-info/METADATA", meta)
        z.writestr(f"{dist}.dist-info/WHEEL", "Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: true\nTag: py3-none-any\n")
        z.writestr(f"{dist}.dist-info/RECORD", "")
else:
    path = os.path.join(out, f"{mod}-{version}.tar.gz")
    with tarfile.open(path, "w:gz") as t:
        data = meta.encode()
        info = tarfile.TarInfo(f"{mod}-{version}/PKG-INFO")
        info.size = len(data)
        t.addfile(info, io.BytesIO(data))
print(path)
