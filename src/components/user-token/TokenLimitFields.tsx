import { useTranslation } from 'react-i18next';
import { toQuotaMillions } from './tokenQuota';

interface Props {
    prefix: string;
    maxConcurrency: string;
    onMaxConcurrencyChange: (value: string) => void;
    usedTokens?: number;
}

export default function TokenLimitFields(props: Props) {
    const { t } = useTranslation();
    return <div className="mb-3">
        <div className="form-control w-full">
            <label className="label" htmlFor={`${props.prefix}-max-concurrency`}>
                <span className="label-text">{t('user_token.max_concurrency')}</span>
            </label>
            <input id={`${props.prefix}-max-concurrency`} type="number" min="0" max="2147483647" step="1"
                className="input input-bordered w-full" value={props.maxConcurrency}
                onChange={e => props.onMaxConcurrencyChange(e.target.value)} />
            <p className="text-xs text-gray-500 mt-1">{t('user_token.concurrency_hint')}</p>
        </div>
        {props.usedTokens !== undefined && <p className="text-xs text-gray-500 mt-2">
            {t('user_token.used_tokens_hint', { amount: toQuotaMillions(props.usedTokens) })}
        </p>}
    </div>;
}
