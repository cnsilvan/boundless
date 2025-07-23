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

use alloy::transports::{
    layers::{RateLimitRetryPolicy, RetryPolicy},
    TransportError, TransportErrorKind,
};
use std::time::Duration;

#[derive(Debug, Copy, Clone, Default)]
pub struct CustomRetryPolicy;

/// The retry policy for the RPC provider used throughout
///
/// This 'extends' the default retry policy to include a retry for
/// OS error 104 which is believed to be behind a number of issues
/// https://github.com/boundless-xyz/boundless/issues/240
/// 
/// Also handles filter errors (-32602: filter not found) which occur
/// when RPC providers clean up expired event filters
impl RetryPolicy for CustomRetryPolicy {
    fn should_retry(&self, error: &TransportError) -> bool {
        let should_retry = match error {
            TransportError::Transport(TransportErrorKind::Custom(err)) => {
                // easier to match against the debug format string because this is what we see in the logs
                let err_debug_str = format!("{err:?}");
                err_debug_str.contains("os error 104") 
                    || err_debug_str.contains("reset by peer")
                    || err_debug_str.contains("filter not found")  // 新增：处理过滤器错误
                    || err_debug_str.contains("error code -32602")  // 新增：处理RPC错误码
                    || err_debug_str.contains("error code -32000")  // 新增：处理另一种过滤器错误码
            }
            _ => false,
        };
        should_retry || RateLimitRetryPolicy::default().should_retry(error)
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<Duration> {
        // 对于过滤器错误，使用较短的退避时间，因为需要快速重建连接
        match error {
            TransportError::Transport(TransportErrorKind::Custom(err)) => {
                let err_debug_str = format!("{err:?}");
                if err_debug_str.contains("filter not found") 
                    || err_debug_str.contains("error code -32602")
                    || err_debug_str.contains("error code -32000") {
                    Some(Duration::from_millis(500)) // 过滤器错误：500ms退避
                } else {
                    RateLimitRetryPolicy::default().backoff_hint(error)
                }
            }
            _ => RateLimitRetryPolicy::default().backoff_hint(error)
        }
    }
}

/// 检查是否为过滤器相关错误
pub fn is_filter_error(error: &TransportError) -> bool {
    match error {
        TransportError::Transport(TransportErrorKind::Custom(err)) => {
            let err_debug_str = format!("{err:?}");
            err_debug_str.contains("filter not found") 
                || err_debug_str.contains("error code -32602")
                || err_debug_str.contains("error code -32000")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::transports::{layers::RetryPolicy, RpcError, TransportErrorKind};
    use std::fmt;

    struct MockError;

    impl fmt::Debug for MockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("reqwest::Error { kind: Request, source: hyper_util::client::legacy::Error(SendRequest, hyper::Error(Io, Os { code: 104, kind: ConnectionReset, message: \"Connection reset by peer\" })) }")
        }
    }

    impl fmt::Display for MockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Mock Error")
        }
    }

    impl std::error::Error for MockError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            None
        }
    }

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

    impl std::error::Error for MockFilterError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            None
        }
    }

    #[test]
    fn retries_on_os_error_104() {
        let policy = CustomRetryPolicy;
        let error = RpcError::Transport(TransportErrorKind::Custom(Box::new(MockError)));
        assert!(policy.should_retry(&error));
    }

    #[test] 
    fn retries_on_filter_not_found() {
        let policy = CustomRetryPolicy;
        let error = RpcError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));
        assert!(policy.should_retry(&error));
    }

    #[test]
    fn provides_short_backoff_for_filter_errors() {
        let policy = CustomRetryPolicy;
        let error = RpcError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));
        let backoff = policy.backoff_hint(&error);
        assert_eq!(backoff, Some(Duration::from_millis(500)));
    }

    #[test]
    fn detects_filter_errors() {
        let error = RpcError::Transport(TransportErrorKind::Custom(Box::new(MockFilterError)));
        assert!(is_filter_error(&error));
    }
}
