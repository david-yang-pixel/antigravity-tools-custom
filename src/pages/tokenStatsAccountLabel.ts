const WORKBUDDY_REALM_PREFIX = 'workbuddy-';

/**
 * Short label for a token-stats account key.
 *
 * Ordinary emails keep the local part (`alice@gmail.com` → `alice`).
 * WorkBuddy keys are `{uid}@workbuddy-{realm}`. The realm is prefixed so a
 * CN account and a Global account that share a uid stay distinct in charts
 * and summaries, including when the label is end-truncated
 * (`cn:u1` vs `global:u1`).
 */
export function accountChartLabel(accountKey: string): string {
    const at = accountKey.lastIndexOf('@');
    if (at <= 0 || at === accountKey.length - 1) {
        return accountKey;
    }
    const local = accountKey.slice(0, at);
    const domain = accountKey.slice(at + 1);
    if (
        domain.startsWith(WORKBUDDY_REALM_PREFIX) &&
        domain.length > WORKBUDDY_REALM_PREFIX.length
    ) {
        return `${domain.slice(WORKBUDDY_REALM_PREFIX.length)}:${local}`;
    }
    return local;
}
