#!/usr/bin/env python3
"""Fetch a pinned static AppImage runtime; default to extraction without FUSE."""
import hashlib
from pathlib import Path
import sys
import urllib.request

URL = 'https://github.com/VHSgunzo/uruntime/releases/download/v0.8.3/uruntime-appimage-squashfs-lite-x86_64'
SHA256 = '85caec9c27c60933f1754281dd39b423f557b1d821a137698d98e6433f48e209'

def prepare(root):
    cache = root / '.toolchain/uruntime'
    cache.mkdir(parents=True, exist_ok=True)
    source = cache / 'uruntime-appimage-squashfs-lite-x86_64'
    data = source.read_bytes() if source.exists() else urllib.request.urlopen(URL).read()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise RuntimeError('uruntime SHA-256 mismatch')
    source.write_bytes(data)
    if data.count(b'URUNTIME_EXTRACT=3') != 1:
        raise RuntimeError('unexpected uruntime extraction policy layout')
    output = cache / 'pengudroid-runtime-x86_64'
    output.write_bytes(data.replace(b'URUNTIME_EXTRACT=3', b'URUNTIME_EXTRACT=1'))
    output.chmod(0o755)
    return output

if __name__ == '__main__':
    print(prepare(Path(__file__).resolve().parent.parent))
