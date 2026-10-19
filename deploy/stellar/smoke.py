"""Exercise the built Linux image using isolated, disposable data only."""
import json
import os
import sys
import time
import urllib.error
import urllib.request

from deploy import verify_portal_response


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
        'max_concurrency': 2,
    })
    key = token['token']
    token_path = '/api/user-tokens/' + token['id']
    overview = request('/api/token-portal/me', key=key)
    verify_portal_response(overview)
    assert overview['max_concurrency'] == 2
    assert overview['balance_micros'] == 0 and overview['free_models'] == []
    assert not {'token_quota', 'remaining_tokens', 'allowed_models'} & overview.keys()
    assert overview['logs']['total'] == 0
    # With no upstream accounts, raw catalogs stay empty even for explicitly
    # free models. An entitlement must not invent an unavailable upstream model.
    request(token_path, key=admin, method='PATCH', data={
        'free_models': ['gemini-3-flash'],
    })
    assert request('/api/token-portal/me', key=key)['available_models'] == []
    assert request('/api/user-token-models', key=admin) == []
    assert request('/v1/models', key=key)['data'] == []
    config = request('/api/config', key=admin)
    assert config['proxy']['only_raw_quota_models'] is True
    config['proxy']['only_raw_quota_models'] = False
    request('/api/config', key=admin, data={'config': config})
    assert 'gemini-3-flash' in request('/api/user-token-models', key=admin)
    overview = request('/api/token-portal/me', key=key)
    assert overview['available_models'] == ['gemini-3-flash']
    assert overview['free_models'] == ['gemini-3-flash']
    assert overview['model_prices'] == []

    # A configured price exposes the model, but a zero-balance key cannot pay.
    price = {'model': 'gemini-3-flash', 'mode': 'per_call', 'call_price_micros': 1000}
    request('/api/model-prices', key=admin, data={
        'prices': [{'pattern': price['model'], 'mode': price['mode'],
                    'call_price_micros': price['call_price_micros']}],
    })
    assert request('/api/token-portal/me', key=key)['model_prices'] == []
    request(token_path, key=admin, method='PATCH', data={'free_models': []})
    overview = request('/api/token-portal/me', key=key)
    assert overview['available_models'] == ['gemini-3-flash']
    assert overview['model_prices'] == [price]
    denied = request('/v1/chat/completions', 402, key=key, data={
        'model': 'gemini-3-flash', 'messages': [{'role': 'user', 'content': 'test'}],
    })
    assert denied['error']['code'] == 'insufficient_balance'
    denied = request('/v1/chat/completions', 403, key=key, data={
        'model': 'unpriced-model', 'messages': [{'role': 'user', 'content': 'test'}],
    })
    assert denied['error']['code'] == 'model_not_priced'
    request('/api/model-prices', key=admin, data={'prices': []})
    assert request('/api/token-portal/me', key=key)['available_models'] == []
    assert request('/v1/models', key=key)['data'] == []

    request(token_path, key=admin, method='PATCH', data={
        'max_concurrency': 1, 'free_models': ['gemini-3-flash'], 'balance_micros': 10000,
    })
    # Editing unrelated fields preserves the balance, and null cannot opt out.
    request(token_path, key=admin, method='PATCH', data={'description': 'edited'})
    request(token_path, 422, key=admin, method='PATCH', data={'balance_micros': None})
    overview = request('/api/token-portal/me', key=key)
    assert overview['max_concurrency'] == 1 and overview['balance_micros'] == 10000
    assert overview['free_models'] == ['gemini-3-flash']
    assert overview['available_models'] == ['gemini-3-flash']
    assert overview['logs']['total'] == 0  # Rejected requests never reach an upstream.
    config['proxy']['only_raw_quota_models'] = True
    request('/api/config', key=admin, data={'config': config})
    assert request('/api/user-token-models', key=admin) == []
    assert request('/api/token-portal/me', key=key)['available_models'] == []
    request('/api/user-tokens', 401, key=key)
    request('/api/token-portal/me?start_time=2&end_time=1', 400, key=key)
    print('Application smoke checks passed: auth isolation, portal, concurrency, free models, '
          'balance defaults/editing, unpriced rejection, priced admission, catalogs, time filter.')


if __name__ == '__main__':
    main()
