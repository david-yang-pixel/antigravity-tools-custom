// JSON wire DTOs. Keep snake_case in sync with the model registry command contract.
export type CapabilityStatus = 'supported' | 'conditional' | 'not_integrated' | 'unsupported' | 'unverified';
export interface ModelCapability {
    key: string;
    label: string;
    status: CapabilityStatus;
    reason: string;
}
export interface ModelSource {
    id: string;
    provider_type: string;
    display_name: string;
    enabled: boolean;
    account_count: number;
}
export interface CandidateModel {
    id: string;
    source_id: string;
    provider_type: string;
    native_model: string;
    display_name: string;
    account_count: number;
    verified_account_count: number;
    verification_status: 'unverified' | 'verified' | 'unavailable' | 'failed';
    last_verified_at: number | null;
    ignored: boolean;
    capabilities: ModelCapability[];
}
export interface PublishedModel {
    id: string;
    public_id: string;
    display_name: string;
    source_id: string;
    native_model: string;
    enabled: boolean;
    legacy: boolean;
}
export interface ModelRegistryView {
    schema_version: number;
    revision: number;
    sources: ModelSource[];
    candidates: CandidateModel[];
    published_models: PublishedModel[];
    notices: string[];
}
export interface SaveModelRegistryRequest {
    revision: number;
    published_models: PublishedModel[];
    ignored_candidate_ids: string[];
    allow_unverified?: boolean;
}
export interface RefreshModelCandidatesRequest { source_id?: string | null }
// WorkBuddy region: cn = domestic, global = international.
export type WorkBuddyRealm = 'cn' | 'global';
export interface DiscoverUpstreamModelsRequest {
    source_id: string;
    realm?: WorkBuddyRealm | null;
}
export interface UpstreamDiscoveryReport {
    source_id: string;
    realm: WorkBuddyRealm | null;
    account_id: string;
    account_label: string;
    attempts: number;
    candidate_ids: string[];
    registry: ModelRegistryView;
}
/** Administrator-written test content; WorkBuddy only, sent once and never stored. */
export interface VerificationPrompt {
    system: string;
    user: string;
}
export interface VerifyModelCandidatesRequest {
    candidate_ids: string[];
    confirm_upstream_usage: boolean;
    account_id?: string | null;
    prompt?: VerificationPrompt | null;
}
export interface ModelVerificationResult {
    candidate_id: string;
    status: 'verified' | 'unavailable' | 'failed';
    message: string;
    checked_at: number;
    account_id: string | null;
}
export interface ModelVerificationReport {
    results: ModelVerificationResult[];
    registry: ModelRegistryView;
}

// Protocol availability describes gateway routing, not native capability evidence.
export type RegistryProtocol = 'openai' | 'anthropic' | 'gemini';
export interface RegistryCatalogModel extends PublishedModel {
    provider_type: string;
    source_name: string;
    protocols: RegistryProtocol[];
    image_generation: boolean;
}

// Administrator-defined OpenAI-compatible upstreams. API keys are write-only: the view
// reports only whether one is set and its last characters.
export interface CustomUpstream {
    id: string;
    source_id: string;
    name: string;
    base_url: string;
    enabled: boolean;
    models: string[];
    api_key_set: boolean;
    api_key_hint: string;
    catalog_count: number;
}
export interface CustomUpstreamSettings {
    upstreams: CustomUpstream[];
    env_locked: boolean;
}
export interface CustomUpstreamUpdate {
    id: string;
    name: string;
    base_url: string;
    enabled: boolean;
    models: string[];
    /** Omit to keep the stored key; an empty string clears it. */
    api_key?: string;
}
