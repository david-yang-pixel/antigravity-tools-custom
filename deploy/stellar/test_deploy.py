import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('deploy', Path(__file__).with_name('deploy.py'))
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


LABELS = {'org.opencontainers.image.revision': 'b' * 40, 'org.opencontainers.image.version': '4.10.0-beta.1'}


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
                    patch.object(deploy, 'inspect_image', return_value={'Config': {'Labels': LABELS}}), \
                    patch.object(deploy, 'verify', side_effect=RuntimeError('unhealthy')):
                with self.assertRaisesRegex(RuntimeError, 'unhealthy'):
                    deploy.deploy(deploy.IMAGE + '@sha256:' + 'a' * 64, 'b' * 40, 'temporary-token')
                self.assertEqual((root / 'compose.yaml').read_text(), original)
                self.assertEqual(sentinel.read_text(), 'new usage must survive rollback')
                self.assertFalse((root / 'last-deployment.json').exists())
                self.assertEqual(compose.call_count, 3)  # stop, new image, previous image
                self.assertTrue(any(call.args[0][0] == 'tar' for call in run.call_args_list))

    def test_image_version_label_is_required_and_checked_against_health(self):
        self.assertEqual(deploy.image_version(LABELS), '4.10.0-beta.1')
        for labels in ({}, {'org.opencontainers.image.version': ''},
                       {'org.opencontainers.image.version': '4.9'}, {'org.opencontainers.image.version': '4.9.6 x'}):
            with self.subTest(labels=labels), self.assertRaisesRegex(RuntimeError, 'version label'):
                deploy.image_version(labels)

    def test_unlabelled_image_is_refused_before_service_stops(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'app'
            root.mkdir()
            (root / 'compose.yaml').write_text('services:\n  antigravity-manager:\n    image: old@sha256:abc\n')
            unlabelled = {k: v for k, v in LABELS.items() if k != 'org.opencontainers.image.version'}
            with patch.object(deploy, 'ROOT', root), patch.object(deploy, 'BACKUPS', Path(tmp) / 'backups'), \
                    patch.object(deploy, 'run'), patch.object(deploy, 'compose') as compose, \
                    patch.object(deploy.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                    patch.object(deploy, 'inspect_image', return_value={'Config': {'Labels': unlabelled}}):
                with self.assertRaisesRegex(RuntimeError, 'version label'):
                    deploy.deploy(deploy.IMAGE + '@sha256:' + 'a' * 64, 'b' * 40, 'temporary-token')
                compose.assert_not_called()
                self.assertFalse((Path(tmp) / 'backups').exists())

    def test_health_must_report_the_image_version(self):
        with patch.object(deploy, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='[{"Image": "id", "State": {"Health": {"Status": "healthy"}}}]')), \
                patch.object(deploy, 'inspect_image', return_value={'Id': 'id'}), \
                patch.object(deploy, 'request', return_value={'version': '4.9.6'}):
            with self.assertRaisesRegex(RuntimeError, 'Unexpected application version'):
                deploy.verify(deploy.IMAGE + '@sha256:' + 'a' * 64, '4.10.0-beta.1')


if __name__ == '__main__':
    unittest.main()
