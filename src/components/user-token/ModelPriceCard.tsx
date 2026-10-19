import { useTranslation } from 'react-i18next';
import { formatYuan, Price } from './money';

interface Props {
    model: string;
    price?: Price;
    /** Explicitly granted free access for this key; missing prices never imply free. */
    free: boolean;
}

/** One available model in the usage portal, with its price broken into lines. */
export default function ModelPriceCard({ model, price, free }: Props) {
    const { t } = useTranslation();
    const line = (label: string, micros: number, unit: string) => <div key={label} className="flex items-baseline justify-between gap-3">
        <dt className="text-slate-500">{label}</dt>
        <dd className="tabular-nums text-slate-800">{formatYuan(micros)} <span className="text-xs text-slate-400">{unit}</span></dd>
    </div>;
    const badge = free
        ? <span className="shrink-0 rounded-full bg-emerald-50 px-2 py-0.5 text-xs text-emerald-700">{t('billing.free')}</span>
        : price
            ? <span className="shrink-0 rounded-full bg-blue-50 px-2 py-0.5 text-xs text-blue-700">{t(`billing.mode_${price.mode}`)}</span>
            : <span className="shrink-0 rounded-full bg-slate-100 px-2 py-0.5 text-xs text-slate-500">{t('billing.unavailable')}</span>;
    return (
        <div className="bg-white border border-slate-200 rounded-xl p-4 min-w-0">
            <div className="flex items-start justify-between gap-3">
                <span className="font-mono text-sm font-medium break-all">{model}</span>
                {badge}
            </div>
            {!free && price && <dl className="mt-3 space-y-1.5 text-sm">
                {price.mode === 'per_call'
                    ? line(t('billing.per_call_label'), price.call_price_micros, t('billing.per_call_unit'))
                    : [
                        line(t('billing.input'), price.input_price_micros, t('billing.per_million_tokens')),
                        ...(price.cached_price_micros == null ? [] : [line(t('billing.cached'), price.cached_price_micros, t('billing.per_million_tokens'))]),
                        line(t('billing.output'), price.output_price_micros, t('billing.per_million_tokens')),
                    ]}
            </dl>}
            {!free && !price && <p className="mt-3 text-xs text-slate-500">{t('billing.unpriced_unavailable')}</p>}
        </div>
    );
}
