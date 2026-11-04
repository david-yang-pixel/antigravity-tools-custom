import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { LoginBrowser, officialOrigin, validateStart, validBrowser, matchingTrace, inputEvents } from './browser_runner.mjs';

test('only exact HTTPS provider origins can deliver a callback', () => {
    for (const origin of ['https://www.trae.cn', 'https://trae.ai', 'https://www.trae.com.cn']) assert.equal(officialOrigin(origin), true);
    for (const origin of [undefined, 'null', 'http://www.trae.cn', 'https://www.trae.cn.evil.test', 'https://user@trae.cn', 'https://trae.cn/path', 'https://trae.cn:444', 'https://trae.cn/']) assert.equal(officialOrigin(origin), false);
});

test('server browser accepts only the official loopback callback bound to its trace', () => {
    const callback_url = 'http://127.0.0.1:18081/authorize';
    const auth = new URL('https://www.trae.cn/authorization');
    auth.searchParams.set('auth_callback_url', callback_url); auth.searchParams.set('login_trace_id', 'test-trace');
    const config = { auth_url: auth.href, callback_url, trace: 'test-trace', browser: { executables: ['chromium'], args: ['--headless'] }, expires_at: Math.floor(Date.now() / 1000) + 600 };
    assert.equal(validateStart(config).callback.href, callback_url);
    for (const override of [{ callback_url: 'https://gateway.test/authorize' }, { callback_url: 'http://127.0.0.1:18081/authorize/nonce' }, { trace: 'other' }, { auth_url: 'https://trae.cn.evil.test/authorization' }, { browser: undefined }, { expires_at: 1 }]) assert.throws(() => validateStart({ ...config, ...override }));
});

test('DevTools is never exposed on a port: debugging flags are refused', () => {
    assert.equal(validBrowser({ executables: ['chromium'], args: ['--headless'] }), true);
    for (const browser of [undefined, { executables: [], args: [] }, { executables: [''], args: [] }, { executables: ['chromium'], args: [1] },
        { executables: ['chromium'], args: ['--remote-debugging-port=9222'] }, { executables: ['chromium'], args: ['--remote-debugging-address=0.0.0.0'] },
        { executables: ['chromium'], args: ['--remote-debugging-pipe'] }]) assert.equal(validBrowser(browser), false);
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

// Opt-in: set ABV_TRAE_BROWSER_PATH to a local Chromium/Chrome/Edge to exercise the real pipe.
test('real browser runs DevTools over private pipes only', { skip: !process.env.ABV_TRAE_BROWSER_PATH }, async t => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'trae-pipe-'));
    const events = [];
    const browser = new LoginBrowser(event => events.push(event));
    const port = await new Promise(resolve => { const s = http.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); }); });
    const callback_url = `http://127.0.0.1:${port}/authorize`;
    const auth = new URL('https://www.trae.ai/authorization');
    auth.searchParams.set('auth_callback_url', callback_url); auth.searchParams.set('login_trace_id', 'pipe-trace');
    const args = ['--headless', '--disable-gpu', '--no-first-run', `--user-data-dir=${path.join(dir, 'profile')}`, '--proxy-server=127.0.0.1:9', '--proxy-bypass-list=127.0.0.1;localhost'];
    if (process.env.ABV_TRAE_BROWSER_NO_SANDBOX === 'true') args.push('--no-sandbox');
    t.after(async () => { await browser.shutdown(); fs.rmSync(dir, { recursive: true, force: true }); });
    assert.deepEqual(await browser.start({ auth_url: auth.href, callback_url, trace: 'pipe-trace', expires_at: Math.floor(Date.now() / 1000) + 120,
        browser: { executables: ['/nonexistent/browser', process.env.ABV_TRAE_BROWSER_PATH], args } }), { ready: true });
    // A port-based DevTools endpoint would publish DevToolsActivePort in the profile.
    assert.equal(fs.existsSync(path.join(dir, 'profile', 'DevToolsActivePort')), false);
    const frame = await browser.snapshot();
    assert.ok(frame.image.startsWith('data:image/jpeg;base64,'));
    assert.equal(frame.width, 1000);
    assert.deepEqual(await browser.input({ type: 'click', x: 10, y: 10 }), {});
    await browser.shutdown();
    assert.notEqual(browser.chrome.exitCode ?? browser.chrome.signalCode, null);
    assert.ok(events.some(event => event.event === 'done'));
});
