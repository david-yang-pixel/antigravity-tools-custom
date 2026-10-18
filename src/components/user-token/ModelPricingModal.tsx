import { useEffect, useState } from 'react';
import { Plus, RefreshCw, Trash2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../../utils/request';
import { showToast } from '../common/ToastContainer';
import { ModelPrice, parseYuan, toYuan } from './money';

type Mode = ModelPrice['mode'];
interface Row { key: number; pattern: string; mode: Mode; call: string; input: string; output: string; cached: string; }

interface Props {
    open: boolean;
    models: string[];
    onClose: () => void;
}

const field = 'input input-bordered input-sm w-full';
let nextKey = 0;

function toRow(price: ModelPrice): Row {
    const row: Row = { key: nextKey++, pattern: price.pattern, mode: price.mode, call: '', input: '', output: '', cached: '' };
    if (price.mode === 'per_call') row.call = toYuan(price.call_price_micros);
    else {
        row.input = toYuan(price.input_price_micros);
        row.output = toYuan(price.output_price_micros);
        row.cached = price.cached_price_micros == null ? '' : toYuan(price.cached_price_micros);
    }
    return row;
}

/** Preview only; the server decides with the same rule (exact case, then case-insensitive). */
function wildcard(pattern: string, model: string): boolean {
    const source = pattern.split('*').map(part => part.replace(/[.+?^${}()|[\]\\]/g, '\\$&')).join('.*');
    return new RegExp(`^${source}$`, 'i').test(model);
}

/** Per-model prices for balance-billed keys: each rule is either per call or per token. */
export default function ModelPricingModal({ open, models, onClose }: Props) {
    const { t } = useTranslation();
    const [rows, setRows] = useState<Row[] | null>(null);
    const [saving, setSaving] = useState(false);

    useEffect(() => {
        if (!open) return;
        setRows(null);
        invoke<ModelPrice[]>('get_model_prices').then(prices => setRows(prices.map(toRow))).catch(e => showToast(String(e), 'error'));
    }, [open]);

    if (!open) return null;
    const update = (key: number, patch: Partial<Row>) => setRows(current => current && current.map(row => row.key === key ? { ...row, ...patch } : row));
    const free = rows ? models.filter(model => !rows.some(row => row.pattern.trim() && wildcard(row.pattern.trim(), model))) : [];

    function build(): ModelPrice[] | null {
        const prices: ModelPrice[] = [];
        for (const row of rows ?? []) {
            const pattern = row.pattern.trim();
            if (!pattern) { showToast(t('billing.pattern_required'), 'error'); return null; }
            if (row.mode === 'per_call') {
                const call = parseYuan(row.call);
                if (call === null) { showToast(t('billing.invalid_price', { pattern }), 'error'); return null; }
                prices.push({ pattern, mode: 'per_call', call_price_micros: call });
            } else {
                const input = parseYuan(row.input);
                const output = parseYuan(row.output);
                const cached = row.cached.trim() ? parseYuan(row.cached) : null;
                if (input === null || output === null || (row.cached.trim() && cached === null)) {
                    showToast(t('billing.invalid_price', { pattern }), 'error'); return null;
                }
                prices.push({ pattern, mode: 'per_token', input_price_micros: input, output_price_micros: output, cached_price_micros: cached });
            }
        }
        return prices;
    }

    async function save() {
        const prices = build();
        if (!prices) return;
        setSaving(true);
        try {
            const saved = await invoke<ModelPrice[]>('save_model_prices', { request: { prices } });
            setRows(saved.map(toRow));
            showToast(t('common.saved') || t('common.update_success'), 'success');
        } catch (e) {
            showToast(String(e), 'error');
        } finally {
            setSaving(false);
        }
    }

    const priceInput = (row: Row, key: 'call' | 'input' | 'output' | 'cached', label: string, placeholder = '') => (
        <label className="text-xs text-gray-500 space-y-1 min-w-0">
            <span>{label}</span>
            <input type="number" min="0" step="0.000001" className={field} value={row[key]} placeholder={placeholder}
                onChange={e => update(row.key, { [key]: e.target.value })} />
        </label>
    );

    return (
        <div className="modal modal-open">
            <div className="modal-box max-w-3xl">
                <h3 className="font-bold text-lg">{t('billing.model_pricing')}</h3>
                <p className="text-xs text-gray-500 mt-1 mb-4">{t('billing.model_pricing_hint')}</p>
                {!rows ? <div className="py-12 flex justify-center"><RefreshCw className="animate-spin text-gray-400" /></div> : <div className="space-y-3">
                    <datalist id="pricing-models">{models.map(model => <option key={model} value={model} />)}</datalist>
                    {rows.map(row => <div key={row.key} className="border border-gray-200 dark:border-base-300 rounded-xl p-3 space-y-3">
                        <div className="flex flex-wrap items-center gap-2">
                            <label className="sr-only" htmlFor={`pricing-pattern-${row.key}`}>{t('billing.pattern')}</label>
                            <input id={`pricing-pattern-${row.key}`} list="pricing-models" className={`${field} font-mono flex-1 min-w-48`} value={row.pattern}
                                placeholder={t('billing.pattern_placeholder')} onChange={e => update(row.key, { pattern: e.target.value })} />
                            <div className="inline-flex rounded-lg border border-gray-200 dark:border-base-300 p-0.5" role="group" aria-label={t('billing.mode')}>
                                {(['per_token', 'per_call'] as const).map(mode => <button key={mode} type="button" aria-pressed={row.mode === mode} onClick={() => update(row.key, { mode })}
                                    className={`rounded-md px-3 py-1 text-xs ${row.mode === mode ? 'bg-blue-500 text-white' : 'text-gray-600 dark:text-gray-300 hover:bg-gray-100 dark:hover:bg-base-200'}`}>
                                    {t(`billing.mode_${mode}`)}
                                </button>)}
                            </div>
                            <button type="button" onClick={() => setRows(rows.filter(item => item.key !== row.key))} title={t('common.delete')}
                                className="p-1.5 hover:bg-red-50 dark:hover:bg-red-900/20 rounded-lg text-gray-400 hover:text-red-500"><Trash2 size={14} /></button>
                        </div>
                        {row.mode === 'per_call'
                            ? <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">{priceInput(row, 'call', t('billing.call_price'))}</div>
                            : <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
                                {priceInput(row, 'input', t('billing.input_price'))}
                                {priceInput(row, 'output', t('billing.output_price'))}
                                {priceInput(row, 'cached', t('billing.cached_price'), t('billing.cached_price_placeholder'))}
                            </div>}
                    </div>)}
                    {rows.length === 0 && <p className="text-sm text-gray-500 text-center py-6">{t('billing.no_prices')}</p>}
                    <button type="button" onClick={() => setRows([...rows, { key: nextKey++, pattern: '', mode: 'per_token', call: '', input: '', output: '', cached: '' }])}
                        className="text-sm text-blue-600 flex items-center gap-1"><Plus size={14} />{t('billing.add_rule')}</button>
                    <p className="text-xs text-gray-500">{t('billing.match_hint')}</p>
                    {free.length > 0 && <details className="text-xs text-amber-700 dark:text-amber-300">
                        <summary className="cursor-pointer">{t('billing.free_models', { count: free.length })}</summary>
                        <p className="mt-2 font-mono break-all text-gray-500">{free.join(', ')}</p>
                    </details>}
                </div>}
                <div className="modal-action">
                    <button className="px-4 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm transition-colors" onClick={onClose}>{t('common.close', { defaultValue: 'Close' })}</button>
                    <button className="px-4 py-2 bg-blue-500 hover:bg-blue-600 text-white text-sm font-medium rounded-lg flex items-center gap-2 disabled:opacity-50" onClick={() => void save()} disabled={!rows || saving}>
                        {saving && <RefreshCw size={14} className="animate-spin" />}{t('common.save', { defaultValue: 'Save' })}
                    </button>
                </div>
            </div>
        </div>
    );
}
