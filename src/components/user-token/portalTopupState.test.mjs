// Run with: node --test src/components/user-token/portalTopupState.test.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import ts from 'typescript';

const source = fs.readFileSync(new URL('./portalTopupState.ts', import.meta.url), 'utf8');
const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2020 },
}).outputText;
const {
    serverNow, remainingSeconds, displayOrderStatus, formatCountdown, toCents,
    takeReturnedOrder, rememberReturnedOrder, forgetReturnedOrder,
} = await import('data:text/javascript;base64,' + Buffer.from(compiled).toString('base64'));

const createdAt = 1792396800;
const order = {
    out_trade_no: '12345', amount_cents: 100, credit_micros: 1000000,
    pay_type: 'alipay', status: 'pending', created_at: createdAt,
    expires_at: createdAt + 86400, paid_at: null,
};

test('countdown uses server time plus monotonic elapsed time, not device wall time', () => {
    assert.equal(serverNow({ seconds: createdAt, receivedAt: 100 }, 1600), createdAt + 1.5);
    assert.equal(serverNow({ seconds: createdAt, receivedAt: 100 }, 0), createdAt);
});

test('pending expires at its original deadline and paid orders remain paid', () => {
    assert.equal(remainingSeconds(order, createdAt), 86400);
    assert.equal(remainingSeconds(order, createdAt + 86399.5), 1);
    assert.equal(remainingSeconds(order, createdAt + 86400), 0);
    assert.equal(remainingSeconds(order, createdAt + 86401), 0);
    assert.equal(displayOrderStatus(order, createdAt + 86399.5), 'pending');
    assert.equal(displayOrderStatus(order, createdAt + 86400), 'closed');
    assert.equal(displayOrderStatus({ ...order, status: 'paid' }, createdAt + 86400), 'paid');
});

test('countdown renders both a full day and the last minute', () => {
    assert.equal(formatCountdown(86400), '24:00:00');
    assert.equal(formatCountdown(3661), '1:01:01');
    assert.equal(formatCountdown(61), '1:01');
    assert.equal(formatCountdown(0), '0:00');
});

test('payment amounts keep exact cents and reject unsupported formats', () => {
    for (const [value, expected] of [
        ['1', 100], ['1.2', 120], ['1.23', 123], ['0', 0], [' 10.01 ', 1001],
        ['1.234', null], ['1e3', null], ['-1', null], ['', null], ['999999999.99', 99999999999],
    ]) assert.equal(toCents(value), expected);
});

test('return and browser Back retain only a validated order number across sign-in', () => {
    const stored = new Map();
    globalThis.sessionStorage = {
        getItem: key => stored.get(key) ?? null,
        setItem: (key, value) => stored.set(key, value),
        removeItem: key => stored.delete(key),
    };
    let replaced = '';
    globalThis.window = {
        location: { search: '?out_trade_no=12345&sign=signature&token=credential', pathname: '/portal' },
        history: { replaceState: (_state, _title, path) => { replaced = path; } },
    };
    assert.equal(takeReturnedOrder(), '12345');
    assert.equal(replaced, '/portal');
    assert.deepEqual([...stored.values()], ['12345']);
    window.location.search = '';
    assert.equal(takeReturnedOrder(), '12345');
    forgetReturnedOrder();
    assert.equal(takeReturnedOrder(), null);
    rememberReturnedOrder('bad-order');
    assert.equal(takeReturnedOrder(), null);
    window.location.search = '?out_trade_no=invalid';
    assert.equal(takeReturnedOrder(), null);
    delete globalThis.window;
    delete globalThis.sessionStorage;
});
