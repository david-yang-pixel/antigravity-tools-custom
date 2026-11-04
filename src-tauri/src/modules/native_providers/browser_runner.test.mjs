import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { once } from 'node:events';
import { LoginBrowser, officialOrigin, validateStart, matchingTrace, inputEvents } from './browser_runner.mjs';

test('only exact HTTPS provider origins can deliver a callback', () => {
    for (const origin of ['https://www.trae.cn', 'https://trae.ai', 'https://www.trae.com.cn']) assert.equal(officialOrigin(origin), true);
    for (const origin of [undefined, 'null', 'http://www.trae.cn', 'https://www.trae.cn.evil.test', 'https://user@trae.cn', 'https://trae.cn/path', 'https://trae.cn:444', 'https://trae.cn/']) assert.equal(officialOrigin(origin), false);
});

test('server browser accepts only the official loopback callback bound to its trace', () => {
    const callback_url = 'http://127.0.0.1:18081/authorize';
    const auth = new URL('https://www.trae.cn/authorization');
    auth.searchParams.set('auth_callback_url', callback_url); auth.searchParams.set('login_trace_id', 'test-trace');
    const config = { auth_url: auth.href, callback_url, trace: 'test-trace', debug_port: 9249, expires_at: Math.floor(Date.now() / 1000) + 600 };
    assert.equal(validateStart(config).callback.href, callback_url);
    for (const override of [{ callback_url: 'https://gateway.test/authorize' }, { callback_url: 'http://127.0.0.1:18081/authorize/nonce' }, { trace: 'other' }, { auth_url: 'https://trae.cn.evil.test/authorization' }, { debug_port: 0 }, { expires_at: 1 }]) assert.throws(() => validateStart({ ...config, ...override }));
});

test('callbacks reject missing, conflicting and oversized traces', () => {
    assert.equal(matchingTrace('loginTraceID=a&authCodeInfo=test', 'a'), true);
    assert.equal(matchingTrace('loginTraceID=a&login_trace_id=a', 'a'), true);
    for (const query of ['', 'loginTraceID=b', 'loginTraceID=a&loginTraceID=b', 'loginTraceID=a&login_trace_id=b', `loginTraceID=a&extra=${'x'.repeat(65536)}`]) assert.equal(matchingTrace(query, 'a'), false);
});

test('bounded input translates to keyboard/mouse CDP only', () => {
    assert.equal(inputEvents({ type: 'click', x: 4, y: 6 })[1][1].type, 'mouseReleased');
    assert.equal(inputEvents({ type: 'text', text: '中文' })[0][0], 'Input.insertText');
    assert.equal(inputEvents({ type: 'key', key: 'SelectAll' })[0][1].modifiers, 2);
    assert.equal(inputEvents({ type: 'key', key: 'ShiftTab' })[0][1].modifiers, 8);
    for (const action of [{ type: 'click', x: -1, y: 0 }, { type: 'click', x: NaN, y: 0 }, { type: 'text', text: '中'.repeat(700) }, { type: 'text', text: '' }, { type: 'key', key: 'F12' }, { type: 'scroll', delta_y: Infinity }, { type: 'evaluate', expression: '1' }, { type: 'navigate', url: 'http://localhost/' }]) assert.throws(() => inputEvents(action));
});

async function receiver(t, accept) {
    const events = [];
    const browser = new LoginBrowser(event => {
        events.push(event);
        if (event.event === 'grant') queueMicrotask(() => browser.grantResult({ request: event.request, success: accept }));
    });
    browser.config = { trace: 'test-trace', expires_at: Math.floor(Date.now() / 1000) + 60 };
    const server = http.createServer((req, res) => void browser.receive(req, res));
    server.listen(0, '127.0.0.1'); await once(server, 'listening');
    browser.callback = new URL(`http://127.0.0.1:${server.address().port}/authorize`);
    t.after(() => { browser.stop(); server.closeAllConnections(); server.close(); });
    return { browser, url: browser.callback, events };
}

test('loopback validates origin and trace before forwarding a grant on stdio', async t => {
    const { url, events } = await receiver(t, false);
    for (const [query, origin, status] of [['loginTraceID=test-trace', 'https://evil.test', 403], ['loginTraceID=other', 'https://www.trae.cn', 400], ['loginTraceID=test-trace&login_trace_id=other', 'https://www.trae.cn', 400]]) {
        const response = await fetch(`${url}?${query}`, { headers: { origin } });
        assert.equal(response.status, status); assert.equal(events.length, 0);
    }
    const response = await fetch(`${url}?loginTraceID=test-trace&authCodeInfo=fixture-only`, { headers: { origin: 'https://www.trae.cn' } });
    assert.equal(response.status, 400); assert.equal(events.length, 1);
    assert.equal((await response.json()).success, false);
    assert.equal(response.headers.get('cache-control'), 'no-store');
});

test('loopback preflight grants only official origins and does not expose a grant', async t => {
    const { url, events } = await receiver(t, false);
    const response = await fetch(url, { method: 'OPTIONS', headers: { origin: 'https://www.trae.cn', 'access-control-request-private-network': 'true' } });
    assert.equal(response.status, 204);
    assert.equal(response.headers.get('access-control-allow-origin'), 'https://www.trae.cn');
    assert.equal(response.headers.get('access-control-allow-private-network'), 'true');
    assert.equal(events.length, 0);
});

test('callback success requires explicit owning-process acknowledgement and is single-use', async t => {
    const { url, events } = await receiver(t, true);
    const response = await fetch(url, { method: 'POST', headers: { origin: 'https://www.trae.ai' }, body: 'login_trace_id=test-trace&authCodeInfo=fixture-only' });
    assert.equal(response.status, 200); assert.deepEqual(await response.json(), { success: true });
    assert.equal(events[0].event, 'grant');
    assert.equal((await fetch(`${url}?loginTraceID=test-trace`)).status, 410);
});
