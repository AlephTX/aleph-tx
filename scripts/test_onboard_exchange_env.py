#!/usr/bin/env python3
"""Unit tests for `scripts/onboard_exchange_env.py` using synthetic non-secret fixtures."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.onboard_exchange_env import audit_env_dict, parse_dotenv_with_duplicates, split_env_file

REPO_ROOT = Path(__file__).resolve().parent.parent


class OnboardExchangeEnvTests(unittest.TestCase):
    def test_audit_classifies_ready_partial_empty_invalid_and_never_leaks_secrets(self) -> None:
        priv = "PRIVATE_" + "KEY"
        sec = "SECRET_" + "KEY"
        hl_hex = "ab" * 32
        arc_hex = "cd" * 32
        lgt_hex = "ef" * 40
        with tempfile.TemporaryDirectory() as tmp:
            env_path = Path(tmp) / ".env.synthetic"
            env_path.write_text(
                "\n".join([
                    "ARCUS_ACCOUNT_ADDRESS=0x1111111111111111111111111111111111111111",
                    "ARCUS_ACCOUNT_INDEX=1",
                    f"ARCUS_API_{priv}={arc_hex}",
                    "HYPERLIQUID_ACCOUNT_ADDRESS=0x2222222222222222222222222222222222222222",
                    "HYPERLIQUID_VAULT_ADDRESS=0x3333333333333333333333333333333333333333",
                    f"HYPERLIQUID_AGENT_{priv}={hl_hex}",
                    "LIGHTER_ACCOUNT_INDEX=123",
                    "LIGHTER_API_KEY_INDEX=7",
                    f"LIGHTER_{priv}={lgt_hex}",
                    "BULK_ACCOUNT_PUBKEY=invalid!pubkey!",
                    "EXTENDED_API_KEY=ext-only-one",
                    f"BACKPACK_{sec}=not_printed_bp_value",
                ]) + "\n",
                encoding="utf-8",
            )
            env, dups = parse_dotenv_with_duplicates(env_path)
            rep = audit_env_dict(env, dups, env_path=str(env_path))
            self.assertEqual(rep["venues"]["arcus"]["status"], "ready")
            self.assertEqual(rep["venues"]["hyperliquid"]["status"], "ready")
            self.assertEqual(rep["venues"]["lighter_core"]["status"], "ready")
            self.assertEqual(rep["venues"]["bulk"]["status"], "invalid")
            self.assertEqual(rep["venues"]["extended"]["status"], "partial")
            self.assertEqual(rep["venues"]["okx"]["status"], "empty")
            self.assertTrue(rep["split_eligible"]["arcus_xyz_basis.env"])
            self.assertTrue(rep["split_eligible"]["live.env"])

            proc = subprocess.run(
                [sys.executable, str(REPO_ROOT / "scripts" / "onboard_exchange_env.py"), "audit", "--env", str(env_path)],
                capture_output=True,
                text=True,
                check=True,
            )
            for secret_frag in (arc_hex, hl_hex, lgt_hex, "ext-only-one", "not_printed_bp_value"):
                self.assertNotIn(secret_frag, proc.stdout)

    def test_split_preserves_existing_dir_mode_and_supports_0640(self) -> None:
        priv = "PRIVATE_" + "KEY"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            env_path = root / ".env.unified"
            out_dir = root / "etc_aleph"
            env_path.write_text(
                "\n".join([
                    "ARCUS_ACCOUNT_ADDRESS=0x1111111111111111111111111111111111111111",
                    "ARCUS_ACCOUNT_INDEX=0",
                    f"ARCUS_API_{priv}=" + "aa" * 32,
                    "HYPERLIQUID_ACCOUNT_ADDRESS=0x2222222222222222222222222222222222222222",
                    "HYPERLIQUID_VAULT_ADDRESS=0x3333333333333333333333333333333333333333",
                    f"HYPERLIQUID_AGENT_{priv}=" + "bb" * 32,
                    "LIGHTER_ACCOUNT_INDEX=42",
                    "OKX_API_KEY=okx-should-never-appear-in-splits",
                    "EXTENDED_API_KEY=ext-should-never-appear-in-splits",
                ]) + "\n",
                encoding="utf-8",
            )
            out_dir.mkdir()
            os.chmod(out_dir, 0o755)
            (out_dir / "monitor.env").write_text("ALEPH_MONITOR_CONTROL_TOKEN=keep-tok\n", encoding="utf-8")

            res = split_env_file(env_path, out_dir, trading_mode=0o640)
            # Pre-existing directory permissions must NOT be clobbered to 0700.
            self.assertEqual(os.stat(out_dir).st_mode & 0o777, 0o755)
            self.assertIn("arcus_xyz_basis.env", res["written"])
            self.assertIn("monitor.env", res["written"])
            self.assertIn("live.env", res["skipped"])

            arc_file = out_dir / "arcus_xyz_basis.env"
            mon_file = out_dir / "monitor.env"
            self.assertEqual(os.stat(arc_file).st_mode & 0o777, 0o640)
            self.assertEqual(os.stat(mon_file).st_mode & 0o777, 0o600)

            arc_txt = arc_file.read_text(encoding="utf-8")
            self.assertIn("HYPERLIQUID_VAULT_ADDRESS=0x3333333333333333333333333333333333333333", arc_txt)
            self.assertNotIn("OKX_API_KEY", arc_txt)
            self.assertNotIn("EXTENDED_API_KEY", arc_txt)

            mon_txt = mon_file.read_text(encoding="utf-8")
            self.assertIn("ALEPH_MONITOR_CONTROL_TOKEN=keep-tok", mon_txt)
            self.assertIn("ALEPH_MONITOR_HL_XYZ_ADDRESS=0x3333333333333333333333333333333333333333", mon_txt)
            self.assertNotIn("PRIVATE", mon_txt)
            self.assertNotIn("aa" * 16, mon_txt)
            self.assertNotIn("bb" * 16, mon_txt)

            # Brand-new directory defaults to 0700 and 0600 files; world-readable mode fails closed.
            new_dir = root / "fresh_out"
            res2 = split_env_file(env_path, new_dir)
            self.assertEqual(os.stat(new_dir).st_mode & 0o777, 0o700)
            self.assertEqual(os.stat(new_dir / "arcus_xyz_basis.env").st_mode & 0o777, 0o600)
            self.assertEqual(res2["written"]["arcus_xyz_basis.env"]["mode"], "0600")
            with self.assertRaises(ValueError):
                split_env_file(env_path, new_dir, trading_mode=0o644)


if __name__ == "__main__":
    unittest.main()
