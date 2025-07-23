// Copyright 2025 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    sync::{atomic::AtomicU32, Arc},
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::rpc_retry_policy::is_filter_error;
use alloy::transports::TransportError;

/// RPC连接错误统计和恢复管理器
#[derive(Debug, Clone)]
pub struct SmartRpcManager {
    /// 连续过滤器错误计数
    consecutive_filter_errors: Arc<AtomicU32>,
    /// 最后一次重建连接的时间
    last_rebuild_time: Arc<RwLock<Option<Instant>>>,
    /// 配置参数
    config: RpcManagerConfig,
}

#[derive(Debug, Clone)]
pub struct RpcManagerConfig {
    /// 触发连接重建的连续错误阈值
    pub error_threshold: u32,
    /// 重建连接的最小间隔
    pub min_rebuild_interval: Duration,
    /// 重建后的监控窗口时间
    pub monitoring_window: Duration,
}

impl Default for RpcManagerConfig {
    fn default() -> Self {
        Self {
            error_threshold: 5,  // 连续5次错误后重建连接
            min_rebuild_interval: Duration::from_secs(30), // 最少30秒间隔
            monitoring_window: Duration::from_secs(60), // 60秒监控窗口
        }
    }
}

impl SmartRpcManager {
    /// 创建新的RPC管理器
    pub fn new(config: RpcManagerConfig) -> Self {
        Self {
            consecutive_filter_errors: Arc::new(AtomicU32::new(0)),
            last_rebuild_time: Arc::new(RwLock::new(None)),
            config,
        }
    }

    /// 使用默认配置创建管理器
    pub fn new_default() -> Self {
        Self::new(RpcManagerConfig::default())
    }

    /// 报告一个错误，返回是否需要重建连接
    pub async fn report_error(&self, error: &TransportError) -> bool {
        if is_filter_error(error) {
            let count = self.consecutive_filter_errors.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            
            warn!(
                "🔥 RPC过滤器错误 #{}: {:?}",
                count, 
                error
            );

            if count >= self.config.error_threshold {
                if self.should_rebuild_connection().await {
                    info!(
                        "🔄 连续{}次过滤器错误，触发RPC连接重建",
                        count
                    );
                    return true;
                }
            }
        } else {
            // 非过滤器错误，重置计数器
            self.reset_error_count();
        }
        
        false
    }

    /// 报告成功操作，重置错误计数
    pub fn report_success(&self) {
        let prev_count = self.consecutive_filter_errors.swap(0, std::sync::atomic::Ordering::SeqCst);
        if prev_count > 0 {
            info!(
                "✅ RPC操作成功，重置错误计数 (之前: {})",
                prev_count
            );
        }
    }

    /// 标记连接已重建
    pub async fn mark_connection_rebuilt(&self) {
        let mut last_rebuild = self.last_rebuild_time.write().await;
        *last_rebuild = Some(Instant::now());
        
        // 重置错误计数
        let prev_count = self.consecutive_filter_errors.swap(0, std::sync::atomic::Ordering::SeqCst);
        
        info!(
            "🔄 RPC连接已重建，重置错误计数 (之前: {})",
            prev_count
        );
    }

    /// 获取当前错误计数
    pub fn current_error_count(&self) -> u32 {
        self.consecutive_filter_errors.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 检查是否应该重建连接
    async fn should_rebuild_connection(&self) -> bool {
        let last_rebuild = self.last_rebuild_time.read().await;
        
        match *last_rebuild {
            Some(last_time) => {
                let elapsed = last_time.elapsed();
                if elapsed < self.config.min_rebuild_interval {
                    warn!(
                        "⏰ 距离上次重建仅{}秒，跳过重建 (最小间隔: {}秒)",
                        elapsed.as_secs(),
                        self.config.min_rebuild_interval.as_secs()
                    );
                    return false;
                }
            }
            None => {}
        }
        
        true
    }

    /// 重置错误计数
    fn reset_error_count(&self) {
        let prev_count = self.consecutive_filter_errors.swap(0, std::sync::atomic::Ordering::SeqCst);
        if prev_count > 0 {
            info!(
                "🔄 非过滤器错误，重置RPC错误计数 (之前: {})",
                prev_count
            );
        }
    }

    /// 获取状态信息
    pub async fn get_status(&self) -> RpcManagerStatus {
        let last_rebuild = self.last_rebuild_time.read().await;
        RpcManagerStatus {
            consecutive_errors: self.current_error_count(),
            error_threshold: self.config.error_threshold,
            last_rebuild_time: *last_rebuild,
            time_since_last_rebuild: last_rebuild.map(|t| t.elapsed()),
        }
    }
}

#[derive(Debug)]
pub struct RpcManagerStatus {
    pub consecutive_errors: u32,
    pub error_threshold: u32,
    pub last_rebuild_time: Option<Instant>,
    pub time_since_last_rebuild: Option<Duration>,
}

/// RPC连接重建特质
pub trait RpcConnectionRebuilder {
    /// 重建RPC连接
    async fn rebuild_connection(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// 连接重建策略
#[derive(Debug, Clone)]
pub enum RebuildStrategy {
    /// 立即重建
    Immediate,
    /// 延迟重建
    Delayed(Duration),
    /// 指数退避重建
    ExponentialBackoff { initial_delay: Duration, max_delay: Duration },
}

impl Default for RebuildStrategy {
    fn default() -> Self {
        Self::Delayed(Duration::from_secs(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::{TransportError, TransportErrorKind};
    use std::fmt;

    struct MockFilterError;

    impl fmt::Debug for MockFilterError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("server returned an error response: error code -32602: filter not found")
        }
    }

    impl fmt::Display for MockFilterError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Filter Not Found Error")
        }
    }

    impl std::error::Error for MockFilterError {}

    #[tokio::test]
    async fn test_error_counting() {
        let config = RpcManagerConfig {
            error_threshold: 3,
            min_rebuild_interval: Duration::from_millis(100),
            monitoring_window: Duration::from_secs(10),
        };
        
        let manager = SmartRpcManager::new(config);
        let error = TransportError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));

        // 前两次错误不应该触发重建
        assert!(!manager.report_error(&error).await);
        assert_eq!(manager.current_error_count(), 1);
        
        assert!(!manager.report_error(&error).await);
        assert_eq!(manager.current_error_count(), 2);

        // 第三次错误应该触发重建
        assert!(manager.report_error(&error).await);
        assert_eq!(manager.current_error_count(), 3);
    }

    #[tokio::test]
    async fn test_success_resets_count() {
        let manager = SmartRpcManager::new_default();
        let error = TransportError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));

        // 累积错误
        assert!(!manager.report_error(&error).await);
        assert!(!manager.report_error(&error).await);
        assert_eq!(manager.current_error_count(), 2);

        // 成功操作应该重置计数
        manager.report_success();
        assert_eq!(manager.current_error_count(), 0);
    }

    #[tokio::test]
    async fn test_rebuild_rate_limiting() {
        let config = RpcManagerConfig {
            error_threshold: 2,
            min_rebuild_interval: Duration::from_millis(100),
            monitoring_window: Duration::from_secs(10),
        };
        
        let manager = SmartRpcManager::new(config);
        let error = TransportError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));

        // 第一次触发重建
        assert!(!manager.report_error(&error).await);
        assert!(manager.report_error(&error).await);
        manager.mark_connection_rebuilt().await;

        // 立即再次尝试应该被限制
        assert!(!manager.report_error(&error).await);
        assert!(!manager.report_error(&error).await); // 因为间隔不够，不应该触发

        // 等待间隔
        tokio::time::sleep(Duration::from_millis(150)).await;
        
        // 现在应该可以再次触发
        assert!(manager.report_error(&error).await);
    }
} 