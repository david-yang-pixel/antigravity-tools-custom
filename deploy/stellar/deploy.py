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
REGISTRY = 'data/model_registry.json'


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


def image_version(labels):
    """Application version the build stamped on the image; /health must report the same."""
    version = labels.get('org.opencontainers.image.version', '')
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?', version):
        raise RuntimeError('Image version label missing or invalid')
    return version


def verify_portal_response(overview):
    """Shared with isolated smoke tests so deployment checks track the live API."""
    required = {'logs', 'daily', 'weekly', 'available_models', 'model_prices',
                'max_concurrency', 'balance_micros', 'free_models'}
    if not isinstance(overview, dict) or not required <= overview.keys():
        raise RuntimeError('Portal response incomplete')
    # Zero and negative balances are valid; null must never disable billing.
    if (type(overview['balance_micros']) is not int
            or not isinstance(overview['free_models'], list)
            or not all(isinstance(model, str) for model in overview['free_models'])):
        raise RuntimeError('Portal billing response invalid')


def publishable(model):
    """Mirrors the registry's public ID rule; other IDs are skipped with a notice."""
    model = model[len('models/'):] if model.startswith('models/') else model
    if not model or len(model.encode()) > 512 or any(
            c.isspace() or ord(c) < 32 or 127 <= ord(c) < 160 or c in '*?[]{}' for c in model):
        return None
    return model


def billable_models(db):
    """Exact priced and per-key free models: what user keys could already call."""
    tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
    models = set()
    if 'model_prices' in tables:
        models.update(row[0] for row in db.execute('SELECT pattern FROM model_prices'))
    for (free,) in db.execute('SELECT free_models FROM user_tokens'):
        try:
            entries = json.loads(free or '[]')
        except ValueError:
            continue
        if isinstance(entries, list):
            models.update(entry for entry in entries if isinstance(entry, str))
    return {model for model in map(publishable, models) if model}


def verify_model_registry(registry, required=()):
    """Public inference is closed unless the registry loads; a first migration must keep billable models."""
    if not isinstance(registry, dict) or not isinstance(registry.get('published_models'), list):
        raise RuntimeError('Model registry response invalid')
    published = {model.get('public_id') for model in registry['published_models']
                 if isinstance(model, dict) and model.get('enabled') is True}
    if not published:
        raise RuntimeError('Model registry has no enabled published models')
    missing = sorted(set(required) - published)
    if missing:
        raise RuntimeError(f'Model registry migration dropped {len(missing)} billable model(s): '
                           + ', '.join(missing[:5]))


def verify(image, version, first_registry=False):
    state = json.loads(run(['docker', 'inspect', CONTAINER], capture_output=True).stdout)[0]
    if state['Image'] != inspect_image(image)['Id'] or state['State']['Health']['Status'] != 'healthy':
        raise RuntimeError('Running image or container health did not match')
    for base in ['http://127.0.0.1:8045', 'https://antigravity.smlershou.top']:
        health = request(base, '/health')
        if health.get('version') != version:
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
        if not {'balance_micros', 'free_models', 'max_concurrency'} <= columns:
            raise RuntimeError('Token schema migration missing')
        user = db.execute('SELECT token FROM user_tokens WHERE enabled = 1 LIMIT 1').fetchone()
        # Later deployments respect deliberate unpublication; only the cutover must keep them.
        required = billable_models(db) if first_registry else set()
    verify_model_registry(request('http://127.0.0.1:8045', '/api/model-registry', key=admin), required)
    if user:
        overview = request('http://127.0.0.1:8045', '/api/token-portal/me', key=user[0])
        verify_portal_response(overview)


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
    # Checked before the service stops, so an unlabelled image never reaches rollback.
    version = image_version(labels)
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
    registry = ROOT / REGISTRY
    first_registry = not registry.exists()
    shutil.copy2(ROOT / 'compose.yaml', backup / 'compose.yaml')
    shutil.copy2(ROOT / '.env', backup / '.env')
    # Stop before archiving SQLite + WAL, so the snapshot is internally consistent.
    try:
        compose('stop', '-t', '60', CONTAINER)
        run(['tar', '-czf', str(backup / 'data.tar.gz'), '-C', str(ROOT), 'data', 'machine-id'])
        (ROOT / 'compose.yaml').write_text(updated)
        compose('up', '-d', '--no-build', '--pull', 'never', '--wait', '--wait-timeout', '120', CONTAINER)
        verify(image, version, first_registry)
        time.sleep(10)
        verify(image, version, first_registry)
    except BaseException:
        (ROOT / 'compose.yaml').write_text(original)
        try:
            compose('up', '-d', '--no-build', '--pull', 'never', '--wait', '--wait-timeout', '120', CONTAINER)
            print('Previous image restored. Data retained; snapshot available at ' + str(backup), flush=True)
        except Exception:
            print('Automatic rollback failed; manual recovery required from ' + str(backup), flush=True)
        # A registry created by a failed cutover would stop the next deploy from migrating
        # current data again. Set it aside (never delete) once the new image has stopped.
        if first_registry and registry.exists():
            try:
                shutil.move(str(registry), str(backup / 'model_registry.json'))
            except OSError:
                print('Move ' + str(registry) + ' aside before redeploying so the migration runs again', flush=True)
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
