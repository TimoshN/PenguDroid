#!/usr/bin/env python3
"""Tests for the one-file AppImage bundle manager."""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "tools" / "build-appimage.sh"
RELEASE = REPO / "target" / "release"
ARTIFACTS = (
    "pengudroid",
    "pengudroid-gui",
    "pengudroid-daemon",
    "libpengudroid_shim.so",
    "libpengudroid_gleshim.so",
    "libpengudroid_vkshim.so",
)


def run(*arguments, env=None, check=False):
    return subprocess.run(
        ["sh", str(SCRIPT), *arguments],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        check=check,
    )


class ScriptTests(unittest.TestCase):
    def test_parses_with_a_posix_shell(self):
        result = subprocess.run(
            ["sh", "-n", str(SCRIPT)], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_independent_profile_refuses_android_payload_options(self):
        for arguments in [("--pengu", "--with-runtime"), ("--pengu", "--with-modules", "/unused/store")]:
            result = run(*arguments)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("excludes ART/framework", result.stderr)

    def test_rejects_an_unknown_option(self):
        result = run("--definitely-not-an-option")
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown option", result.stderr)


class AppDirTests(unittest.TestCase):
    """Assemble a real AppDir from stand-ins for the release binaries."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="test-appimage-")
        self.addCleanup(self.temporary.cleanup)
        self.fake = Path(self.temporary.name)
        # The script resolves its artifacts under <repo>/target, so run a copy of
        # the tree layout it expects; copying the repo is too expensive, so only
        # the paths the script touches are mirrored.
        self.appdir = REPO / "target" / "appimage" / "PenguDroid.AppDir"

    def test_independent_appdir_does_not_provision_or_load_android_runtime(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        result = run("--skip-build", "--pengu", "--no-image")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.appdir / "usr/share/pengudroid/pengu-backend").is_file())
        forbidden = {"libart.so", "libartd.so", "libandroid_runtime.so", "libhwui.so", "libandroidfw.so", "framework.jar", "core-oj.jar", "core-libart.jar"}
        self.assertFalse([str(path) for path in self.appdir.rglob("*") if path.name in forbidden or path.suffix == ".jar"])
        environment = dict(os.environ, PENGUDROID_RUNTIME_MANIFEST="/missing/should-not-be-loaded.json",
            PENGUDROID_SYSROOT="/missing/legacy-system", PENGUDROID_RUNTIME_MODULES="/missing/modules")
        result = subprocess.run(["sh", str(self.appdir / "AppRun"), "--cli", "compat-index"],
            env=environment, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        import json
        data = json.loads(result.stdout)
        self.assertEqual(data["forbidden_android_runtime_mappings"], [])
        self.assertGreater(len(data["wired_jni_slots"]), 0)
        result = subprocess.run(["sh", str(self.appdir / "AppRun"), "--cli", "runtime", "--download"],
            env=environment, text=True, capture_output=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("independent profile excludes", result.stderr)

    def test_assembled_layout(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        result = run("--skip-build", "--no-image", "--output", str(self.fake / "out"))
        self.assertEqual(result.returncode, 0, result.stderr)

        self.assertIn("assembled", result.stdout)
        self.assertEqual(
            sorted(path.name for path in (self.appdir / "usr/bin").iterdir()),
            sorted(ARTIFACTS),
        )
        for name in ARTIFACTS:
            self.assertTrue(os.access(self.appdir / "usr/bin" / name, os.X_OK), name)

        apprun = (self.appdir / "AppRun").read_text()
        self.assertTrue(apprun.startswith("#!/bin/sh"))
        self.assertIn('exec "$HERE/usr/bin/pengudroid-gui"', apprun)
        # An explicit choice must survive the AppRun defaults: a developer
        # testing a freshly prepared translator relies on it.
        self.assertIn('PENGUDROID_NATIVE_BRIDGE_LIB:=$HERE/usr/lib/pengudroid/bridge', apprun)
        self.assertIn('[ -z "${PENGUDROID_SYSROOT:-}" ]', apprun)

    def test_desktop_entry_launches_the_shell_and_accepts_apk_files(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        run("--skip-build", "--no-image")
        entry = (self.appdir / "pengudroid.desktop").read_text()
        self.assertIn("Type=Application", entry)
        self.assertIn("Name=PenguDroid", entry)
        self.assertIn("Exec=pengudroid-gui", entry)
        self.assertIn("Icon=pengudroid", entry)
        # The "+" button installs through the CLI, so the file manager should be
        # able to hand an APK to the shell as well.
        self.assertIn("MimeType=application/vnd.android.package-archive", entry)
        self.assertTrue((self.appdir / "pengudroid.png").is_file())
        self.assertEqual(
            (self.appdir / "pengudroid.desktop").read_bytes(),
            (self.appdir / "usr/share/applications/pengudroid.desktop").read_bytes(),
        )

    def test_module_store_is_bundled_with_its_offline_installer(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        store = self.fake / "store"
        store.mkdir()
        (store / "runtime-modules.json").write_text('{"format":2}')
        (store / "native.tar").write_bytes(b"archive fixture")
        result = run("--skip-build", "--no-image", "--with-modules", str(store))
        self.assertEqual(result.returncode, 0, result.stderr)
        bundled = self.appdir / "usr/share/pengudroid"
        self.assertEqual((bundled / "runtime-modules/native.tar").read_bytes(), b"archive fixture")
        self.assertTrue((bundled / "setup/tools/runtime-modules.py").is_file())
        self.assertIn("PENGUDROID_RUNTIME_MODULES", (self.appdir / "AppRun").read_text())

    def test_arm64_component_store_omits_legacy_guest_sysroot(self):
        bridge = REPO / ".toolchain/nativebridge-stage"
        if not all((RELEASE / name).is_file() for name in ARTIFACTS) or not (bridge / "libberberis_arm64.so").is_file():
            self.skipTest("release binaries/native bridge are not built")
        store = self.fake / "components"
        store.mkdir()
        (store / "runtime-modules.json").write_text('{"format":2,"arm64_guest":true}')
        (store / "arm64-guest.tar").write_bytes(b"archive fixture")
        result = run("--skip-build", "--no-image", "--with-bridge", "--bridge-dir", str(bridge), "--with-modules", str(store))
        self.assertEqual(result.returncode, 0, result.stderr)
        bundled = self.appdir / "usr/share/pengudroid"
        self.assertFalse((bundled / "sysroot").exists())
        self.assertFalse((bundled / "guest-arm64").exists())
        self.assertTrue((self.appdir / "usr/lib/pengudroid/bridge/libberberis_arm64.so").is_file())

    def test_bundled_modules_do_not_auto_select_a_user_legacy_sysroot(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        store = self.fake / "store"
        store.mkdir()
        (store / "runtime-modules.json").write_text('{"format":2}')
        result = run("--skip-build", "--no-image", "--with-modules", str(store))
        self.assertEqual(result.returncode, 0, result.stderr)
        cli = self.appdir / "usr/bin/pengudroid"
        cli.write_text('#!/bin/sh\nprintf "%s" "${PENGUDROID_SYSROOT-unset}"\n')
        data = self.fake / "data"
        (data / "pengudroid/sysroot").mkdir(parents=True)
        env = {k:v for k,v in os.environ.items() if not k.startswith("PENGUDROID_")}
        env["XDG_DATA_HOME"] = str(data)
        observed = subprocess.run(["sh", str(self.appdir / "AppRun"), "--cli", "--version"],
            env=env, capture_output=True, text=True)
        self.assertEqual(observed.returncode, 0, observed.stderr)
        self.assertEqual(observed.stdout, "unset")
        env["PENGUDROID_SYSROOT"] = "/explicit/override"
        observed = subprocess.run(["sh", str(self.appdir / "AppRun"), "--cli", "--version"],
            env=env, capture_output=True, text=True)
        self.assertEqual(observed.stdout, "/explicit/override")

    def test_bundled_cli_reports_its_version(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        run("--skip-build", "--no-image")
        binary = self.appdir / "usr/bin/pengudroid"
        env = dict(os.environ)
        # The CLI looks for its host libraries beside itself.
        env["LD_LIBRARY_PATH"] = str(binary.parent)
        result = subprocess.run(
            [str(binary), "--version"], capture_output=True, text=True, env=env
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pengudroid", result.stdout.lower())


if __name__ == "__main__":
    unittest.main()
