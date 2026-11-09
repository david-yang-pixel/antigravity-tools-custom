import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { existsSync } from 'node:fs';
import { readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { authorize, totp } from './browser.mjs';
import { socksFixture } from './browser-proxy.fixture.mjs';

test('TOTP uses the standard Base32 key and 30 second SHA1 counter', () => {
    const key = 'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ';
    assert.equal(totp(key, 59), '287082');
    assert.equal(totp(key, 1111111109), '081804');
    assert.equal(totp(key, 1234567890), '005924');
    assert.throws(() => totp('not-a-base32-key'), /INVALID_2FA/);
});

test('built-in worker refuses a merchant login origin', async () => {
    await assert.rejects(authorize({auth_url:'https://merchant.example/login'}), {code:'INVALID_LOGIN_ORIGIN'});
});

test('official browser flow enters email, password and TOTP then captures the OAuth callback', {
    skip: !existsSync(process.env.ABV_CHATGPT_BROWSER || '/usr/bin/chromium'),
}, async () => {
    const before = new Set((await readdir(tmpdir())).filter(name => name.startsWith('abv-chatgpt-')));
    const steps = [];
    const form = (type, name, action, label = '') => `<form method="post" action="${action}">${label}<input type="${type}" name="${name}"><button type="submit">Continue</button></form>`;
    const server = createServer(async (req, res) => {
        let raw = ''; for await (const part of req) raw += part;
        const params = new URLSearchParams(raw);
        res.setHeader('Content-Type', 'text/html');
        if (req.url === '/challenge') { res.end('<p>Verify you are human</p>'); return; }
        if (req.url === '/cf-check') { res.statusCode=403; res.end('<title>Just a moment...</title><div id="challenge-running">Checking...</div>'); return; }
        if (req.url === '/automatic-check') { res.end('<title>Just a moment...</title><div id="challenge-running">Checking...</div><script>setTimeout(()=>location.replace("/oauth/authorize"),500)</script>'); return; }
        if (req.url === '/blocked') { res.statusCode=403; res.end('<p>Access denied</p>'); return; }
        if (req.url === '/empty') { res.end('<title>Loading</title>'); return; }
        if (req.url === '/stalled-email') { res.end(form('email','username','/stalled-email')); return; }
        if (req.url === '/reset') { req.socket.destroy(); return; }
        if (req.url === '/email-code') { res.end('<p>Check your inbox for an email verification code</p>'); return; }
        if (req.url === '/oauth/authorize') res.end(form('email', 'username', '/password'));
        else if (req.url === '/password') {
            assert.equal(params.get('username'), 'a@example.com'); steps.push('email');
            res.end(form('password', 'password', '/totp'));
        } else if (req.url === '/totp') {
            assert.equal(params.get('password'), 'private-password'); steps.push('password');
            res.end(form('text', 'totp', '/done', 'Enter the code from your authenticator app'));
        } else if (req.url === '/done') {
            assert.equal(params.get('totp'), totp('JBSWY3DPEHPK3PXP')); steps.push('totp');
            res.writeHead(302, { Location: 'http://localhost:1455/auth/callback?code=private-code&state=test-state' }); res.end();
        } else { res.statusCode = 404; res.end('missing'); }
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const base = `http://127.0.0.1:${server.address().port}`;
    const socks = await socksFixture();
    const input = { auth_url: `${base}/oauth/authorize`, browser: process.env.ABV_CHATGPT_BROWSER || '/usr/bin/chromium',
        email: 'a@example.com', password: 'private-password', two_factor: 'JBSWY3DPEHPK3PXP', proxy: socks.url };
    try {
        const result = await authorize(input, { testOrigin: base, timeoutMs: 20000 });
        assert.equal(result.callback, 'http://localhost:1455/auth/callback?code=private-code&state=test-state');
        assert.deepEqual(steps, ['email', 'password', 'totp']);
        steps.length=0;
        const automatic = await authorize({...input,auth_url:base+'/automatic-check'},{testOrigin:base,timeoutMs:10000,challengeGraceMs:1500});
        assert.equal(automatic.callback,result.callback);
        assert.deepEqual(steps,['email','password','totp']);
        assert(socks.destinations.some(target => target.host === '127.0.0.1' && target.port === server.address().port), 'Chromium must use the authenticated route even for the local fixture');
        for (const [path, code] of [['/challenge', 'CHALLENGE'], ['/cf-check', 'CHALLENGE'], ['/blocked','LOGIN_BLOCKED'], ['/email-code', 'EMAIL_VERIFICATION'], ['/reset','LOGIN_CONNECTION_FAILED']]) {
            await assert.rejects(authorize({ ...input, auth_url: base + path }, { testOrigin: base, timeoutMs: 5000, challengeGraceMs:100 }), { code });
        }
        for (const [path, code] of [['/empty','LOGIN_PAGE_TIMEOUT'], ['/stalled-email','EMAIL_STEP_TIMEOUT']]) {
            await assert.rejects(authorize({...input,auth_url:base+path},{testOrigin:base,timeoutMs:10000,stepTimeoutMs:750}), {code});
        }
        await assert.rejects(authorize({...input, proxy:socks.url.replace('route-password','wrong-password')}, {testOrigin:base, timeoutMs:5000}), {code:'PROXY_AUTH_FAILED'});
        const leftovers = (await readdir(tmpdir())).filter(name => name.startsWith('abv-chatgpt-') && !before.has(name));
        assert.deepEqual(leftovers, [], 'temporary browser profiles must be removed on success and failure');
    } finally { await socks.close(); await new Promise(resolve => server.close(resolve)); }
});
