import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('deploy', Path(__file__).with_name('deploy.py'))
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


LABELS = {'org.opencontainers.image.revision': 'b' * 40, 'org.opencontainers.image.version': '4.10.0-beta.1'}


class PortalVerificationTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        (self.root / 'data').mkdir()
        (self.root / '.env').write_text('ABV_WEB_PASSWORD=test-admin')
        (self.root / 'data/gui_config.json').write_text(json.dumps({'proxy': {}}))
        self.columns = {'max_concurrency', 'balance_micros', 'free_models'}
        self.overview = {
            'logs': {'total': 0}, 'daily': [], 'weekly': [], 'available_models': [],
            'model_prices': [], 'max_concurrency': 2, 'balance_micros': 0, 'free_models': [],
        }
        self.registry = {'published_models': [{'public_id': 'catalog', 'enabled': True}]}
        self.prices = []
        self.free = None

    def verify(self, enabled=True, first_registry=False):
        with sqlite3.connect(self.root / 'data/user_tokens.db') as db:
            db.execute('DROP TABLE IF EXISTS user_tokens')
            db.execute('DROP TABLE IF EXISTS model_prices')
            columns = ', '.join(sorted(self.columns))
            db.execute(f'CREATE TABLE user_tokens (token TEXT, enabled INTEGER, {columns})')
            values = {'token': 'test-user', 'enabled': enabled}
            if 'free_models' in self.columns:
                values['free_models'] = self.free
            db.execute(f'INSERT INTO user_tokens ({", ".join(values)}) VALUES ({", ".join("?" * len(values))})',
                       tuple(values.values()))
            if self.prices:
                db.execute('CREATE TABLE model_prices (pattern TEXT PRIMARY KEY)')
                db.executemany('INSERT INTO model_prices VALUES (?)', [(p,) for p in self.prices])

        def request(base, path, expected=200, key=None):
            if expected == 401:
                self.assertIsNone(key)
                return {}
            if path == '/health':
                return {'version': '4.10.0-beta.1'}
            if path == '/usage':
                return b'<html>portal</html>'
            if path == '/api/user-tokens':
                self.assertEqual(key, 'test-admin')
                return []
            if path == '/api/model-registry':
                self.assertEqual(key, 'test-admin')
                return self.registry
            self.assertEqual(path, '/api/token-portal/me')
            self.assertEqual(key, 'test-user')
            return self.overview

        with patch.object(deploy, 'ROOT', self.root), \
                patch.object(deploy, 'run', return_value=subprocess.CompletedProcess([], 0, stdout='[{"Image": "id", "State": {"Health": {"Status": "healthy"}}}]')), \
                patch.object(deploy, 'inspect_image', return_value={'Id': 'id'}), \
                patch.object(deploy, 'request', side_effect=request) as requests:
            deploy.verify(deploy.IMAGE + '@sha256:' + 'a' * 64, '4.10.0-beta.1', first_registry)
            return requests.call_args_list

    def test_current_schema_and_portal_need_no_legacy_fields(self):
        self.verify()

    def test_migrated_database_can_keep_legacy_columns(self):
        self.columns.update({'allowed_models', 'token_quota'})
        self.verify()

    def test_current_billing_columns_are_required(self):
        for field in sorted(self.columns):
            with self.subTest(field=field):
                self.columns.remove(field)
                with self.assertRaisesRegex(RuntimeError, 'schema migration missing'):
                    self.verify()
                self.columns.add(field)

    def test_current_portal_fields_are_required(self):
        for field in list(self.overview):
            with self.subTest(field=field):
                value = self.overview.pop(field)
                with self.assertRaisesRegex(RuntimeError, 'Portal response incomplete'):
                    self.verify()
                self.overview[field] = value

    def test_balance_cannot_opt_out_and_free_models_must_be_a_list(self):
        for field, value in [('balance_micros', None), ('balance_micros', True),
                             ('balance_micros', '0'), ('free_models', None),
                             ('free_models', 'model'), ('free_models', [1])]:
            with self.subTest(field=field, value=value):
                previous = self.overview[field]
                self.overview[field] = value
                with self.assertRaisesRegex(RuntimeError, 'Portal billing response invalid'):
                    self.verify()
                self.overview[field] = previous

    def test_negative_balance_is_valid_after_post_request_settlement(self):
        self.overview['balance_micros'] = -1
        self.verify()

    def test_model_registry_must_load_with_enabled_publications(self):
        for registry in [None, {}, {'published_models': []},
                         {'published_models': [{'public_id': 'catalog', 'enabled': False}]}]:
            with self.subTest(registry=registry):
                self.registry = registry
                with self.assertRaisesRegex(RuntimeError, 'Model registry'):
                    self.verify()

    def test_first_cutover_must_keep_exact_priced_and_free_models(self):
        self.prices = ['catalog', 'models/priced', 'wild-*', 'bad id']
        self.free = json.dumps(['free'])
        self.verify()  # Later deployments respect deliberate unpublication.
        with self.assertRaisesRegex(RuntimeError, r'dropped 2 billable model\(s\): free, priced'):
            self.verify(first_registry=True)
        self.registry['published_models'] += [{'public_id': 'priced', 'enabled': True},
                                              {'public_id': 'free', 'enabled': True}]
        self.verify(first_registry=True)

    def test_no_enabled_user_does_not_skip_schema_or_require_a_test_user(self):
        calls = self.verify(enabled=False)
        self.assertFalse(any(c.args[1] == '/api/token-portal/me' and c.kwargs.get('key') for c in calls))


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

    def test_failed_cutover_sets_aside_only_the_registry_it_created(self):
        for existed in (False, True):
            with self.subTest(existed=existed), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp) / 'app'
                root.mkdir()
                (root / 'compose.yaml').write_text(
                    'services:\n  antigravity-manager:\n    image: old@sha256:abc\n    volumes:\n      - ./data:/root/.antigravity_tools\n')
                (root / '.env').write_text('API_KEY=test')
                (root / 'machine-id').write_text('retained identity')
                (root / 'data').mkdir()
                registry = root / deploy.REGISTRY
                if existed:
                    registry.write_text('{"kept": true}')

                def failed_verify(*args):
                    registry.write_text('{"created": true}')
                    raise RuntimeError('dropped billable models')

                with patch.object(deploy, 'ROOT', root), patch.object(deploy, 'BACKUPS', Path(tmp) / 'backups'), \
                        patch.object(deploy, 'run'), patch.object(deploy, 'compose'), \
                        patch.object(deploy.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                        patch.object(deploy, 'inspect_image', return_value={'Config': {'Labels': LABELS}}), \
                        patch.object(deploy, 'verify', side_effect=failed_verify):
                    with self.assertRaisesRegex(RuntimeError, 'dropped'):
                        deploy.deploy(deploy.IMAGE + '@sha256:' + 'a' * 64, 'b' * 40, 'temporary-token')
                backups = list((Path(tmp) / 'backups').glob('*/model_registry.json'))
                self.assertEqual(registry.exists(), existed)
                self.assertEqual(len(backups), 0 if existed else 1)

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
