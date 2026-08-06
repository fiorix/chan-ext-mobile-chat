from __future__ import annotations

import importlib.util
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = REPO_ROOT / "scripts/package-chan-extension.py"
SPEC = importlib.util.spec_from_file_location("package_chan_extension", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
PACKAGE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = PACKAGE
SPEC.loader.exec_module(PACKAGE)


class PackageChanExtensionTests(unittest.TestCase):
    def build(self, target: str) -> Path:
        temporary = self.enterContext(tempfile.TemporaryDirectory())
        root = Path(temporary)
        binary = root / PACKAGE.TARGETS[target].executable
        binary.write_bytes(b"test binary")
        return PACKAGE.build_package(REPO_ROOT, target, binary, root / "dist")

    def test_unix_archive_carries_everything_the_installer_requires(self) -> None:
        archive = self.build("linux-x86_64")
        with tarfile.open(archive, "r:gz") as bundle:
            names = set(bundle.getnames())
            executable = bundle.getmember("mobile-chat/mobile-chat-extension")
            self.assertEqual(executable.mode, 0o755, "the binary must stay executable")
        for required in PACKAGE.PAYLOAD_FILES:
            self.assertIn(f"mobile-chat/{required}", names)

    def test_windows_archive_uses_the_exe_name(self) -> None:
        archive = self.build("windows-x86_64")
        with zipfile.ZipFile(archive) as bundle:
            names = set(bundle.namelist())
        self.assertIn("mobile-chat/mobile-chat-extension.exe", names)
        for required in PACKAGE.PAYLOAD_FILES:
            self.assertIn(f"mobile-chat/{required}", names)

    def test_every_target_is_named_for_its_platform(self) -> None:
        # install.sh selects an archive by these exact names.
        self.assertEqual(
            {name: target.archive for name, target in PACKAGE.TARGETS.items()},
            {
                "linux-x86_64": "mobile-chat-linux-x86_64.tar.gz",
                "linux-aarch64": "mobile-chat-linux-aarch64.tar.gz",
                "windows-x86_64": "mobile-chat-windows-x86_64.zip",
                "macos-aarch64": "mobile-chat-macos-aarch64.tar.gz",
            },
        )

    def test_the_declaration_in_the_archive_matches_what_chan_accepts(self) -> None:
        # A field Chan does not know silently drops the whole extension, so the
        # shipped template has to stay within its four fields.
        import tomllib

        declaration = (
            REPO_ROOT / "packaging/chan-extension/mobile-chat.toml"
        ).read_bytes()
        parsed = tomllib.loads(declaration.decode())
        self.assertEqual(set(parsed), {"name", "command", "args", "capabilities"})
        self.assertEqual(parsed["name"], "Mobile Chat")
        self.assertEqual(parsed["capabilities"], ["session-context"])

    def test_archive_is_reproducible(self) -> None:
        first = self.build("linux-x86_64").read_bytes()
        second = self.build("linux-x86_64").read_bytes()
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()
