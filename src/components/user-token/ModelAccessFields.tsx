import { useState } from 'react';
import { useTranslation } from 'react-i18next';

interface Props {
    prefix: string;
    selected: string[];
    models: string[];
    loadError: boolean;
    onSelectedChange: (models: string[]) => void;
}

export default function ModelAccessFields(props: Props) {
    const { t } = useTranslation();
    const [search, setSearch] = useState('');
    const choices = [...new Set([...props.models, ...props.selected])].sort();
    const filtered = choices.filter(model => model.toLowerCase().includes(search.toLowerCase()));
    const custom = search.trim();
    return (
        <fieldset className="border border-gray-200 dark:border-base-300 rounded-xl p-3 mb-4">
            <legend className="px-1 text-sm font-medium">{t('user_token.free_models')}</legend>
            <label htmlFor={`${props.prefix}-model-search`} className="sr-only">{t('user_token.search_models')}</label>
            <input id={`${props.prefix}-model-search`} className="input input-bordered input-sm w-full mb-2" value={search}
                onChange={e => setSearch(e.target.value)} placeholder={t('user_token.search_models')} />
            {props.loadError && <p role="status" className="text-xs text-amber-700 mb-2">{t('user_token.model_load_error')}</p>}
            <div className="max-h-40 overflow-y-auto space-y-1">
                {filtered.map(model => <label key={model} className="flex items-start gap-2 text-xs py-1 cursor-pointer">
                    <input type="checkbox" checked={props.selected.includes(model)} className="mt-0.5"
                        onChange={e => props.onSelectedChange(e.target.checked ? [...props.selected, model] : props.selected.filter(m => m !== model))} />
                    <span className="font-mono break-all">{model}</span>
                </label>)}
                {filtered.length === 0 && <p className="text-xs text-gray-500 py-1">{t('user_token.no_models_found')}</p>}
            </div>
            {custom && !choices.includes(custom) && <button type="button" className="text-xs text-blue-600 mt-2 break-all"
                onClick={() => { props.onSelectedChange([...props.selected, custom]); setSearch(''); }}>
                {t('user_token.add_model', { model: custom })}
            </button>}
            <p className="text-xs text-gray-500 mt-2">{t('user_token.free_models_selected', { count: props.selected.length })}</p>
            <p className="text-xs text-gray-500 mt-2">{t('user_token.free_models_hint')}</p>
        </fieldset>
    );
}
