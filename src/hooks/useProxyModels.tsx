import { useMemo, useEffect, useState, useCallback } from 'react';
import { MODEL_CONFIG, compareModelsDesc, inferModelGroup } from '../config/modelConfig';
import { useAccountStore } from '../stores/useAccountStore';
import { useConfigStore } from '../stores/useConfigStore';
import { Bot, Sparkles } from 'lucide-react';
import { request } from '../utils/request';

export interface CanonicalFamilyDto {
    canonical_id: string;
    display_name: string;
    match_ids: string[];
}

export const useProxyModels = () => {
    const { accounts, fetchAccounts } = useAccountStore();
    const onlyRaw = useConfigStore(state => state.config?.proxy.only_raw_quota_models);
    const [canonicalFamilies, setCanonicalFamilies] = useState<CanonicalFamilyDto[]>([]);
    const [modelIds, setModelIds] = useState<string[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(false);
    const [revision, setRevision] = useState(0);
    const refresh = useCallback(() => setRevision(value => value + 1), []);

    useEffect(() => {
        if (useAccountStore.getState().accounts.length === 0) {
            void fetchAccounts().catch(() => {});
        }
        let cancelled = false;
        request<CanonicalFamilyDto[]>('get_canonical_families')
            .then(data => { if (!cancelled && data) setCanonicalFamilies(data); })
            .catch(err => console.error('Failed to fetch canonical families:', err));
        return () => { cancelled = true; };
    }, [fetchAccounts]);

    useEffect(() => {
        let cancelled = false;
        setLoading(true);
        setError(false);
        // Admin choices include added models before pricing. Public APIs apply
        // capability and price filters. MODEL_CONFIG only supplies presentation.
        request<string[]>('get_user_token_models')
            .then(ids => { if (!cancelled) setModelIds(ids); })
            .catch(() => {
                if (!cancelled) { setModelIds([]); setError(true); }
            })
            .finally(() => { if (!cancelled) setLoading(false); });
        return () => { cancelled = true; };
    }, [accounts, onlyRaw, revision]);

    const models = useMemo(() => Array.from(new Set(modelIds)).map(id => {
        const cfg = MODEL_CONFIG[id];
        const group = cfg?.group || inferModelGroup(id);
        const Icon = cfg?.Icon;
        return {
            id,
            name: id,
            desc: id,
            group,
            icon: Icon ? <Icon size={16} /> : group === 'Claude'
                ? <Sparkles size={16} className="text-purple-400" />
                : <Bot size={16} className="text-blue-400" />,
        };
    }).sort(compareModelsDesc), [modelIds]);

    return { models, canonicalFamilies, loading, error, refresh };
};
