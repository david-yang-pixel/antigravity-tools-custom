// Offline pure-logic checks for quota window names.
// Run: node --test src/utils/quotaWindow.test.mjs
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import ts from 'typescript';

const source = await readFile(new URL('./quotaWindow.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext } });
const { quotaWindow } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);

test('window names carry their length and unit', () => {
    assert.deepEqual(quotaWindow('5h'), { count: 5, unit: 'hour' });
    assert.deepEqual(quotaWindow('7d'), { count: 7, unit: 'day' });
    assert.deepEqual(quotaWindow('30d'), { count: 30, unit: 'day' });
    assert.deepEqual(quotaWindow('90m'), { count: 90, unit: 'minute' });
});

test('names that are not a window length are left to the caller', () => {
    for (const name of ['', 'credits', '0h', 'h5', '5w', '1.5h']) assert.equal(quotaWindow(name), null, name);
});
