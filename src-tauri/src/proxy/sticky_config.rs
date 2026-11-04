use serde::{Deserialize, Serialize};

/// 调度模式枚举
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SchedulingMode {
    /// 缓存优先 (Cache-first): 尽可能锁定同一账号，限流时优先等待，极大提升 Prompt Caching 命中率
    CacheFirst,
    /// 平衡模式 (Balance): 锁定同一账号，限流时立即切换到备选账号，兼顾成功率和性能
    Balance,
    /// 性能优先 (Performance-first): 纯轮询模式 (Round-robin)，账号负载最均衡，但不利用缓存
    PerformanceFirst,
}

impl Default for SchedulingMode {
    fn default() -> Self {
        Self::Balance
    }
}

/// 粘性会话配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StickySessionConfig {
    /// 当前调度模式
    pub mode: SchedulingMode,
    /// 缓存优先模式下的最大等待时间 (秒)
    pub max_wait_seconds: u64,
}

impl Default for StickySessionConfig {
    fn default() -> Self {
        Self {
            mode: SchedulingMode::Balance,
            max_wait_seconds: 60,
        }
    }
}

/// Account pools share the live scheduling controls; each provider owns its fixed account.
#[derive(Clone, Default)]
pub struct AccountSchedulingPolicy {
    pub sticky: StickySessionConfig,
    pub preferred: Option<String>,
    pub breaker: crate::models::CircuitBreakerConfig,
}

impl AccountSchedulingPolicy {
    pub async fn live(manager: &super::TokenManager, preferred: Option<String>) -> Self {
        Self {
            sticky: manager.get_sticky_config().await,
            breaker: manager.get_circuit_breaker_config().await,
            preferred,
        }
    }

    /// The half-open probe holds the model's only admission slot, so it must not wait
    /// for a busy or cooling session account while other accounts are free.
    pub fn for_probe(&self) -> Self {
        let mut policy = self.clone();
        if policy.sticky.mode == SchedulingMode::CacheFirst {
            policy.sticky.mode = SchedulingMode::Balance;
        }
        policy.sticky.max_wait_seconds = 0;
        policy
    }
}

pub fn account_backoff_seconds(
    breaker: &crate::models::CircuitBreakerConfig,
    consecutive: u64,
    minimum: i64,
) -> i64 {
    let step = breaker
        .backoff_steps
        .get(
            (consecutive.saturating_sub(1) as usize)
                .min(breaker.backoff_steps.len().saturating_sub(1)),
        )
        .copied()
        .unwrap_or(60)
        .min(86400) as i64;
    minimum.max(step).clamp(1, 86400)
}
