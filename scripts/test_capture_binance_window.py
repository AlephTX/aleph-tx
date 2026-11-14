import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from capture_binance_window import digest, run


class CaptureWindowTests(unittest.TestCase):
    def test_protected_window_and_binary_mismatch_cannot_start_a_process(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / 'perp-capture'
            binary.write_text('synthetic never executed')
            protected = root / 'protected.json'
            protected.write_text('{}')
            base = {'collector_sha256': digest(binary), 'capture_from_ms': 100_000,
                    'capture_until_ms': 36_300_000, 'protected_until_ms': 99_999,
                    'validation_start_ms': 200_000, 'validation_end_ms': 36_200_000,
                    'disk_reserve_bytes': 14 * 2**30, 'protected_policy': str(protected),
                    'protected_policy_sha256': digest(protected)}
            for overrides in ({'protected_until_ms': 100_000}, {'collector_sha256': 'bad'},
                              {'disk_reserve_bytes': 0}, {'validation_end_ms': 36_199_999}):
                (root / 'capture-plan.json').write_text(json.dumps(base | overrides))
                with patch('capture_binance_window.subprocess.Popen') as process:
                    with self.assertRaises(ValueError):
                        run(root)
                    process.assert_not_called()
            self.assertEqual(protected.read_text(), '{}')

    def test_missed_warmup_is_a_failed_unit_not_a_shifted_window(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            binary = root / 'perp-capture'
            binary.write_text('synthetic never executed')
            protected = root / 'protected.json'
            protected.write_text('{}')
            plan = {'collector_sha256': digest(binary), 'capture_from_ms': 100_000,
                    'capture_until_ms': 36_300_000, 'protected_until_ms': 99_999,
                    'validation_start_ms': 200_000, 'validation_end_ms': 36_200_000,
                    'disk_reserve_bytes': 14 * 2**30, 'protected_policy': str(protected),
                    'protected_policy_sha256': digest(protected)}
            (root / 'capture-plan.json').write_text(json.dumps(plan))
            with patch('capture_binance_window.subprocess.Popen') as process, \
                    patch('capture_binance_window.time.time_ns', return_value=141_000 * 1_000_000):
                with self.assertRaisesRegex(ValueError, 'warmup'):
                    run(root)
                process.assert_not_called()
            self.assertTrue((root / 'capture-driver.lock').exists())
            self.assertEqual(json.loads((root / 'capture-plan.json').read_text()), plan)


if __name__ == '__main__':
    unittest.main()
