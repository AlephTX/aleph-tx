import gzip
import json
from pathlib import Path
import tempfile
import unittest

from inventory_public_sources import inventory_public_sources


class PublicInventoryTests(unittest.TestCase):
    def test_strategy_independent_campaigns_and_exact_metadata_are_discoverable(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for prefix in ("hyperliquid-public-10h", "extended-lighter-index-10h", "public-venues-10h"):
                campaign = root / (prefix + "-synthetic")
                metadata = campaign / "metadata"
                metadata.mkdir(parents=True)
                (campaign / "public.ndjson.gz").write_bytes(prefix.encode())
                (campaign / "public.ndjson.gz.partial").write_bytes(b"active")
                (metadata / "lighter").write_bytes(b"synthetic public market terms")
                (metadata / "account.json").write_bytes(b"excluded")
            report = inventory_public_sources(root)
            self.assertEqual(report["unique_archives"], 3)
            self.assertEqual(report["unique_metadata"], 1)
            self.assertEqual(report["skipped_counts"]["incomplete_archive"], 3)
            self.assertEqual(report["skipped_counts"]["private_indicating_name"], 3)

    def test_hip3_market_metadata_has_distinct_venue_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            metadata = root / "public-venues-synthetic" / "metadata"
            metadata.mkdir(parents=True)
            for dex in ('xyz', 'io'):
                (metadata / f'{dex}-markets.json').write_text(json.dumps({'dex': dex}))
            report = inventory_public_sources(root)
            self.assertEqual(report['unique_metadata'], 2)
            self.assertEqual({item['source_family_hint'] for item in report['artifacts']},
                             {'entropy', 'trade_xyz'})

    def test_known_archives_are_deduplicated_without_counting_unknown_quality(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            raw = root / "2026-11-12-public" / "raw"
            raw.mkdir(parents=True)
            content = gzip.compress(b'{"synthetic":true}\n', mtime=0)
            (raw / "lighter-trades-001.jsonl.gz").write_bytes(content)
            (raw / "lighter-trades-copy.jsonl.gz").write_bytes(content)
            campaign = root / "robinhood-12h-synthetic"
            campaign.mkdir()
            (campaign / "public.ndjson.zst").write_bytes(b"synthetic fixture, no zstd decoding required")
            report = inventory_public_sources(root)
            self.assertEqual(report["unique_archives"], 2)
            self.assertEqual(report["archive_path_count"], 3)
            self.assertEqual(report["duplicate_aliases"], 1)
            self.assertEqual(report["artifacts"][0]["source_family_hint"], "lighter")
            self.assertEqual(len(report["artifacts"][0]["aliases"]), 1)
            self.assertTrue(all(a["coverage"] == "unknown" for a in report["artifacts"]))
            self.assertEqual(report["inventory_sha256"], inventory_public_sources(root)["inventory_sha256"])

    def test_private_names_active_files_unknown_locations_and_symlinks_are_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            campaign = root / "extended-lighter-synthetic"
            raw = campaign / "raw"
            raw.mkdir(parents=True)
            for name in ("lighter-account-001.jsonl.gz", "lighter-private-001.zst",
                         "capture-001.ndjson.partial", "note.txt"):
                (raw / name).write_bytes(b"synthetic")
            for name in ("results", "source", "unknown"):
                folder = campaign / name
                folder.mkdir()
                (folder / "capture-001.ndjson.zst").write_bytes(b"synthetic")
            other = root / "private-campaign"
            other.mkdir()
            (other / "public.ndjson.gz").write_bytes(b"synthetic")
            (raw / "lighter-linked.jsonl.gz").symlink_to(other / "public.ndjson.gz")
            report = inventory_public_sources(root)
            self.assertEqual(report["unique_archives"], 0)
            self.assertEqual(report["skipped_counts"]["private_indicating_name"], 2)
            self.assertEqual(report["skipped_counts"]["incomplete_archive"], 1)
            self.assertEqual(report["skipped_counts"]["excluded_campaign_directory"], 3)
            self.assertEqual(report["skipped_counts"]["symlink"], 1)

    def test_only_capture_names_in_segments_and_campaign_root_are_admitted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            campaign = root / "2026-11-13-public"
            segments = campaign / "segments"
            segments.mkdir(parents=True)
            (segments / "capture-001.ndjson.zst").write_bytes(b"one")
            (segments / "arbitrary.ndjson.zst").write_bytes(b"two")
            (campaign / "capture-002.ndjson.zst").write_bytes(b"three")
            (campaign / "private.json.gz").write_bytes(b"four")
            report = inventory_public_sources(root)
            self.assertEqual(report["unique_archives"], 2)
            self.assertTrue(all(a["source_family_hint"] == "mixed_or_unknown" for a in report["artifacts"]))

    def test_exact_public_market_and_fee_metadata_names_are_indexed_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            campaign = root / "extended-lighter-synthetic"
            metadata = campaign / "metadata-000"
            metadata.mkdir(parents=True)
            for name in ("lighter", "extended", "extended_fees", "lighter_fees"):
                (metadata / name).write_bytes(name.encode())
            for name in ("metadata-000-core.json", "metadata-000-robinhood.json", "lighter-metadata.json",
                         "lighter_robinhood-metadata.json", "extended-metadata.json"):
                (campaign / name).write_bytes(name.encode())
            # Repeated snapshots remain one canonical digest, with a path alias.
            later = campaign / "metadata-016"
            later.mkdir()
            (later / "lighter_fees").write_bytes(b"lighter_fees")
            robinhood = root / "robinhood-public-synthetic"
            robinhood.mkdir()
            (robinhood / "markets.json").write_bytes(b"robinhood markets")
            (campaign / "capture-000.ndjson.zst").write_bytes(b"public book")
            report = inventory_public_sources(root)
            self.assertEqual(report["schema_version"], 2)
            self.assertEqual(report["unique_archives"], 1)
            self.assertEqual(report["unique_metadata"], 10)
            self.assertEqual(report["unique_artifacts"], 11)
            self.assertEqual(report["metadata_path_count"], 11)
            self.assertEqual(report["duplicate_aliases"], 1)
            fees = [a for a in report["artifacts"] if a.get("metadata_kind") == "fees"]
            self.assertEqual(len(fees), 2)
            self.assertTrue(all(a["format"] == "public_metadata" and a["coverage"] == "unknown" for a in fees))

    def test_metadata_allowlist_does_not_expand_to_account_or_compiled_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            campaign = root / "2026-11-13-public"
            metadata = campaign / "metadata-000"
            metadata.mkdir(parents=True)
            for name in ("lighter_account", "balance", "arbitrary.json"):
                (metadata / name).write_bytes(b"synthetic")
            for name in ("markets.json", "metadata-000-unknown.json", "custom-metadata.json"):
                (campaign / name).write_bytes(b"synthetic")
            results = campaign / "results"
            results.mkdir()
            (results / "lighter-metadata.json").write_bytes(b"synthetic")
            self.assertEqual(inventory_public_sources(root)["unique_artifacts"], 0)


if __name__ == "__main__":
    unittest.main()
