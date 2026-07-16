from __future__ import annotations

import importlib.util
import struct
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "package_agentrouter_core_macos.py"
SPEC = importlib.util.spec_from_file_location("package_agentrouter_core_macos", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PackageAgentRouterCoreMacosTests(unittest.TestCase):
    def test_parses_arm64_macho_header(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / "codex"
            binary.write_bytes(b"\xcf\xfa\xed\xfe" + struct.pack("<I", 0x0100000C))
            self.assertEqual(MODULE.macho_cpu_type(binary), 0x0100000C)

    def test_rejects_non_macho_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            binary = Path(temp) / "codex"
            binary.write_bytes(b"not-macho")
            with self.assertRaisesRegex(ValueError, "not a thin 64-bit Mach-O"):
                MODULE.macho_cpu_type(binary)

    def test_sanitizes_component_version(self) -> None:
        self.assertEqual(
            MODULE.sanitize_version("0.145.0 agentrouter/4"), "0.145.0-agentrouter-4"
        )


if __name__ == "__main__":
    unittest.main()
