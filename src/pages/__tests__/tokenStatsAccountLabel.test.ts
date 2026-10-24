/**
 * Run: npx tsx src/pages/__tests__/tokenStatsAccountLabel.test.ts
 */
import { accountChartLabel } from '../tokenStatsAccountLabel';

let passed = 0;
let failed = 0;

function test(description: string, fn: () => void): void {
    try {
        fn();
        passed++;
    } catch (e: unknown) {
        failed++;
        const msg = e instanceof Error ? e.message : String(e);
        console.error(`  FAIL: ${description}  — ${msg}`);
    }
}

function assertEqual(actual: string, expected: string): void {
    if (actual !== expected) {
        throw new Error(`expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
    }
}

test('ordinary email keeps the local part', () => {
    assertEqual(accountChartLabel('alice@gmail.com'), 'alice');
});

test('same uid in CN and Global stays distinct', () => {
    const cn = accountChartLabel('u1@workbuddy-cn');
    const global = accountChartLabel('u1@workbuddy-global');
    assertEqual(cn, 'cn:u1');
    assertEqual(global, 'global:u1');
    if (cn === global) {
        throw new Error('CN and Global short labels collided');
    }
});

test('encoded uid separators stay in the local part', () => {
    assertEqual(accountChartLabel('Ada%2B42@workbuddy-cn'), 'cn:Ada%2B42');
    assertEqual(
        accountChartLabel('Ada%2B42%40workbuddy-global@workbuddy-cn'),
        'cn:Ada%2B42%40workbuddy-global',
    );
});

test('a key without a realm separator is unchanged', () => {
    assertEqual(accountChartLabel('not-an-email'), 'not-an-email');
});

if (failed > 0) {
    console.error(`${failed} failed, ${passed} passed`);
    throw new Error(`${failed} failed`);
}
console.log(`${passed} passed`);
