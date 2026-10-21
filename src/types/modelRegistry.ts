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
export interface VerifyModelCandidatesRequest {
    candidate_ids: string[];
    confirm_upstream_usage: boolean;
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
