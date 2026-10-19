import { useTranslation } from 'react-i18next';
import { formatYuan } from './money';

interface Props {
    prefix: string;
    balance: string;
    onBalanceChange: (value: string) => void;
    /** Balance when the dialog opened, for edits. */
    currentBalance?: number | null;
}

/** Every key uses balance billing; amounts are entered in yuan. */
export default function BalanceFields(props: Props) {
    const { t } = useTranslation();
    return <fieldset className="border border-gray-200 dark:border-base-300 rounded-xl p-3 mb-4">
        <legend className="px-1 text-sm font-medium">{t('billing.balance')}</legend>
        <p className="text-xs text-gray-500">{t('billing.balance_billing_hint')}</p>
        <div className="form-control w-full mt-3">
            <label className="label py-1" htmlFor={`${props.prefix}-balance`}>
                <span className="label-text">{t('billing.balance_yuan')}</span>
            </label>
            <input id={`${props.prefix}-balance`} type="text" inputMode="decimal"
                className="input input-bordered w-full" value={props.balance}
                onChange={e => props.onBalanceChange(e.target.value)} />
            <p className="text-xs text-gray-500 mt-1">
                {props.currentBalance !== undefined && props.currentBalance !== null && `${t('billing.current_balance', { amount: formatYuan(props.currentBalance) })} `}
                {t('billing.balance_edit_hint')}
            </p>
        </div>
    </fieldset>;
}
