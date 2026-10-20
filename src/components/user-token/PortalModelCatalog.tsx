import { useRef, useState } from 'react';
import { Search, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import ModelPriceCard from './ModelPriceCard';
import type { Price } from './money';

interface Props {
    models: string[];
    prices: (Price & { model: string })[];
    freeModels: string[];
}

// Group by model family, not the gateway account or transport provider. Match
// prefixes so new versions stay together; unknown/custom aliases remain visible.
const families = [
    { name: 'Claude', pattern: /^claude(?:[-_.]|$)/i },
    { name: 'Gemini', pattern: /^gemini(?:[-_.]|$)/i },
    { name: 'GPT / OpenAI', pattern: /^(?:gpt|o\d+)(?:[-_.]|$)/i },
    { name: 'DeepSeek', pattern: /^deepseek(?:[-_.]|$)/i },
    { name: 'GLM', pattern: /^glm(?:[-_.]|$)/i },
    { name: 'Kimi', pattern: /^kimi(?:[-_.]|$)/i },
    { name: 'Qwen', pattern: /^qwen(?:\d|[-_.]|$)/i },
    { name: 'MiniMax', pattern: /^minimax(?:[-_.]|$)/i },
    { name: 'MiMo', pattern: /^mimo(?:[-_.]|$)/i },
    { name: 'Hunyuan', pattern: /^(?:hunyuan(?:[-_.]|$)|hy\d)/i },
    { name: 'Step', pattern: /^step(?:[-_.]|$)/i },
    { name: 'Grok', pattern: /^grok(?:[-_.]|$)/i },
];

export default function PortalModelCatalog({ models, prices, freeModels }: Props) {
    const { t } = useTranslation();
    const [input, setInput] = useState('');
    const [query, setQuery] = useState('');
    const searchInput = useRef<HTMLInputElement>(null);
    const groups = [...families.map(family => ({ name: family.name, models: [] as string[] })),
        { name: t('token_portal.other_models'), models: [] as string[] }];
    const keyword = query.toLowerCase();
    for (const model of models) {
        const nativeId = model.split('/').pop() ?? model;
        const index = families.findIndex(family => family.pattern.test(nativeId));
        const group = groups[index < 0 ? families.length : index];
        if (model.toLowerCase().includes(keyword) || group.name.toLowerCase().includes(keyword)) group.models.push(model);
    }
    const visibleGroups = groups.filter(group => group.models.length > 0);
    const visibleCount = visibleGroups.reduce((count, group) => count + group.models.length, 0);
    const pricesByModel = new Map(prices.map(price => [price.model, price]));
    const freeIds = new Set(freeModels.map(model => model.replace(/^models\//, '')));

    function clearSearch() {
        setInput(''); setQuery(''); searchInput.current?.focus();
    }

    if (!models.length) return <p className="text-sm text-slate-500">{t('token_portal.empty_catalog')}</p>;

    return <div className="space-y-6 pt-1">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <form role="search" aria-label={t('token_portal.catalog_search')} className="flex w-full gap-2 sm:max-w-lg"
                onSubmit={event => { event.preventDefault(); setQuery(input.trim()); }}>
                <label className="sr-only" htmlFor="portal-model-search">{t('token_portal.catalog_search')}</label>
                <div className="relative min-w-0 flex-1">
                    <Search size={16} aria-hidden="true" className="pointer-events-none absolute left-3 top-3 text-slate-400" />
                    <input ref={searchInput} id="portal-model-search" type="search" value={input}
                        onChange={event => { setInput(event.target.value); if (!event.target.value.trim()) setQuery(''); }}
                        placeholder={t('token_portal.catalog_search_placeholder')}
                        className="w-full rounded-lg border border-slate-200 bg-white py-2.5 pl-9 pr-3 text-sm outline-none focus:border-blue-500 focus:ring-2 focus:ring-blue-100" />
                </div>
                <button type="submit" className="shrink-0 rounded-lg bg-blue-600 px-4 py-2.5 text-sm font-medium text-white hover:bg-blue-700 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 focus-visible:ring-offset-2">{t('token_portal.catalog_search_button')}</button>
                {(input || query) && <button type="button" onClick={clearSearch} aria-label={t('token_portal.catalog_clear_search')} title={t('token_portal.catalog_clear_search')}
                    className="shrink-0 rounded-lg border border-slate-200 bg-white px-3 text-slate-500 hover:text-slate-900 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500"><X size={16} aria-hidden="true" /></button>}
            </form>
            <p role="status" className="text-xs text-slate-500">{t('token_portal.catalog_result_count', { count: visibleCount, total: models.length })}</p>
        </div>
        {visibleGroups.map((group, index) => <section key={group.name} aria-labelledby={`portal-model-family-${index}`} className="space-y-3">
            <div className="flex items-center gap-3">
                <h3 id={`portal-model-family-${index}`} className="text-base font-semibold text-slate-800">{group.name}</h3>
                <span className="rounded-full bg-slate-100 px-2 py-0.5 text-xs tabular-nums text-slate-500">{group.models.length}</span>
                <div aria-hidden="true" className="h-px flex-1 bg-slate-200" />
            </div>
            <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
                {group.models.map(model => <ModelPriceCard key={model} model={model} price={pricesByModel.get(model)} free={freeIds.has(model.replace(/^models\//, ''))} />)}
            </div>
        </section>)}
        {visibleCount === 0 && <div className="rounded-xl border border-dashed border-slate-200 bg-white px-5 py-10 text-center">
            <p className="text-sm text-slate-500">{t('token_portal.catalog_no_matches')}</p>
            <button type="button" onClick={clearSearch} className="mt-3 rounded px-2 py-1 text-sm text-blue-600 hover:text-blue-700 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500">{t('token_portal.catalog_clear_search')}</button>
        </div>}
    </div>;
}
