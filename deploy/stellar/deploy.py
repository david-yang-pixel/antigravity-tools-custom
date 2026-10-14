#!/usr/bin/env python3
"""Root-owned SSH forced command. Install manually; never execute incoming shell text.

Accepts only: deploy <fixed GHCR repository>@sha256:<digest> <commit SHA>
stdin carries an ephemeral GitHub Actions registry token, never an SSH command.
"""
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path('/opt/antigravity-manager')
BACKUPS = Path('/opt/antigravity-manager-backups')
IMAGE = 'ghcr.io/david-yang-pixel/antigravity-tools-custom'
CONTAINER = 'antigravity-manager'


def parse_command(command):
    match = re.fullmatch(r'deploy (' + re.escape(IMAGE) + r'@sha256:[0-9a-f]{64}) ([0-9a-f]{40})', command)
    if not match:
        raise ValueError('Only deployment of a pinned Stellar AI image is permitted')
    return match.groups()


def run(args, **kwargs):
    return subprocess.run(args, check=True, timeout=kwargs.pop('timeout', 180), **kwargs)


def inspect_image(image):
    return json.loads(run(['docker', 'image', 'inspect', image], capture_output=True).stdout)[0]


def compose(*args):
    run(['docker', 'compose', '--project-directory', str(ROOT), '-f', str(ROOT / 'compose.yaml'), *args])


def request(base, path, expected=200, key=None):
    headers = {'Authorization': 'Bearer ' + key} if key else {}
    req = urllib.request.Request(base + path, headers=headers)
    try:
        response = urllib.request.urlopen(req, timeout=15)
    except urllib.error.HTTPError as exc:
        response = exc
    with response:
        data = response.read()
        if response.code != expected:
            raise RuntimeError(f'{path}: expected HTTP {expected}, got {response.code}')
        return json.loads(data) if 'application/json' in response.headers.get('Content-Type', '') else data


def verify(image):
    state = json.loads(run(['docker', 'inspect', CONTAINER], capture_output=True).stdout)[0]
    if state['Image'] != inspect_image(image)['Id'] or state['State']['Health']['Status'] != 'healthy':
        raise RuntimeError('Running image or container health did not match')
    for base in ['http://127.0.0.1:8045', 'https://antigravity.smlershou.top']:
        health = request(base, '/health')
        if health.get('version') != '4.9.6':
            raise RuntimeError('Unexpected application version')
        html = request(base, '/usage')
        if not isinstance(html, bytes) or b'<html' not in html.lower():
            raise RuntimeError('Portal HTML missing')
        request(base, '/api/user-tokens', 401)
        request(base, '/api/token-portal/me', 401)
        request(base, '/v1/models', 401)
    # Read-only verification using existing credentials; never print keys or API bodies.
    config = json.loads((ROOT / 'data/gui_config.json').read_text())
    proxy = config['proxy']
    env = {}
    for line in (ROOT / '.env').read_text().splitlines():
        if '=' in line and not line.lstrip().startswith('#'):
            k, v = line.split('=', 1)
            env[k.strip()] = v.strip().strip('"').strip("'")
    admin = (env.get('ABV_WEB_PASSWORD') or env.get('WEB_PASSWORD') or proxy.get('admin_password')
             or env.get('ABV_API_KEY') or env.get('API_KEY') or proxy.get('api_key'))
    if not admin:
        raise RuntimeError('Cannot verify authenticated admin endpoint')
    request('http://127.0.0.1:8045', '/api/user-tokens', key=admin)
    with sqlite3.connect((ROOT / 'data/user_tokens.db').as_uri() + '?mode=ro', uri=True) as db:
        columns = {row[1] for row in db.execute('PRAGMA table_info(user_tokens)')}
        if not {'allowed_models', 'max_concurrency', 'token_quota'} <= columns:
            raise RuntimeError('Token schema migration missing')
        user = db.execute('SELECT token FROM user_tokens WHERE enabled = 1 LIMIT 1').fetchone()
    if user:
        overview = request('http://127.0.0.1:8045', '/api/token-portal/me', key=user[0])
        if not {'logs', 'daily', 'weekly', 'available_models', 'token_quota'} <= overview.keys():
            raise RuntimeError('Portal response incomplete')


def deploy(image, revision, credential):
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix='stellar-registry-') as directory:
        env = {**os.environ, 'DOCKER_CONFIG': directory}
        result = subprocess.run(
            ['docker', 'login', 'ghcr.io', '-u', 'david-yang-pixel', '--password-stdin'],
            input=credential, text=True, env=env, capture_output=True, timeout=60,
        )
        if result.returncode:
            raise RuntimeError('GHCR authentication failed')
        run(['docker', 'pull', image], env=env, timeout=600)
    labels = inspect_image(image)['Config'].get('Labels') or {}
    if labels.get('org.opencontainers.image.revision') != revision:
        raise RuntimeError('Image revision does not match requested commit')
    original = (ROOT / 'compose.yaml').read_text()
    if len(re.findall(r'^    image:.*$', original, re.M)) != 1:
        raise RuntimeError('Unexpected Compose layout; refusing automated edit')
    if not (ROOT / 'machine-id').exists():
        # Proxy passwords use this identity for encryption; retain it across image builds.
        run(['docker', 'cp', CONTAINER + ':/etc/machine-id', str(ROOT / 'machine-id')])
        (ROOT / 'machine-id').chmod(0o400)
    updated = re.sub(r'^    image:.*$', '    image: ' + image, original, flags=re.M)
    if './machine-id:/etc/machine-id:ro' not in updated:
        anchor = '      - ./data:/root/.antigravity_tools'
        if anchor not in updated:
            raise RuntimeError('Unexpected data mount; refusing automated edit')
        updated = updated.replace(anchor, anchor + '\n      - ./machine-id:/etc/machine-id:ro\n      - ./machine-id:/var/lib/dbus/machine-id:ro')
    BACKUPS.mkdir(mode=0o700, exist_ok=True)
    stamp = time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()) + '-' + revision[:12]
    backup = BACKUPS / stamp
    backup.mkdir(mode=0o700)
    shutil.copy2(ROOT / 'compose.yaml', backup / 'compose.yaml')
    shutil.copy2(ROOT / '.env', backup / '.env')
    # Stop before archiving SQLite + WAL, so the snapshot is internally consistent.
    try:
        compose('stop', '-t', '60', CONTAINER)
        run(['tar', '-czf', str(backup / 'data.tar.gz'), '-C', str(ROOT), 'data', 'machine-id'])
        (ROOT / 'compose.yaml').write_text(updated)
        compose('up', '-d', '--no-build', '--pull', 'never', '--wait', '--wait-timeout', '120', CONTAINER)
        verify(image)
        time.sleep(10)
        verify(image)
    except BaseException:
        (ROOT / 'compose.yaml').write_text(original)
        try:
            compose('up', '-d', '--no-build', '--pull', 'never', '--wait', '--wait-timeout', '120', CONTAINER)
            print('Previous image restored. Data retained; snapshot available at ' + str(backup), flush=True)
        except Exception:
            print('Automatic rollback failed; manual recovery required from ' + str(backup), flush=True)
        raise
    (ROOT / 'last-deployment.json').write_text(json.dumps({
        'revision': revision, 'image': image, 'backup': str(backup), 'verified_at': stamp,
    }, indent=2) + '\n')
    print('Verified deployment: ' + revision + '\nBackup: ' + str(backup), flush=True)


def main():
    image, revision = parse_command(os.environ.get('SSH_ORIGINAL_COMMAND', ''))
    credential = sys.stdin.readline(8192).strip()
    if not credential or len(credential) >= 8191:
        raise ValueError('Missing or invalid registry credential')
    with open('/run/lock/stellar-deploy.lock', 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        deploy(image, revision, credential)


if __name__ == '__main__':
    main()
