import importlib.util
import json
import os
import subprocess
from pathlib import Path
import shutil
import struct
import tempfile
import unittest
from test_runtime_modules import elf64

spec=importlib.util.spec_from_file_location("pengu_guest",Path(__file__).with_name("pengu-guest.py"))
guest=importlib.util.module_from_spec(spec);spec.loader.exec_module(guest)

def arm_elf(needed=()):
    data=bytearray(elf64(needed));struct.pack_into("<H",data,18,183);return data

class NativeOnlyGuestTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.source=self.root/"source";self.output=self.root/"output"
        for name in guest.SEEDS:
            path=self.source/guest.ROOT_LIB/name;path.parent.mkdir(parents=True,exist_ok=True)
            path.write_bytes(arm_elf())
        for name in ["linker64","app_process64"]:
            path=self.source/"system/bin/arm64"/name;path.parent.mkdir(parents=True,exist_ok=True)
            path.write_bytes(arm_elf(["libc.so"]))
        config=self.source/"system/etc/ld.config.arm64.txt";config.parent.mkdir(parents=True)
        config.write_text("dir.system = /system\n")
        (self.source/"pengudroid-arm64-bridge-bundle.json").write_text(json.dumps({"schema":1,"files":{}}))
        # These must not leak into the independent output, even if present in source.
        for name in ["libart.so","libandroid_runtime.so","libhwui.so"]:
            (self.source/guest.ROOT_LIB/name).write_bytes(arm_elf())
    def test_native_closure_is_flat_and_has_no_framework_or_source_tree_dependency(self):
        guest.build(self.source,self.output)
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o755)
        self.assertFalse((self.output/"system").exists());self.assertFalse((self.output/"apex").exists())
        names={p.name for p in self.output.rglob("*")}
        self.assertTrue({"libart.so","libandroid_runtime.so","libhwui.so"}.isdisjoint(names))
        descriptor=json.loads((self.output/"pengudroid-runtime.json").read_text())
        self.assertEqual(descriptor["profile"],"pengu")
        self.assertEqual(set(descriptor["components"]),{"arm64-guest"})
        shutil.rmtree(self.source)
        self.assertTrue((self.output/"native/bin/linker64").is_file())
        self.assertTrue((self.output/"native/lib64/libc.so").is_file())
    def test_real_shim_maps_native_descriptor_before_legacy_overlay(self):
        shim = Path(__file__).resolve().parent.parent / "target/debug/libpengudroid_shim.so"
        if not shim.is_file():
            self.skipTest("build pengudroid-shim for the live ABI mapping probe")
        guest.build(self.source, self.output)
        code = r'''
import ctypes, os
from pathlib import Path
root = Path(os.environ["PENGUDROID_ARM64_GUEST_ROOT"])
lib = ctypes.CDLL(None)
lib.__pengudroid_map_path.argtypes = [ctypes.c_char_p]
lib.__pengudroid_map_path.restype = ctypes.c_void_p
lib.__pengudroid_free_path.argtypes = [ctypes.c_void_p]
for source, dest in [(b"/system", root / "native"),
    (b"/system/lib64/arm64/libc.so", root / "native/lib64/libc.so"),
    (b"/system/framework/framework.jar", root / "native/__unsupported__/system/framework/framework.jar")]:
    pointer = lib.__pengudroid_map_path(source)
    assert pointer and ctypes.string_at(pointer) == os.fsencode(dest)
    lib.__pengudroid_free_path(pointer)
assert not lib.__pengudroid_map_path(b"/etc/passwd")
lib.realpath.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
lib.realpath.restype = ctypes.c_char_p
for path in [b"/system", b"/system/bin/arm64/linker64", b"/system/lib64/arm64/libc.so"]:
    buf = ctypes.create_string_buffer(4096)
    assert lib.realpath(path, buf) == path, (path, buf.value)
'''
        environment = {key: value for key, value in os.environ.items()
            if not key.startswith("PENGUDROID_") and key != "LD_PRELOAD"}
        environment.update(PENGUDROID_RUNTIME_MANIFEST=str(self.output / "pengudroid-runtime.json"),
            PENGUDROID_ARM64_GUEST_ROOT=str(self.output), PENGUDROID_SYSROOT=str(self.source),
            LD_PRELOAD=str(shim))
        result = subprocess.run(["python3", "-c", code], env=environment, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
    def test_forbidden_transitive_dependency_is_an_error_not_a_silent_copy(self):
        (self.source/guest.ROOT_LIB/"libandroid.so").write_bytes(arm_elf(["libart.so"]))
        with self.assertRaisesRegex(ValueError,"forbidden framework dependency"):
            guest.build(self.source,self.output)
        self.assertFalse(self.output.exists())
    def test_wrong_machine_and_missing_dependencies_are_rejected(self):
        library=self.source/guest.ROOT_LIB/"libandroid.so"
        library.write_bytes(elf64())
        with self.assertRaisesRegex(ValueError,"AArch64"):guest.build(self.source,self.output)
        library.write_bytes(arm_elf(["libmissing.so"]))
        with self.assertRaisesRegex(ValueError,"missing native ABI input"):guest.build(self.source,self.output)
    def test_provenance_hash_mismatch_and_existing_output_are_not_overwritten(self):
        manifest=self.source/"pengudroid-arm64-bridge-bundle.json"
        manifest.write_text(json.dumps({"schema":1,"files":{"system/bin/arm64/linker64":{"sha256":"0"*64}}}))
        with self.assertRaisesRegex(ValueError,"file changed"):guest.build(self.source,self.output)
        self.assertFalse(self.output.exists())
        self.output.mkdir();(self.output/"keep").write_text("user data")
        with self.assertRaisesRegex(ValueError,"new or empty"):guest.build(self.source,self.output)
        self.assertEqual((self.output/"keep").read_text(),"user data")

if __name__=="__main__":unittest.main()
