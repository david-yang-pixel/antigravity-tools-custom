from datetime import datetime, timezone
import importlib.util
import io
from pathlib import Path
import unittest
import urllib.error

spec = importlib.util.spec_from_file_location('tag_deployments', Path(__file__).with_name('tag_deployments.py'))
tags = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tags)

IMAGE = 'ghcr.io/david-yang-pixel/antigravity-tools-custom'
NOW = datetime(2026, 10, 18, 12, 0, tzinfo=timezone.utc)


def job(run_id, sha, completed_at, name='deploy', conclusion='success', attempt=1):
    return {'run_id': run_id, 'head_sha': sha, 'completed_at': completed_at,
            'name': name, 'conclusion': conclusion, 'run_attempt': attempt}


# Production history before the tag job existed (runs 1-2 were cancelled before deploying).
HISTORY = {
    1: [job(1, 'df491ff' + '0' * 33, '2026-10-14T08:03:41Z', conclusion='cancelled')],
    2: [job(2, '723aad3' + '0' * 33, '2026-10-14T08:30:38Z', conclusion='cancelled'),
        job(2, '723aad3' + '0' * 33, '2026-10-14T08:05:53Z', name='Quality checks')],
    3: [job(3, '0292fc9' + '0' * 33, '2026-10-14T09:10:08Z')],
    4: [job(4, 'f85148f' + '0' * 33, '2026-10-14T11:07:17Z')],
    5: [job(5, '4698f17' + '0' * 33, '2026-10-15T08:58:26Z')],
    6: [job(6, 'd2db740' + '0' * 33, '2026-10-15T09:26:40Z')],
    7: [job(7, 'cc45ac6' + '0' * 33, '2026-10-17T03:31:34Z')],
}


class FakeGitHub:
    repo = 'david-yang-pixel/antigravity-tools-custom'

    def __init__(self, runs, refs=(), race=()):
        self.runs = runs
        self.refs = set(refs)
        self.race = set(race)
        self.objects = {}

    def pages(self, path, key):
        if path.startswith('/git/matching-refs/tags/deploy-'):
            return [{'ref': 'refs/tags/' + ref} for ref in sorted(self.refs)]
        if path.startswith('/actions/workflows/'):
            assert 'branch=main' in path and 'created=%3E%3D2026-09-18' in path
            return [{'id': run_id} for run_id in self.runs]
        run_id = int(path.split('/')[3])
        assert path.endswith('?filter=all')
        return self.runs[run_id]

    def call(self, method, path, body):
        if path == '/git/tags':
            sha = 'tagobj-' + body['tag']
            self.objects[sha] = body
            return {'sha': sha}
        tag = body['ref'].removeprefix('refs/tags/')
        if tag in self.race or tag in self.refs:
            raise urllib.error.HTTPError(path, 422, 'Reference already exists', {}, io.BytesIO())
        self.refs.add(tag)
        return {}

    def message(self, tag):
        return self.objects['tagobj-' + tag]['message']


def run(github, run_id=8, attempt=1, sha='9' * 40, digest='sha256:' + 'd' * 64):
    return tags.run(github, IMAGE, run_id, attempt, sha, digest, 'https://github.com', NOW)


class TagDeploymentTests(unittest.TestCase):
    def test_backfills_history_with_beijing_minute_and_short_commit(self):
        runs = {**HISTORY, 8: [job(8, '9' * 40, '2026-10-18T10:15:00Z')]}
        github = FakeGitHub(runs)
        created = run(github)
        self.assertEqual([tag for tag, _, _ in created], [
            'deploy-20261014-1710-0292fc9', 'deploy-20261014-1907-f85148f',
            'deploy-20261015-1658-4698f17', 'deploy-20261015-1726-d2db740',
            'deploy-20261017-1131-cc45ac6', 'deploy-20261018-1815-9999999',
        ])
        self.assertEqual([current for _, _, current in created], [False] * 5 + [True])
        current = github.message('deploy-20261018-1815-9999999')
        self.assertIn('Image: ' + IMAGE + '@sha256:' + 'd' * 64, current)
        self.assertNotIn('Backfilled', current)
        old = github.message('deploy-20261017-1131-cc45ac6')
        self.assertTrue(old.startswith('Deployed to production at 2026-10-17 11:31:34 UTC+8\n'))
        self.assertIn('Image: ' + IMAGE + ':sha-cc45ac6', old)
        self.assertIn('Backfilled', old)
        self.assertEqual(github.objects['tagobj-deploy-20261017-1131-cc45ac6']['tagger']['date'], '2026-10-17T03:31:34Z')
        # Second run of the same job creates nothing.
        self.assertEqual(run(github), [])

    def test_names_never_trigger_release_and_cross_midnight_in_beijing(self):
        name = tags.tag_name(datetime(2026, 10, 14, 17, 30, tzinfo=timezone.utc), 'a' * 40)
        self.assertEqual(name, 'deploy-20261015-0130-aaaaaaa')
        self.assertFalse(name.startswith('v'))

    def test_every_successful_attempt_is_a_deployment(self):
        runs = {9: [job(9, 'b' * 40, '2026-10-18T01:00:00Z', attempt=1),
                    job(9, 'b' * 40, '2026-10-18T02:00:00Z', conclusion='failure', attempt=2),
                    job(9, 'b' * 40, '2026-10-18T03:00:00Z', attempt=3)]}
        created = run(FakeGitHub(runs), run_id=9, attempt=3, sha='b' * 40)
        self.assertEqual([tag for tag, _, _ in created], ['deploy-20261018-0900-bbbbbbb', 'deploy-20261018-1100-bbbbbbb'])

    def test_current_deployment_is_tagged_even_if_job_listing_lags(self):
        created = run(FakeGitHub({}))
        self.assertEqual(created, [('deploy-20261018-2000-9999999', '9' * 40, True)])

    def test_tag_created_meanwhile_is_not_an_error(self):
        runs = {8: [job(8, '9' * 40, '2026-10-18T10:15:00Z')]}
        self.assertEqual(run(FakeGitHub(runs, race={'deploy-20261018-1815-9999999'})), [])


if __name__ == '__main__':
    unittest.main()
