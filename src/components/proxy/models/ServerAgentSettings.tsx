import { useState } from 'react';
import type { ExperimentalConfig } from '../../../types/config';
import RegistryDialog from './RegistryDialog';

// Web/headless parity: these are gateway controls, not local binary patches.
export default function ServerAgentSettings({ config, onChange }: {
    config?: ExperimentalConfig;
    onChange: (updates: Partial<ExperimentalConfig>) => void;
}) {
    const [confirmManual, setConfirmManual] = useState(false);
    const [accepted, setAccepted] = useState(false);
    const automatic = config?.enable_cowork_auto_compact ?? false;
    return <div className="space-y-4">
        <p className="registry-muted">服务端识别的 Claude Cowork 会话专用，不是所有 Agent 或模型的通用压缩。AG 请求链路已接入；WB 尚未接入。Web / headless 仍可保存这些网关设置。 / Server-side Claude Cowork controls on the AG path, not universal compression; not integrated with WB.</p>
        <label className="flex items-start gap-2 text-sm"><input type="checkbox" checked={automatic} onChange={e => onChange({ enable_cowork_auto_compact: e.target.checked })} /><span>Cowork 自动响应式压缩 / Auto reactive compact</span></label>
        <p className="registry-muted">仅在服务端识别 Cowork 工具特征且上下文达到门限时触发；总结请求豁免。默认关闭。 / Applies to recognized Cowork traffic at the threshold; summarization requests are exempt. Off by default.</p>
        <label className="block space-y-1 text-xs font-medium">触发门限 / Token threshold
            <input type="number" className="registry-input" min={50000} max={500000} step={10000} disabled={!automatic} value={config?.cowork_compact_threshold ?? 200000}
                onChange={e => { const value = Number(e.target.value); if (Number.isFinite(value)) onChange({ cowork_compact_threshold: Math.min(500000, Math.max(50000, Math.trunc(value))) }); }} />
        </label>
        <label className="flex items-start gap-2 text-sm"><input type="checkbox" checked={config?.enable_cowork_manual_compact ?? false} onChange={e => {
            if (e.target.checked) { setAccepted(false); setConfirmManual(true); }
            else onChange({ enable_cowork_manual_compact: false });
        }} /><span>Cowork 手动深度归档（实验性） / Manual deep compact (experimental)</span></label>
        <p className="registry-muted">启用后识别会话中的 <code>./compact</code>；历史可能被归档，摘要可能丢失细节。此开关不安装补丁，也不承诺 8k 上下文。 / Recognizes ./compact; archived history and summaries can lose details. No patch is installed and no 8k budget is promised.</p>
        <p className="registry-notice">Web 隐藏的仅是本机 Desktop 补丁检查、注入和重启操作。需要时请在客户端所在设备的桌面应用管理。 / Only local Desktop patch/check/restart tools are hidden on Web.</p>
        {confirmManual && <RegistryDialog title="启用实验性深度归档 / Enable deep compact" confirmText="接受风险 / Accept risk" blocked={!accepted} onConfirm={() => { onChange({ enable_cowork_manual_compact: true }); setConfirmManual(false); }} onCancel={() => setConfirmManual(false)}>
            <p>此功能可改变 Cowork 会话的历史结构；归档后的摘要不保证完整保留细节。请先备份重要内容。启用后仍需点击「保存高级设置」。 / This changes Cowork history; summaries may omit details. Back up important content. Save advanced settings after enabling.</p>
            <label className="flex items-start gap-2"><input type="checkbox" checked={accepted} onChange={e => setAccepted(e.target.checked)} />我理解归档风险 / I understand the archival risk</label>
        </RegistryDialog>}
    </div>;
}
