from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'prepare-tempo-xtask.sh'


class PrepareTempo(unittest.TestCase):
    def test_native_support_is_idempotent_in_both_layouts(self):
        for relative in ('crates/state-bloat/src/generate.rs', 'xtask/src/generate_state_bloat.rs'):
            with self.subTest(layout=relative), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for name in ('xtask/src/genesis_args.rs', relative):
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text('pub mnemonic_file: Option<PathBuf>,\n')
                before = {p.relative_to(root): p.read_bytes() for p in root.rglob('*.rs')}
                for _ in range(2):
                    result = subprocess.run([str(SCRIPT), directory], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn('already supports', result.stdout)
                self.assertEqual(before, {p.relative_to(root): p.read_bytes() for p in root.rglob('*.rs')})

    def test_unknown_layout_fails_without_partial_edit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(['git', 'init', '-q', directory], check=True)
            source = root / 'xtask/src/genesis_args.rs'
            source.parent.mkdir(parents=True)
            source.write_text('unsupported source\n')
            result = subprocess.run([str(SCRIPT), directory], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(source.read_text(), 'unsupported source\n')


if __name__ == '__main__':
    unittest.main()
