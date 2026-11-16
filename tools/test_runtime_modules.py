"""Offline modular runtime regressions: no downloads or system image required."""
import importlib.util
import io
import json
from pathlib import Path
import shutil
import struct
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runtime_modules", Path(__file__).with_name("runtime-modules.py"))
modules = importlib.util.module_from_spec(spec)
spec.loader.exec_module(modules)


def elf64(needed=()):
    strings = bytearray(b"\0")
    offsets = []
    for name in needed:
        offsets.append(len(strings))
        strings.extend(name.encode() + b"\0")
    dynamic_offset = 64 + 2 * 56
    dynamic_size = 16 * (len(needed) + 3)
    strings_offset = dynamic_offset + dynamic_size
    size = strings_offset + len(strings)
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HHI", header, 16, 3, 62, 1)
    struct.pack_into("<Q", header, 32, 64)
    struct.pack_into("<HHH", header, 52, 64, 56, 2)
    ph = struct.pack("<II6Q", 1, 4, 0, 0x1000, 0, size, size, 4096)
    ph += struct.pack("<II6Q", 2, 4, dynamic_offset, 0x1000 + dynamic_offset, 0, dynamic_size, dynamic_size, 8)
    dynamic = b"".join(struct.pack("<qQ", 1, index) for index in offsets)
    dynamic += struct.pack("<qQ", 5, 0x1000 + strings_offset)
    dynamic += struct.pack("<qQ", 10, len(strings)) + bytes(16)
    return bytes(header) + ph + dynamic + strings


class RuntimeModulesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.store = self.root / "modules"
        self.output = self.root / "assembled"
        for name, data in {
            "system/build.prop": b"ro.build.version.sdk=37\n",
            "system/apex/com.android.art/lib64/libart.so": b"\x7fELF\x02\x01" + bytes(12) + b"\x3e\x00",
            "system/framework/framework.jar": b"framework fixture",
            "system/framework/framework-res.apk": b"resource fixture",
            "system/lib64/libc.so": b"native fixture",
            "system/etc/fonts.xml": b"config fixture",
            "system/fonts/font.ttf": b"font fixture",
            "system/bin/tool": b"executable fixture",
            "data/user/secret": b"never package app data",
        }.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        (self.source / "system/bin/tool").chmod(0o755)
        (self.source / "system/lib64/libart.so").symlink_to("/apex/com.android.art/lib64/libart.so")

    def pack(self):
        return modules.pack(self.source, self.store, profile="full")

    def malicious(self, name, target=None):
        self.pack()
        archive = self.store / "evil.tar"
        with tarfile.open(archive, "w") as payload:
            member = tarfile.TarInfo(name)
            if target is not None:
                member.type = tarfile.SYMTYPE
                member.linkname = target
                payload.addfile(member)
            else:
                member.size = 4
                payload.addfile(member, io.BytesIO(b"evil"))
        manifest = json.loads((self.store / "runtime-modules.json").read_text())
        manifest["modules"].append({"name": "evil", "archive": archive.name,
                                    "bytes": archive.stat().st_size, "sha256": modules.digest(archive)})
        (self.store / "runtime-modules.json").write_text(json.dumps(manifest))

    def test_roundtrip_runs_without_original_tree_and_excludes_user_data(self):
        self.pack()
        shutil.rmtree(self.source)
        modules.assemble(self.store, self.output, layout="legacy")
        self.assertEqual((self.output / "system/lib64/libart.so").read_bytes()[:4], b"\x7fELF")
        self.assertEqual((self.output / "system/framework/framework.jar").read_bytes(), b"framework fixture")
        self.assertEqual((self.output / "system/bin/tool").stat().st_mode & 0o777, 0o755)
        self.assertFalse((self.output / "data").exists())
        self.assertTrue((self.output / "pengudroid-modular-runtime.json").is_file())

    def test_archives_are_reproducible(self):
        self.pack()
        second = self.root / "second"
        modules.pack(self.source, second, profile="full")
        self.assertEqual((self.store / "runtime-modules.json").read_bytes(), (second / "runtime-modules.json").read_bytes())

    def test_corruption_does_not_publish_partial_runtime(self):
        self.pack()
        next(self.store.glob("*.tar")).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "checksum"):
            modules.assemble(self.store, self.output, layout="legacy")
        self.assertFalse(self.output.exists())
        self.assertFalse(list(self.root.glob(".runtime-modules-*")))

    def test_rejects_traversal_collisions_and_links_used_as_directories(self):
        for name, target in [("../escaped", None), ("/tmp/escaped", None),
                             ("system/build.prop", None), ("system/escape", "../../outside"),
                             ("system/alias", "../system"), ("system/lib64", "../system")]:
            with self.subTest(name=name):
                shutil.rmtree(self.store, ignore_errors=True)
                self.malicious(name, target)
                if name == "system/alias":
                    # A leaf alias alone is valid; a child beneath it is not.
                    manifest = json.loads((self.store / "runtime-modules.json").read_text())
                    archive = self.store / "evil.tar"
                    with tarfile.open(archive, "a") as payload:
                        child = tarfile.TarInfo("system/alias/child")
                        payload.addfile(child, io.BytesIO())
                    manifest["modules"][-1].update(bytes=archive.stat().st_size, sha256=modules.digest(archive))
                    (self.store / "runtime-modules.json").write_text(json.dumps(manifest))
                with self.assertRaises((ValueError, OSError)):
                    modules.assemble(self.store, self.output, layout="legacy")
                self.assertFalse(self.output.exists())

    def test_does_not_overwrite_existing_destination(self):
        self.pack()
        self.output.mkdir()
        (self.output / "keep").write_text("user content")
        with self.assertRaisesRegex(ValueError, "already exists"):
            modules.assemble(self.store, self.output, layout="legacy")
        self.assertEqual((self.output / "keep").read_text(), "user content")

    def desktop_fixture(self):
        (self.source / "system/apex/com.android.art/lib64/libart.so").write_bytes(elf64(["libextra.so"]))
        extra = self.source / "system/apex/com.android.extra/lib64/libextra.so"
        extra.parent.mkdir(parents=True)
        extra.write_bytes(elf64(["libleaf.so"]))
        (extra.parent / "libdynamic_only.so").write_bytes(elf64())
        (extra.parent / "libleaf.so").write_bytes(elf64())
        for name in ("system/app/unused.apk", "system/lib/unused.so", "system/bin/tool",
                     "system/framework/x86/boot.art", "system/framework/oat/x86_64/unused.odex",
                     "system/apex/com.android.art/lib/unused.so", "system/apex/com.android.extra/unused.apk",
                     "system/apex/unused.capex"):
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"unused")

    def test_desktop_profile_closes_transitive_native_dependencies_and_excludes_os(self):
        self.desktop_fixture()
        modules.pack(self.source, self.store)
        manifest = json.loads((self.store / "runtime-modules.json").read_text())
        art = next(module for module in manifest["modules"] if module["name"] == "apex-com.android.art")
        self.assertIn("apex-com.android.extra", art["requires"])
        native = next(module for module in manifest["modules"] if module["name"] == "native")
        self.assertIn("apex-com.android.art", native["requires"])
        self.assertIn("system/lib64/libart.so", manifest["symlink_dependencies"])
        self.assertEqual(manifest["elf_edges"], 2)
        modules.assemble(self.store, self.output, layout="legacy")
        self.assertTrue((self.output / "apex/com.android.extra/lib64/libleaf.so").is_file())
        self.assertTrue((self.output / "apex/com.android.extra/lib64/libdynamic_only.so").is_file())
        self.assertTrue((self.output / "system/fonts/font.ttf").is_file())
        self.assertTrue((self.output / "system/framework/framework-res.apk").is_file())
        for name in ("app", "lib", "bin/tool", "framework/x86", "framework/oat", "apex/unused.capex"):
            self.assertFalse((self.output / "system" / name).exists(), name)

    def test_desktop_profile_refuses_missing_dependency_without_publishing(self):
        self.desktop_fixture()
        (self.source / "system/apex/com.android.extra/lib64/libleaf.so").unlink()
        with self.assertRaisesRegex(ValueError, "missing ELF dependency libleaf.so"):
            modules.pack(self.source, self.store)
        self.assertFalse(self.store.exists())

    def test_module_graph_rejects_deleted_required_modules(self):
        self.desktop_fixture()
        modules.pack(self.source, self.store)
        path = self.store / "runtime-modules.json"
        manifest = json.loads(path.read_text())
        manifest["modules"] = [m for m in manifest["modules"] if m["name"] != "apex-com.android.extra"]
        path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "missing required module"):
            modules.assemble(self.store, self.output, layout="legacy")
        self.assertFalse(self.output.exists())

    def test_installed_identity_preserves_manifest_bytes_for_idempotent_updates(self):
        self.pack()
        path = self.store / "runtime-modules.json"
        path.write_text(json.dumps(json.loads(path.read_text()), separators=(",", ":")))
        modules.assemble(self.store, self.output, layout="legacy")
        self.assertEqual(path.read_bytes(), (self.output / "pengudroid-modular-runtime.json").read_bytes())

    def test_components_install_has_no_system_apex_or_build_prop(self):
        self.pack()
        shutil.rmtree(self.source)
        modules.assemble(self.store, self.output)
        self.assertFalse((self.output / "system").exists())
        self.assertFalse((self.output / "apex").exists())
        self.assertFalse((self.output / "components/platform/build.prop").exists())
        self.assertEqual((self.output / "components/native/libart.so").read_bytes()[:4], b"\x7fELF")
        descriptor = json.loads((self.output / "pengudroid-runtime.json").read_text())
        self.assertEqual(descriptor["properties"]["ro.build.version.sdk"], "37")
        self.assertEqual(descriptor["components"]["framework"], "components/framework")
        mapping = {m["guest"]: m["host"] for m in descriptor["mounts"]}
        self.assertEqual(mapping["/system/etc"], "components/config")
        self.assertEqual(mapping["/apex/com.android.art"], "components/com.android.art")

    def test_add_guest_materializes_links_and_uses_component_paths(self):
        self.pack()
        guest = self.root / "guest"
        for name, data in {
            "pengudroid-arm64-bridge-bundle.json": b'{"schema":1}',
            "system/bin/arm64/linker64": b"guest linker fixture",
            "system/bin/arm64/app_process64": b"guest executable fixture",
            "system/lib64/arm64/libc.so": b"guest libc fixture",
            "system/etc/ld.config.arm64.txt": b"guest config fixture",
        }.items():
            path = guest / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        (guest / "system/lib64/arm64/alias.so").symlink_to("libc.so")
        combined = self.root / "combined"
        modules.add_guest(self.store, guest, combined)
        shutil.rmtree(guest)
        shutil.rmtree(self.store)
        modules.assemble(combined, self.output)
        self.assertEqual((self.output / "components/arm64/lib/alias.so").read_bytes(), b"guest libc fixture")
        self.assertFalse((self.output / "components/arm64/lib/alias.so").is_symlink())
        self.assertTrue((self.output / "components/arm64/bin/linker64").is_file())
        self.assertFalse((self.output / "system").exists())
        descriptor = json.loads((self.output / "pengudroid-runtime.json").read_text())
        self.assertEqual(descriptor["components"]["arm64-guest"], "components/arm64")

    def test_version_one_store_remains_readable(self):
        self.pack()
        path = self.store / "runtime-modules.json"
        manifest = json.loads(path.read_text())
        manifest["format"] = 1
        for key in ("roots", "profile", "native_dependencies"):
            manifest.pop(key, None)
        for module in manifest["modules"]:
            module.pop("requires", None)
        path.write_text(json.dumps(manifest))
        modules.assemble(self.store, self.output, layout="legacy")
        self.assertTrue((self.output / "system/framework/framework.jar").is_file())

    def test_elf_reader_rejects_truncation_unmapped_tables_and_invalid_names(self):
        path = self.root / "elf.so"
        path.write_bytes(elf64(["liba.so"]))
        self.assertEqual(modules.elf_needed(path), ["liba.so"])
        for image in (elf64()[:40], elf64(["../escape.so"]), elf64(["liba.so"])[:-1]):
            path.write_bytes(image)
            with self.assertRaises(ValueError):
                modules.elf_needed(path)

    def test_rejects_wrong_sdk_and_architecture(self):
        (self.source / "system/build.prop").write_text("ro.build.version.sdk=33\n")
        with self.assertRaisesRegex(ValueError, "API 37"):
            self.pack()
        (self.source / "system/build.prop").write_text("ro.build.version.sdk=37\n")
        (self.source / "system/apex/com.android.art/lib64/libart.so").write_bytes(bytes(20))
        with self.assertRaisesRegex(ValueError, "x86_64"):
            self.pack()


if __name__ == "__main__":
    unittest.main()
