import { useTranslation } from 'react-i18next';
import { formatYuan } from './money';

interface Props {
    prefix: string;
    billed: boolean;
    balance: string;
    onBilledChange: (billed: boolean) => void;
    onBalanceChange: (value: string) => void;
    /** Balance when the dialog opened, for edits. */
    currentBalance?: number | null;
}

/** Balance billing switch and amount for a key; amounts are entered in yuan. */
export default function BalanceFields(props: Props) {
    const { t } = useTranslation();
    return (
        <fieldset className="border border-gray-200 dark:border-base-300 rounded-xl p-3 mb-4">
            <legend className="px-1 text-sm font-medium">{t('billing.balance_billing')}</legend>
            <label className="flex items-center justify-between gap-4 cursor-pointer">
                <span className="text-xs text-gray-500">{t('billing.balance_billing_hint')}</span>
                <input type="checkbox" className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600"
                    checked={props.billed} onChange={e => props.onBilledChange(e.target.checked)} />
            </label>
            {props.billed && <div className="form-control w-full mt-3">
                <label className="label py-1" htmlFor={`${props.prefix}-balance`}>
                    <span className="label-text">{t('billing.balance_yuan')}</span>
                </label>
                <input id={`${props.prefix}-balance`} type="number" min="0" max="1000000000" step="0.01"
                    className="input input-bordered w-full" value={props.balance}
                    onChange={e => props.onBalanceChange(e.target.value)} />
                <p className="text-xs text-gray-500 mt-1">
                    {props.currentBalance !== undefined && props.currentBalance !== null && `${t('billing.current_balance', { amount: formatYuan(props.currentBalance) })} `}
                    {t('billing.balance_edit_hint')}
                </p>
            </div>}
        </fieldset>
    );
}
