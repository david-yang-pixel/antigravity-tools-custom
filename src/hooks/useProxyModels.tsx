import { useMemo, useEffect, useState, useCallback } from 'react';
import { MODEL_CONFIG, compareModelsDesc, inferModelGroup } from '../config/modelConfig';
import { Bot, Sparkles } from 'lucide-react';
import { request } from '../utils/request';
import type { ModelRegistryView } from '../types/modelRegistry';
import { registryCatalog } from '../components/proxy/models/registryLogic';

export const useProxyModels = () => {
    const [registry, setRegistry] = useState<ModelRegistryView | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(false);
    const [revision, setRevision] = useState(0);
    const refresh = useCallback(() => setRevision(value => value + 1), []);

    useEffect(() => {
        window.addEventListener('model-registry-changed', refresh);
        return () => window.removeEventListener('model-registry-changed', refresh);
    }, [refresh]);

    useEffect(() => {
        let cancelled = false;
        setLoading(true);
        setError(false);
        // Fail closed while reloading: never retain an unpublished/stale fallback.
        setRegistry(null);
        request<ModelRegistryView>('get_model_registry')
            .then(view => { if (!cancelled) setRegistry(view); })
            .catch(() => { if (!cancelled) { setRegistry(null); setError(true); } })
            .finally(() => { if (!cancelled) setLoading(false); });
        return () => { cancelled = true; };
    }, [revision]);

    const models = useMemo(() => (registry ? registryCatalog(registry) : []).map(model => {
        // Hardcoded catalog supplies icons/order only, never publication or routing.
        const cfg = MODEL_CONFIG[model.native_model];
        const group = cfg?.group || inferModelGroup(model.native_model);
        const Icon = cfg?.Icon;
        return {
            ...model,
            registry_id: model.id,
            id: model.public_id,
            name: model.display_name || model.public_id,
            desc: `${model.source_name} → ${model.native_model}`,
            group,
            icon: Icon ? <Icon size={16} /> : group === 'Claude'
                ? <Sparkles size={16} className="text-purple-400" />
                : <Bot size={16} className="text-blue-400" />,
        };
    }).sort(compareModelsDesc), [registry]);

    return { models, loading, error, refresh };
};
