import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('deploy', Path(__file__).with_name('deploy.py'))
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


class DeploymentTests(unittest.TestCase):
    def test_restricted_command(self):
        valid = 'deploy ' + deploy.IMAGE + '@sha256:' + 'a' * 64 + ' ' + 'b' * 40
        self.assertEqual(deploy.parse_command(valid)[1], 'b' * 40)
        for command in ('id', valid + '; id', valid + '\n', valid + ' extra',
                        valid.replace('david-yang-pixel', 'other'), ''):
            with self.subTest(command=command), self.assertRaises(ValueError):
                deploy.parse_command(command)

    def test_failed_health_restores_compose_and_does_not_rewind_data(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'app'
            root.mkdir()
            original = 'services:\n  antigravity-manager:\n    image: old@sha256:abc\n    volumes:\n      - ./data:/root/.antigravity_tools\n'
            (root / 'compose.yaml').write_text(original)
            (root / '.env').write_text('API_KEY=test')
            (root / 'machine-id').write_text('retained identity')
            (root / 'data').mkdir()
            sentinel = root / 'data/keep'
            sentinel.write_text('new usage must survive rollback')
            with patch.object(deploy, 'ROOT', root), patch.object(deploy, 'BACKUPS', Path(tmp) / 'backups'), \
                    patch.object(deploy, 'run') as run, patch.object(deploy, 'compose') as compose, \
                    patch.object(deploy.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                    patch.object(deploy, 'inspect_image', return_value={'Config': {'Labels': {'org.opencontainers.image.revision': 'b' * 40}}}), \
                    patch.object(deploy, 'verify', side_effect=RuntimeError('unhealthy')):
                with self.assertRaisesRegex(RuntimeError, 'unhealthy'):
                    deploy.deploy(deploy.IMAGE + '@sha256:' + 'a' * 64, 'b' * 40, 'temporary-token')
                self.assertEqual((root / 'compose.yaml').read_text(), original)
                self.assertEqual(sentinel.read_text(), 'new usage must survive rollback')
                self.assertFalse((root / 'last-deployment.json').exists())
                self.assertEqual(compose.call_count, 3)  # stop, new image, previous image
                self.assertTrue(any(call.args[0][0] == 'tar' for call in run.call_args_list))


if __name__ == '__main__':
    unittest.main()
