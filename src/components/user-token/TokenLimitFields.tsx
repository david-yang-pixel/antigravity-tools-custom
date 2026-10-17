import { useTranslation } from 'react-i18next';

import { parseQuotaMillions, toQuotaMillions } from './tokenQuota';

interface Props {
    prefix: string;
    maxConcurrency: string;
    tokenQuota: string;
    onMaxConcurrencyChange: (value: string) => void;
    onTokenQuotaChange: (value: string) => void;
    usedTokens?: number;
    /** Overrides the quota hint, e.g. for registered users where 0 means "not activated". */
    quotaHint?: string;
}

export default function TokenLimitFields(props: Props) {
    const { t } = useTranslation();
    const quota = parseQuotaMillions(props.tokenQuota);
    const exhausted = props.usedTokens !== undefined && quota !== null && quota > 0
        && props.usedTokens >= quota;
    return (
        <div className="mb-3">
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                <div className="form-control w-full">
                    <label className="label" htmlFor={`${props.prefix}-max-concurrency`}>
                        <span className="label-text">{t('user_token.max_concurrency')}</span>
                    </label>
                    <input id={`${props.prefix}-max-concurrency`} type="number" min="0" max="2147483647" step="1"
                        className="input input-bordered w-full" value={props.maxConcurrency}
                        onChange={e => props.onMaxConcurrencyChange(e.target.value)} />
                    <p className="text-xs text-gray-500 mt-1">{t('user_token.concurrency_hint')}</p>
                </div>
                <div className="form-control w-full">
                    <label className="label" htmlFor={`${props.prefix}-token-quota`}>
                        <span className="label-text">{t('user_token.token_quota')}</span>
                    </label>
                    <input id={`${props.prefix}-token-quota`} type="number" min="0" max="9007199254.740991" step="0.000001"
                        className="input input-bordered w-full" value={props.tokenQuota}
                        onChange={e => props.onTokenQuotaChange(e.target.value)} />
                    <p className="text-xs text-gray-500 mt-1">{props.quotaHint ?? t('user_token.quota_hint')}</p>
                </div>
            </div>
            <p className="text-xs text-gray-500 mt-2">{t('user_token.quota_settlement_hint')}</p>
            {props.usedTokens !== undefined && (
                <p className={`text-xs mt-2 ${exhausted ? 'text-red-500' : 'text-gray-500'}`}>
                    {t('user_token.used_tokens_hint', { amount: toQuotaMillions(props.usedTokens) })}
                    {exhausted && ` ${t('user_token.quota_edit_exhausted')}`}
                </p>
            )}
        </div>
    );
}
