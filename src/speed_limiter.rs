use std::num::{NonZeroU32};
use std::sync::{Arc, RwLock};
use governor::clock;
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};

type GovRateLimiter = governor::RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware>;

pub struct SpeedLimiter {
    upper: Option<SpeedLimiterRef>,
    // Weight, governor instance, and the configured rate (including the unlimited sentinel).
    limiter: RwLock<(NonZeroU32, Arc<Option<GovRateLimiter>>, NonZeroU32)>,
}

pub type SpeedLimiterRef = Arc<SpeedLimiter>;

impl SpeedLimiter {
    pub fn new(upper: Option<SpeedLimiterRef>, rate: Option<NonZeroU32>, weight: Option<NonZeroU32>) -> SpeedLimiterRef {
        let rate = rate.unwrap_or(NonZeroU32::new(u32::MAX).unwrap());
        let weight = weight.unwrap_or(NonZeroU32::new(64 * 1024).unwrap());
        if rate.get() == u32::MAX {
            Arc::new(SpeedLimiter {
                upper,
                limiter: RwLock::new((weight, Arc::new(None), rate)),
            })
        } else {
            Arc::new(SpeedLimiter {
                upper,
                limiter: RwLock::new((weight, Arc::new(
                    Some(GovRateLimiter::direct(
                        governor::Quota::per_second(rate)
                            .allow_burst(rate)))), rate)),
            })
        }
    }

    pub fn new_with_burst(upper: Option<SpeedLimiterRef>, rate: Option<NonZeroU32>, weight: Option<NonZeroU32>, burst: Option<NonZeroU32>) -> SpeedLimiterRef {
        let rate = rate.unwrap_or(NonZeroU32::new(u32::MAX).unwrap());
        let weight = weight.unwrap_or(NonZeroU32::new(64 * 1024).unwrap());
        let burst = burst.unwrap_or(rate);
        if rate.get() == u32::MAX {
            Arc::new(SpeedLimiter {
                upper,
                limiter: RwLock::new((weight, Arc::new(None), rate)),
            })
        } else {
            Arc::new(SpeedLimiter {
                upper,
                limiter: RwLock::new((weight, Arc::new(
                    Some(GovRateLimiter::direct(
                        governor::Quota::per_second(rate)
                            .allow_burst(burst)))), rate)),
            })
        }
    }

    pub fn set_limit(&self, rate: Option<NonZeroU32>, weight: Option<NonZeroU32>) {
        let rate = rate.unwrap_or(NonZeroU32::new(u32::MAX).unwrap());
        let weight = weight.unwrap_or(NonZeroU32::new(64 * 1024).unwrap());
        let mut limiter = self.limiter.write().unwrap();
        if limiter.2 == rate && limiter.0 == weight {
            return;
        }
        if rate.get() == u32::MAX {
            *limiter = (weight, Arc::new(None), rate);
        } else {
            *limiter = (weight, Arc::new(Some(
                GovRateLimiter::direct(
                    governor::Quota::per_second(rate)
                        .allow_burst(rate)))), rate);
        }
    }

    pub fn new_limit_session(self: &SpeedLimiterRef) -> SpeedLimitSession {
        let upper_session = if self.upper.is_some() {
            Some(Box::new(self.upper.as_ref().unwrap().new_limit_session()))
        } else {
            None
        };

        SpeedLimitSession::new(upper_session, self.clone())
    }

    async fn until_ready(&self) -> usize {
        let (piece_size, limiter) = {
            let limiter = self.limiter.read().unwrap();
            (limiter.0, limiter.1.clone())
        };
        if limiter.is_none() {
            piece_size.get() as usize
        } else {
            let limiter = limiter.as_ref().as_ref().unwrap();
            limiter.until_ready().await;
            piece_size.get() as usize
        }
    }
}

pub struct SpeedLimitSession {
    upper_session: Option<Box<SpeedLimitSession>>,
    limiter: SpeedLimiterRef,
    upper_remainder: usize,
    local_remainder: usize,
}

impl SpeedLimitSession {
    fn new(upper_session: Option<Box<SpeedLimitSession>>, limiter: SpeedLimiterRef) -> SpeedLimitSession {
        SpeedLimitSession {
            upper_session,
            limiter,
            upper_remainder: 0,
            local_remainder: 0,
        }
    }

    #[async_recursion::async_recursion(Sync)]
    async fn until_ready_inner(&mut self) -> usize {
        if self.upper_session.is_some() {
            if self.upper_remainder == 0 {
                self.upper_remainder = self.upper_session.as_mut().unwrap().until_ready().await;
            }

            if self.local_remainder == 0 {
                self.local_remainder = self.limiter.until_ready().await;
            }

            if self.upper_remainder > self.local_remainder {
                self.upper_remainder -= self.local_remainder;
                let ret = self.local_remainder;
                self.local_remainder = 0;
                ret
            } else {
                self.local_remainder -= self.upper_remainder;
                let ret = self.upper_remainder;
                self.upper_remainder = 0;
                ret
            }
        } else {
            self.limiter.until_ready().await
        }
    }

    pub async fn until_ready(&mut self) -> usize {
        self.until_ready_inner().await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use std::time::Instant;
    use tokio;

    // 辅助函数：创建 NonZeroU32
    fn nz_u32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("Value must be non-zero")
    }

    /// 测试 RateLimiter::new 创建无限制实例
    #[test]
    fn test_rate_limiter_new_unlimited() {
        let rate_limiter = SpeedLimiter::new(None, None, Some(nz_u32(1)));

        // 检查是否正确设置了权重
        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 1);
        assert!(limiter_data.1.is_none());
    }

    /// 测试 RateLimiter::new 创建有限制实例
    #[test]
    fn test_rate_limiter_new_limited() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(100)), Some(nz_u32(1)));

        // 检查是否正确设置了权重和限制器
        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 1);
        assert!(limiter_data.1.is_some());
    }

    /// 测试 RateLimiter::set_limit 从有限制变为无限制
    #[test]
    fn test_set_limit_to_unlimited() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(100)), Some(nz_u32(1)));
        rate_limiter.set_limit(Some(nz_u32(u32::MAX)), Some(nz_u32(5)));

        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 5);
        assert!(limiter_data.1.is_none());
    }

    /// 测试 RateLimiter::set_limit 从无限制变为有限制
    #[test]
    fn test_set_limit_to_limited() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(u32::MAX)), Some(nz_u32(1)));
        rate_limiter.set_limit(Some(nz_u32(50)), Some(nz_u32(3)));

        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 3);
        assert!(limiter_data.1.is_some());
    }

    #[test]
    fn test_set_limit_same_rate_and_weight_preserves_bucket() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(1)), Some(nz_u32(1)));
        let original = rate_limiter.limiter.read().unwrap().1.clone();
        assert!(original.as_ref().as_ref().unwrap().check().is_ok());

        rate_limiter.set_limit(Some(nz_u32(1)), Some(nz_u32(1)));

        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 1);
        assert!(Arc::ptr_eq(&original, &limiter_data.1));
        assert!(limiter_data.1.as_ref().as_ref().unwrap().check().is_err());
    }

    #[test]
    fn test_set_limit_same_rate_and_weight_preserves_custom_burst() {
        let rate_limiter = SpeedLimiter::new_with_burst(
            None, Some(nz_u32(1)), None, Some(nz_u32(2)),
        );
        let original = rate_limiter.limiter.read().unwrap().1.clone();
        assert!(matches!(original.as_ref().as_ref().unwrap().check_n(nz_u32(2)), Ok(Ok(()))));

        rate_limiter.set_limit(Some(nz_u32(1)), None);

        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 64 * 1024);
        assert!(Arc::ptr_eq(&original, &limiter_data.1));
        assert!(limiter_data.1.as_ref().as_ref().unwrap().check().is_err());
    }

    #[test]
    fn test_set_limit_same_unlimited_rate_and_weight_reuses_instance() {
        let rate_limiter = SpeedLimiter::new(None, None, Some(nz_u32(5)));
        let original = rate_limiter.limiter.read().unwrap().1.clone();

        for rate in [Some(nz_u32(u32::MAX)), None] {
            rate_limiter.set_limit(rate, Some(nz_u32(5)));
            let limiter_data = rate_limiter.limiter.read().unwrap();
            assert_eq!(limiter_data.0.get(), 5);
            assert!(limiter_data.1.is_none());
            assert!(Arc::ptr_eq(&original, &limiter_data.1));
        }
    }

    #[test]
    fn test_set_limit_changed_weight_replaces_bucket() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(1)), Some(nz_u32(1)));
        let mut previous = rate_limiter.limiter.read().unwrap().1.clone();
        assert!(previous.as_ref().as_ref().unwrap().check().is_ok());

        for weight in [Some(nz_u32(5)), None] {
            rate_limiter.set_limit(Some(nz_u32(1)), weight);
            let updated = rate_limiter.limiter.read().unwrap().1.clone();
            assert!(!Arc::ptr_eq(&previous, &updated));
            assert!(updated.as_ref().as_ref().unwrap().check().is_ok());

            rate_limiter.set_limit(Some(nz_u32(1)), weight);
            let limiter_data = rate_limiter.limiter.read().unwrap();
            assert_eq!(limiter_data.0, weight.unwrap_or(nz_u32(64 * 1024)));
            assert!(Arc::ptr_eq(&updated, &limiter_data.1));
            assert!(limiter_data.1.as_ref().as_ref().unwrap().check().is_err());
            previous = updated;
        }
    }

    #[test]
    fn test_set_limit_changed_rate_replaces_bucket() {
        // These rates have the same nanosecond refill interval but are distinct settings.
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(1_000_000_000)), Some(nz_u32(3)));
        let original = rate_limiter.limiter.read().unwrap().1.clone();

        rate_limiter.set_limit(Some(nz_u32(1_000_000_001)), Some(nz_u32(3)));

        let updated = rate_limiter.limiter.read().unwrap().1.clone();
        assert!(!Arc::ptr_eq(&original, &updated));

        rate_limiter.set_limit(Some(nz_u32(1_000_000_001)), Some(nz_u32(3)));
        let limiter_data = rate_limiter.limiter.read().unwrap();
        assert_eq!(limiter_data.0.get(), 3);
        assert!(Arc::ptr_eq(&updated, &limiter_data.1));
    }

    /// 测试 new_limit_session 不带上级限制器
    #[test]
    fn test_new_limit_session_without_upper() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(100)), Some(nz_u32(1)));
        let session = rate_limiter.new_limit_session();

        assert!(session.upper_session.is_none());
        // Note: We can't directly compare Arc pointers, but we can check that it's the same object
        assert!(Arc::ptr_eq(&session.limiter, &rate_limiter));
    }

    /// 测试 new_limit_session 带上级限制器
    #[test]
    fn test_new_limit_session_with_upper() {
        let upper_limiter = SpeedLimiter::new(None, Some(nz_u32(200)), Some(nz_u32(2)));
        let rate_limiter = SpeedLimiter::new(Some(upper_limiter.clone()), Some(nz_u32(100)), Some(nz_u32(1)));
        let session = rate_limiter.new_limit_session();

        assert!(session.upper_session.is_some());
        assert!(Arc::ptr_eq(&session.limiter, &rate_limiter));
    }

    /// 测试 until_ready 在无限制情况下的行为（同步部分）
    #[tokio::test]
    async fn test_until_ready_unlimited() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(u32::MAX)), Some(nz_u32(7)));
        let result = rate_limiter.until_ready().await;
        assert_eq!(result, 7);
    }

    /// 测试 until_ready 在有限制情况下的行为（检查能否正常调用）
    #[tokio::test]
    async fn test_until_ready_limited() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(1000)), Some(nz_u32(4))); // High rate to avoid waiting
        let result = rate_limiter.until_ready().await;
        assert_eq!(result, 4);
    }

    /// 测试 RateLimitSession::new
    #[test]
    fn test_rate_limit_session_new() {
        let rate_limiter = SpeedLimiter::new(None, Some(nz_u32(100)), Some(nz_u32(1)));
        let session = SpeedLimitSession::new(None, rate_limiter.clone());

        assert!(session.upper_session.is_none());
        assert!(Arc::ptr_eq(&session.limiter, &rate_limiter));
        assert_eq!(session.upper_remainder, 0);
        assert_eq!(session.local_remainder, 0);
    }

    /// 测试嵌套速率限制器的基本结构
    #[tokio::test]
    async fn test_nested_rate_limiters() {
        let top_limiter = SpeedLimiter::new(None, Some(nz_u32(1000)), Some(nz_u32(1)));
        let middle_limiter = SpeedLimiter::new(Some(top_limiter.clone()), Some(nz_u32(500)), Some(nz_u32(1)));
        let bottom_limiter = SpeedLimiter::new(Some(middle_limiter.clone()), Some(nz_u32(250)), Some(nz_u32(1)));

        // Check the chain structure
        assert!(bottom_limiter.upper.is_some());
        assert!(middle_limiter.upper.is_some());
        assert!(top_limiter.upper.is_none());

        let start = Instant::now();
        let mut speed = 0;
        let mut session = bottom_limiter.new_limit_session();
        while start.elapsed().as_millis() < 1000 {
            speed += session.until_ready().await;
        }
        assert!(speed > 480);
        assert!(speed < 520);
    }

    #[tokio::test]
    async fn test_nested_rate_limiters2() {
        let top_limiter = SpeedLimiter::new(None, Some(nz_u32(1000)), Some(nz_u32(4)));
        let middle_limiter = SpeedLimiter::new(Some(top_limiter.clone()), Some(nz_u32(500)), Some(nz_u32(2)));
        let bottom_limiter = SpeedLimiter::new(Some(middle_limiter.clone()), Some(nz_u32(250)), Some(nz_u32(1)));

        // Check the chain structure
        assert!(bottom_limiter.upper.is_some());
        assert!(middle_limiter.upper.is_some());
        assert!(top_limiter.upper.is_none());

        let start = Instant::now();
        let mut speed = 0;
        let mut session = bottom_limiter.new_limit_session();
        while start.elapsed().as_millis() < 1000 {
            speed += session.until_ready().await;
        }
        assert!(speed > 480);
        assert!(speed < 520);
    }

    #[tokio::test]
    async fn test_nested_rate_limiters3() {
        let top_limiter = SpeedLimiter::new(None, Some(nz_u32(10)), Some(nz_u32(4)));
        let middle_limiter = SpeedLimiter::new(Some(top_limiter.clone()), Some(nz_u32(500)), Some(nz_u32(2)));
        let bottom_limiter = SpeedLimiter::new(Some(middle_limiter.clone()), Some(nz_u32(250)), Some(nz_u32(1)));

        // Check the chain structure
        assert!(bottom_limiter.upper.is_some());
        assert!(middle_limiter.upper.is_some());
        assert!(top_limiter.upper.is_none());

        let start = Instant::now();
        let mut speed = 0;
        let mut session = bottom_limiter.new_limit_session();
        while start.elapsed().as_millis() < 1000 {
            speed += session.until_ready().await;
        }
        assert!(speed > 75);
        assert!(speed < 85);
    }
}
