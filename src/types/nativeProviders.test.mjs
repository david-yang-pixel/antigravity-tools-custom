import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import ts from 'typescript';
const source = await readFile(new URL('./nativeProviders.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext } });
const { nativeAccountStatus } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
test('stale quota does not disable scheduling and refreshable expiry is separate from revocation', () => {
    const account = { enabled: true, auth_error: null, expires_at: 100, can_refresh: true, cooldown_until: 0, in_flight: 0, quota: null };
    assert.equal(nativeAccountStatus(account, 1000), 'ready');
    assert.equal(nativeAccountStatus({ ...account, can_refresh: false }, 1000), 'reauthorize');
    assert.equal(nativeAccountStatus({ ...account, quota: { remaining: 0, unlimited: false, updated_at: 950 } }, 1000), 'exhausted');
    assert.equal(nativeAccountStatus({ ...account, quota: { remaining: 0, unlimited: false, updated_at: 10 } }, 1000), 'ready');
    assert.equal(nativeAccountStatus({ ...account, cooldown_until: 1010 }, 1000), 'cooldown');
    assert.equal(nativeAccountStatus({ ...account, enabled: false, cooldown_until: 1010 }, 1000), 'disabled');
});
test('official free models stay ready at zero credits without bypassing other account limits', () => {
    const account = { enabled: true, auth_error: null, expires_at: 2000, can_refresh: true,
        cooldown_until: 0, in_flight: 0, quota: { remaining: 0, unlimited: false, updated_at: 950 },
        models: [{ id: 'free-model', is_free: true }] };
    assert.equal(nativeAccountStatus(account, 1000), 'ready');
    assert.equal(nativeAccountStatus({ ...account, models: [{ id: 'unknown-model' }] }, 1000), 'exhausted');
    assert.equal(nativeAccountStatus({ ...account, in_flight: 1 }, 1000), 'busy');
    assert.equal(nativeAccountStatus({ ...account, cooldown_until: 1010 }, 1000), 'cooldown');
    assert.equal(nativeAccountStatus({ ...account, auth_error: 'expired' }, 1000), 'reauthorize');
    assert.equal(nativeAccountStatus({ ...account, enabled: false }, 1000), 'disabled');
});
