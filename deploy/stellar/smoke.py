"""Exercise the built Linux image using isolated, disposable data only."""
import json
import os
import sys
import time
import urllib.error
import urllib.request


def main():
    base = sys.argv[1]

    def request(path, status=200, key=None, data=None, method=None):
        headers = {'Content-Type': 'application/json'}
        if key:
            headers['Authorization'] = 'Bearer ' + key
        req = urllib.request.Request(base + path, headers=headers, method=method,
                                     data=json.dumps(data).encode() if data is not None else None)
        try:
            response = urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError as exc:
            response = exc
        with response:
            body = response.read()
            assert response.code == status, f'{path}: expected {status}, got {response.code}'
            return json.loads(body) if 'application/json' in response.headers.get('Content-Type', '') else body

    for attempt in range(60):
        try:
            request('/health')
            break
        except (OSError, AssertionError):
            if attempt == 59:
                raise
            time.sleep(2)
    request('/usage')
    request('/api/user-tokens', 401)
    request('/api/token-portal/me', 401)
    admin = os.environ['SMOKE_ADMIN']
    request('/api/proxy/start', key=admin, method='POST')
    token = request('/api/user-tokens', key=admin, data={
        'username': 'CI smoke test', 'expires_type': 'never', 'max_ips': 0,
        'max_concurrency': 2, 'token_quota': 1000000, 'allowed_models': ['gemini-3-flash'],
    })
    key = token['token']
    overview = request('/api/token-portal/me', key=key)
    assert overview['max_concurrency'] == 2 and overview['token_quota'] == 1000000
    assert overview['allowed_models'] == ['gemini-3-flash']
    # With no upstream accounts, raw catalogs stay empty, including for keys
    # whose saved permissions mention an old or unavailable model.
    assert overview['available_models'] == []
    assert request('/api/user-token-models', key=admin) == []
    assert request('/v1/models', key=key)['data'] == []
    config = request('/api/config', key=admin)
    assert config['proxy']['only_raw_quota_models'] is True
    config['proxy']['only_raw_quota_models'] = False
    request('/api/config', key=admin, data={'config': config})
    assert 'gemini-3-flash' in request('/api/user-token-models', key=admin)
    assert request('/api/token-portal/me', key=key)['available_models'] == ['gemini-3-flash']
    config['proxy']['only_raw_quota_models'] = True
    request('/api/config', key=admin, data={'config': config})
    assert request('/api/user-token-models', key=admin) == []
    assert request('/api/token-portal/me', key=key)['available_models'] == []
    assert overview['logs']['total'] == 0
    request('/api/user-tokens', 401, key=key)
    request('/api/token-portal/me?start_time=2&end_time=1', 400, key=key)
    request('/v1/chat/completions', 403, key=key, data={
        'model': 'forbidden-model', 'messages': [{'role': 'user', 'content': 'test'}],
    })
    request('/api/user-tokens/' + token['id'], key=admin, method='PATCH', data={
        'max_concurrency': 1, 'token_quota': 2000000,
    })
    overview = request('/api/token-portal/me', key=key)
    assert overview['max_concurrency'] == 1 and overview['token_quota'] == 2000000
    print('Application smoke checks passed: auth isolation, portal, limits, models, time filter, editing.')


if __name__ == '__main__':
    main()
