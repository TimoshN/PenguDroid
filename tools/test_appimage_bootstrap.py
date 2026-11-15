"""Execute an ELF through a relocated private loader, not the host interpreter."""
import importlib.util
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("appimage_bootstrap", Path(__file__).with_name("appimage-bootstrap.py"))
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)


class InterpreterTests(unittest.TestCase):
    def test_relocated_executable_runs_with_the_private_loader(self):
        with tempfile.TemporaryDirectory(prefix="pengudroid ELF ") as directory:
            directory = Path(directory)
            binary = directory / "program"
            shutil.copy2("/bin/true", binary)
            data = binary.read_bytes()
            if data[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", data, 18)[0] != 62:
                self.skipTest("requires Linux x86_64 ELF")
            offset = struct.unpack_from("<Q", data, 32)[0]
            size, count = struct.unpack_from("<HH", data, 54)
            segment = next(offset + i * size for i in range(count) if struct.unpack_from("<I", data, offset + i * size)[0] == 3)
            start = struct.unpack_from("<Q", data, segment + 8)[0]
            length = struct.unpack_from("<Q", data, segment + 32)[0]
            loader = directory / "private loader"
            shutil.copy2(data[start:start + length].rstrip(b"\0").decode(), loader)
            bootstrap.patch_interpreter(binary, loader)
            patched = binary.read_bytes()
            start = struct.unpack_from("<Q", patched, segment + 8)[0]
            length = struct.unpack_from("<Q", patched, segment + 32)[0]
            self.assertEqual(patched[start:start + length], str(loader).encode() + b"\0")
            self.assertEqual(subprocess.run([binary]).returncode, 0)

    def test_data_files_are_not_modified(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "data"
            path.write_bytes(b"not an ELF")
            bootstrap.patch_interpreter(path, "/not-used")
            self.assertEqual(path.read_bytes(), b"not an ELF")
