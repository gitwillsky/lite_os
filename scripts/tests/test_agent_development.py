from __future__ import annotations

import importlib
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import agent_cli_cache  # noqa: E402
import prepare_agent_development  # noqa: E402


class AgentDevelopmentTests(unittest.TestCase):
    def test_fixed_aarch64_artifacts_never_use_latest(self) -> None:
        self.assertEqual(agent_cli_cache.CODEX_VERSION, "0.145.0")
        self.assertEqual(agent_cli_cache.CLAUDE_VERSION, "2.1.212-r1")
        self.assertNotIn("latest", agent_cli_cache.CODEX_ARCHIVE_URL)
        self.assertNotIn("latest", agent_cli_cache.CLAUDE_APK_URL)
        self.assertEqual(len(agent_cli_cache.CODEX_ARCHIVE_SHA256), 64)
        self.assertEqual(len(agent_cli_cache.CLAUDE_APK_SHA256), 64)
        self.assertEqual(len(agent_cli_cache.CLAUDE_KEY_SHA256), 64)
        self.assertEqual(len(agent_cli_cache.CLAUDE_INDEX_SHA256), 64)
        self.assertIn(
            f"generation={agent_cli_cache.CLAUDE_INDEX_GENERATION}",
            agent_cli_cache.CLAUDE_INDEX_URL,
        )

    def test_agent_alpine_dependency_versions_and_digests_are_fixed(self) -> None:
        packages = agent_cli_cache.AGENT_ALPINE_PACKAGES
        self.assertEqual(
            {name for _, name, _ in packages},
            {
                "bash-5.2.37-r0.apk",
                "libgcc-14.2.0-r6.apk",
                "libstdc++-14.2.0-r6.apk",
                "ripgrep-14.1.1-r0.apk",
            },
        )
        self.assertTrue(
            all(repository in {"main", "community"} for repository, _, _ in packages)
        )
        self.assertTrue(all(len(digest) == 64 for _, _, digest in packages))
        self.assertEqual(
            agent_cli_cache._package_identity("libstdc++-14.2.0-r6.apk"),
            ("libstdc++", "14.2.0-r6"),
        )

    def test_riscv64_agent_assets_fail_before_download(self) -> None:
        with patch.dict(os.environ, {"ARCH": "riscv64", "ACCEL": "tcg"}, clear=True):
            module = importlib.reload(agent_cli_cache)
            with self.assertRaisesRegex(RuntimeError, "only ARCH=aarch64"):
                module.ensure_supported_target()
        with patch.dict(os.environ, {"ARCH": "aarch64", "ACCEL": "hvf"}, clear=True):
            importlib.reload(agent_cli_cache)

    def test_install_payload_binds_script_and_artifact_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            codex = directory / "codex"
            key = directory / "key"
            index = directory / "index"
            apk = directory / "claude.apk"
            codex.write_bytes(b"codex")
            key.write_bytes(b"key")
            index.write_bytes(b"index")
            apk.write_bytes(b"apk")
            artifacts = agent_cli_cache.AgentCliArtifacts(
                codex_binary=codex,
                claude_key=key,
                claude_index=index,
                claude_apk=apk,
                alpine_apks=(),
                fingerprint="fixture",
            )
            payload = prepare_agent_development.installation_payload(artifacts)

        self.assertEqual(payload["kind"], "agent-development-image")
        self.assertIn("install_script_sha256", payload)
        self.assertEqual(payload["artifacts"]["arch"], "aarch64")

    def test_development_defaults_leave_product_runtime_size_unchanged(self) -> None:
        self.assertEqual(prepare_agent_development.DEFAULT_IMAGE_SIZE_MIB, 32768)
        self.assertEqual(prepare_agent_development.DEFAULT_QEMU_MEMORY, "6G")


if __name__ == "__main__":
    unittest.main()
