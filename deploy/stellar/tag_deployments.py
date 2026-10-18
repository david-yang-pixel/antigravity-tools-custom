#!/usr/bin/env python3
"""Tag every verified production deployment that has no tag yet.

Runs in the workflow's tag job after a deployment. A deployment is a successful
`deploy` job of the Stellar workflow on main; it is tagged
deploy-YYYYMMDD-HHMM-<short commit> (Beijing time of the deploy job's completion).
Tag names are derived from the run history alone, so repeating the job is
idempotent, and deployments whose tag job failed or predates it are backfilled
within the lookback window.
"""
from datetime import datetime, timedelta, timezone
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

API = 'https://api.github.com'
WORKFLOW = 'stellar-deploy.yml'
DEPLOY_JOB = 'deploy'
LOOKBACK_DAYS = 30
BEIJING = timezone(timedelta(hours=8))
BOT = {'name': 'github-actions[bot]', 'email': '41898282+github-actions[bot]@users.noreply.github.com'}


def parse_time(value):
    return datetime.fromisoformat(value.replace('Z', '+00:00'))


def tag_name(completed_at, sha):
    # Never "v*": that pattern triggers release.yml.
    return 'deploy-' + completed_at.astimezone(BEIJING).strftime('%Y%m%d-%H%M') + '-' + sha[:7]


def deployments(jobs):
    """Successful deploy jobs (all run attempts), oldest first."""
    found = [
        {
            'sha': job['head_sha'],
            'completed_at': parse_time(job['completed_at']),
            'run_id': job['run_id'],
            'attempt': job.get('run_attempt', 1),
        }
        for job in jobs
        if job.get('name') == DEPLOY_JOB and job.get('conclusion') == 'success' and job.get('completed_at')
    ]
    return sorted(found, key=lambda item: item['completed_at'])


def message(deployment, image, run_url, digest=None):
    when = deployment['completed_at'].astimezone(BEIJING).strftime('%Y-%m-%d %H:%M:%S')
    image_ref = image + '@' + digest if digest else image + ':sha-' + deployment['sha']
    lines = ['Deployed to production at ' + when + ' UTC+8', 'Image: ' + image_ref, 'Run: ' + run_url]
    if not digest:
        lines += ['', 'Backfilled from the workflow run history; the image digest was not recorded.']
    return '\n'.join(lines)


class GitHub:
    def __init__(self, repo, token):
        self.repo = repo
        self.token = token

    def call(self, method, path, body=None):
        request = urllib.request.Request(
            API + '/repos/' + self.repo + path,
            data=None if body is None else json.dumps(body).encode(),
            method=method,
            headers={
                'Accept': 'application/vnd.github+json',
                'Authorization': 'Bearer ' + self.token,
                'X-GitHub-Api-Version': '2022-11-28',
            },
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.loads(response.read() or 'null')

    def pages(self, path, key):
        page = 1
        while True:
            separator = '&' if '?' in path else '?'
            data = self.call('GET', path + separator + 'per_page=100&page=' + str(page))
            items = data[key] if key else data
            yield from items
            if len(items) < 100:
                return
            page += 1


def run(github, image, run_id, run_attempt, run_sha, digest, server_url, now):
    existing = {ref['ref'].removeprefix('refs/tags/') for ref in github.pages('/git/matching-refs/tags/deploy-', None)}
    since = (now - timedelta(days=LOOKBACK_DAYS)).strftime('%Y-%m-%d')
    query = urllib.parse.urlencode({'branch': 'main', 'created': '>=' + since})
    jobs = []
    for workflow_run in github.pages('/actions/workflows/' + WORKFLOW + '/runs?' + query, 'workflow_runs'):
        jobs += github.pages('/actions/runs/' + str(workflow_run['id']) + '/jobs?filter=all', 'jobs')
    found = deployments(jobs)
    current = [item for item in found if item['run_id'] == run_id and item['attempt'] == run_attempt]
    if not current:
        # The deploy job this tag job depends on succeeded; never leave it untagged
        # because the jobs listing lagged behind.
        found.append({'sha': run_sha, 'completed_at': now, 'run_id': run_id, 'attempt': run_attempt})

    created = []
    for item in found:
        tag = tag_name(item['completed_at'], item['sha'])
        if tag in existing:
            continue
        is_current = item['run_id'] == run_id and item['attempt'] == run_attempt
        run_url = server_url + '/' + github.repo + '/actions/runs/' + str(item['run_id'])
        tagger = dict(BOT, date=item['completed_at'].astimezone(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'))
        obj = github.call('POST', '/git/tags', {
            'tag': tag, 'object': item['sha'], 'type': 'commit', 'tagger': tagger,
            'message': message(item, image, run_url, digest if is_current else None),
        })
        try:
            github.call('POST', '/git/refs', {'ref': 'refs/tags/' + tag, 'sha': obj['sha']})
        except urllib.error.HTTPError as error:
            if error.code != 422:  # 422: created meanwhile by another run
                raise
            continue
        existing.add(tag)
        created.append((tag, item['sha'], is_current))
    return created


def main():
    env = os.environ
    github = GitHub(env['GITHUB_REPOSITORY'], env['GH_TOKEN'])
    created = run(
        github, env['IMAGE'], int(env['GITHUB_RUN_ID']), int(env.get('GITHUB_RUN_ATTEMPT', '1')),
        env['GITHUB_SHA'], env.get('DIGEST') or None, env.get('GITHUB_SERVER_URL', 'https://github.com'),
        datetime.now(timezone.utc),
    )
    lines = ['- `' + tag + '` → `' + sha + '`' + ('' if current else ' (backfilled)') for tag, sha, current in created]
    summary = '\n'.join(['### Deployment tags'] + (lines or ['No missing deployment tags.']))
    print(summary)
    if env.get('GITHUB_STEP_SUMMARY'):
        with open(env['GITHUB_STEP_SUMMARY'], 'a') as handle:
            handle.write(summary + '\n')


if __name__ == '__main__':
    sys.exit(main())
