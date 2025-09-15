#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::num::NonZeroU32;
use governor::{clock, RateLimiter};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use nonzero_ext::nonzero;
use crate::LimitRef;

#[async_trait::async_trait]
pub trait Datagram: Send + 'static {
    type Error;
    async fn send_to(&mut self, buf: &[u8]) -> Result<usize, Self::Error>;
    async fn recv_from(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

enum ReadState {
    Idle,
    Reading((usize, usize)),
}

enum WriteState {
    Idle,
    Writing((usize, usize)),
}

pub struct LimitDatagram<D: Datagram> {
    inner: D,
    limit: LimitRef,
    limit_quota: NonZeroU32,
    allow_burst: NonZeroU32,
    write_limiter: Option<RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware>>,
    read_limiter: Option<RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware>>,
    read_state: ReadState,
    write_state: WriteState,
}

impl<D: Datagram> LimitDatagram<D> {
    pub fn new(inner: D, limit: LimitRef) -> Self {
        Self { inner,
            limit,
            limit_quota: nonzero!(10u32),
            allow_burst: nonzero!(1u32),
            write_limiter: None,
            read_limiter: None,
            read_state: ReadState::Idle,
            write_state: WriteState::Idle,
        }
    }

    pub fn set_limit_quota(&mut self, piece_count: u32) {
        self.limit_quota = NonZeroU32::new(piece_count).unwrap_or(nonzero!(10u32));
    }

    pub fn set_allow_burst(&mut self, piece_count: u32) {
        self.allow_burst = NonZeroU32::new(piece_count).unwrap_or(nonzero!(1u32));
    }
}

#[async_trait::async_trait]
impl<D: Datagram> Datagram for LimitDatagram<D> {
    type Error = D::Error;

    async fn send_to(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match &mut self.write_state {
            WriteState::Idle => {
                if let Some(limit) = self.limit.write_limit() {
                    if self.write_limiter.is_none() {
                        self.write_limiter = Some(RateLimiter::direct(governor::Quota::per_second(self.limit_quota).allow_burst(self.allow_burst)));
                    }
                    let mut write_len = limit / self.limit_quota.get() as usize;
                    if write_len == 0 {
                        write_len = 1;
                    }
                    self.write_limiter.as_ref().unwrap().until_ready().await;
                    self.inner.send_to(buf).await?;
                    if buf.len() > write_len {
                        self.write_state = WriteState::Idle;
                    } else {
                        self.write_state = WriteState::Writing((write_len, buf.len()));
                    }
                    Ok(buf.len())
                } else {
                    self.inner.send_to(buf).await
                }
            }
            WriteState::Writing((write_len, written_len)) => {
                self.inner.send_to(buf).await?;
                if *written_len + buf.len() >= *write_len {
                    self.write_state = WriteState::Idle;
                    Ok(buf.len())
                } else {
                    self.write_state = WriteState::Writing((*write_len, *written_len + buf.len()));
                    Ok(buf.len())
                }
            },
        }
    }

    async fn recv_from(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match &mut self.read_state {
            ReadState::Idle => {
                if let Some(limit) = self.limit.read_limit() {
                    if self.read_limiter.is_none() {
                        self.read_limiter = Some(RateLimiter::direct(governor::Quota::per_second(self.limit_quota).allow_burst(self.allow_burst)));
                    }
                    let mut read_len = limit / self.limit_quota.get() as usize;
                    if read_len == 0 {
                        read_len = 1;
                    }
                    self.read_limiter.as_ref().unwrap().until_ready().await;
                    let len = self.inner.recv_from(buf).await?;
                    if len > read_len {
                        self.read_state = ReadState::Idle;
                        Ok(len)
                    } else {
                        self.read_state = ReadState::Reading((read_len, len));
                        Ok(len)
                    }

                } else {
                    self.inner.recv_from(buf).await
                }
            },
            ReadState::Reading((read_len, readded_len)) => {
                let len = self.inner.recv_from(buf).await?;
                if *readded_len + len >= *read_len {
                    self.read_state = ReadState::Idle;
                } else {
                    self.read_state = ReadState::Reading((*read_len, *readded_len + len));
                }
                Ok(len)
            },
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};
    use crate::Limit;

    // Mock Datagram 实现用于测试
    #[derive(Clone)]
    struct MockDatagram {
        // 记录调用历史
        call_history: Arc<Mutex<Vec<String>>>,
        // 预设的返回值队列
        send_returns: Arc<Mutex<VecDeque<Result<usize, &'static str>>>>,
        recv_returns: Arc<Mutex<VecDeque<Result<usize, &'static str>>>>,
    }

    impl MockDatagram {
        fn new() -> Self {
            Self {
                call_history: Arc::new(Mutex::new(Vec::new())),
                send_returns: Arc::new(Mutex::new(VecDeque::new())),
                recv_returns: Arc::new(Mutex::new(VecDeque::new())),
            }
        }

        // 设置send_to的返回值
        fn with_send_result(mut self, result: Result<usize, &'static str>) -> Self {
            self.send_returns.lock().unwrap().push_back(result);
            self
        }

        // 设置recv_from的返回值
        fn with_recv_result(mut self, result: Result<usize, &'static str>) -> Self {
            self.recv_returns.lock().unwrap().push_back(result);
            self
        }

        // 获取调用历史
        fn get_call_history(&self) -> Vec<String> {
            self.call_history.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Datagram for MockDatagram {
        type Error = &'static str;

        async fn send_to(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.call_history.lock().unwrap().push("send_to".to_string());
            match self.send_returns.lock().unwrap().pop_front() {
                Some(result) => Ok(buf.len()),
                None => Ok(buf.len()), // 默认返回缓冲区长度
            }
        }

        async fn recv_from(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            self.call_history.lock().unwrap().push("recv_from".to_string());
            match self.recv_returns.lock().unwrap().pop_front() {
                Some(_result) => Ok(buf.len()),
                None => Ok(buf.len()), // 默认返回0
            }
        }
    }

    // Mock LimitRef 实现
    struct MockLimitRef {
        read_limit: Option<usize>,
        write_limit: Option<usize>,
    }

    impl MockLimitRef {
        fn new(read_limit: Option<usize>, write_limit: Option<usize>) -> Arc<Self> {
            Arc::new(Self { read_limit, write_limit })
        }
    }

    impl Limit for MockLimitRef {
        fn read_limit(&self) -> Option<usize> {
            self.read_limit
        }

        fn write_limit(&self) -> Option<usize> {
            self.write_limit
        }
    }

    // 测试new方法的基本功能
    #[tokio::test]
    async fn test_limit_datagram_new() {
        let mock_datagram = MockDatagram::new();
        let limit_ref = MockLimitRef::new(Some(100), Some(100));
        let limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        // 验证初始状态
        match limit_datagram.read_state {
            ReadState::Idle => (),
            _ => panic!("Expected ReadState::Idle"),
        }
        match limit_datagram.write_state {
            WriteState::Idle => (),
            _ => panic!("Expected WriteState::Idle"),
        }
    }

    // 测试set_limit_quota方法
    #[test]
    fn test_set_limit_quota() {
        let mock_datagram = MockDatagram::new();
        let limit_ref = MockLimitRef::new(Some(100), Some(100));
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        // 设置正常值
        limit_datagram.set_limit_quota(50);
        // 注意：由于字段是私有的，我们无法直接验证值，但可以通过行为间接验证
        assert_eq!(limit_datagram.limit_quota.get(), 50);
    }

    // 测试set_allow_burst方法
    #[test]
    fn test_set_allow_burst() {
        let mock_datagram = MockDatagram::new();
        let limit_ref = MockLimitRef::new(Some(100), Some(100));
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        // 设置正常值
        limit_datagram.set_allow_burst(5);
        assert_eq!(limit_datagram.allow_burst.get(), 5);
    }

    // 测试在无写限制时send_to的行为
    #[tokio::test]
    async fn test_send_to_without_write_limit() {
        let mock_datagram = MockDatagram::new().with_send_result(Ok(10));
        let limit_ref = MockLimitRef::new(Some(100), None); // 无写限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        let buffer = [0u8; 10];
        let result = limit_datagram.send_to(&buffer).await;

        // 验证结果
        assert_eq!(result, Ok(10));
    }

    // 测试在无读限制时recv_from的行为
    #[tokio::test]
    async fn test_recv_from_without_read_limit() {
        let mock_datagram = MockDatagram::new().with_recv_result(Ok(10));
        let limit_ref = MockLimitRef::new(None, Some(100)); // 无读限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        let mut buffer = [0u8; 10];
        let result = limit_datagram.recv_from(&mut buffer).await;

        // 验证结果
        assert_eq!(result, Ok(10));
    }

    // 测试有写限制时首次send_to的行为
    #[tokio::test]
    async fn test_send_to_with_write_limit_initial() {
        let mock_datagram = MockDatagram::new().with_send_result(Ok(10));
        let limit_ref = MockLimitRef::new(None, Some(100)); // 有写限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        let buffer = [0u8; 10];
        let result = limit_datagram.send_to(&buffer).await;

        // 验证结果
        assert!(result.is_ok());
    }

    // 测试有读限制时首次recv_from的行为
    #[tokio::test]
    async fn test_recv_from_with_read_limit_initial() {
        let mock_datagram = MockDatagram::new().with_recv_result(Ok(10));
        let limit_ref = MockLimitRef::new(Some(100), None); // 有读限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        let mut buffer = [0u8; 10];
        let result = limit_datagram.recv_from(&mut buffer).await;

        // 验证结果
        assert!(result.is_ok());
    }

    // 测试Writing状态下的send_to行为
    #[tokio::test]
    async fn test_send_to_in_writing_state() {
        let mock_datagram = MockDatagram::new()
            .with_send_result(Ok(5))
            .with_send_result(Ok(5));
        let limit_ref = MockLimitRef::new(None, Some(10)); // 有写限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);
        limit_datagram.set_limit_quota(1);

        // 第一次调用进入Writing状态
        let buffer1 = [0u8; 5];
        let _ = limit_datagram.send_to(&buffer1).await;

        // 验证状态
        match limit_datagram.write_state {
            WriteState::Writing(_) => (),
            _ => panic!("Expected WriteState::Writing"),
        }

        let start = Instant::now();
        // 第二次调用保持在Writing状态
        let buffer2 = [0u8; 2];
        let result = limit_datagram.send_to(&buffer2).await;
        assert!(start.elapsed() <= Duration::from_millis(50));
        // 验证结果
        assert_eq!(result, Ok(2));

        // 第二次调用保持在Writing状态
        let buffer2 = [0u8; 5];
        let result = limit_datagram.send_to(&buffer2).await;

        // 验证结果
        assert_eq!(result, Ok(5));
        assert!(start.elapsed() <= Duration::from_millis(100));
        // 第二次调用保持在Writing状态
        let buffer2 = [0u8; 5];
        let result = limit_datagram.send_to(&buffer2).await;

        // 验证结果
        assert_eq!(result, Ok(5));
        assert!(start.elapsed() >= Duration::from_millis(900));
    }

    // 测试Reading状态下的recv_from行为
    #[tokio::test]
    async fn test_recv_from_in_reading_state() {
        let mock_datagram = MockDatagram::new()
            .with_recv_result(Ok(5))
            .with_recv_result(Ok(5));
        let limit_ref = MockLimitRef::new(Some(10), None); // 有读限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);
        limit_datagram.set_limit_quota(1);

        // 第一次调用进入Reading状态
        let mut buffer1 = [0u8; 5];
        let _ = limit_datagram.recv_from(&mut buffer1).await;

        // 验证状态
        match limit_datagram.read_state {
            ReadState::Reading(_) => (),
            _ => panic!("Expected ReadState::Reading"),
        }

        let start = Instant::now();
        // 第二次调用保持在Reading状态
        let mut buffer2 = [0u8; 2];
        let result = limit_datagram.recv_from(&mut buffer2).await;
        assert!(start.elapsed() <= Duration::from_millis(50));

        // 验证结果
        assert_eq!(result, Ok(2));

        let mut buffer2 = [0u8; 5];
        let result = limit_datagram.recv_from(&mut buffer2).await;
        assert!(start.elapsed() <= Duration::from_millis(100));

        // 验证结果
        assert_eq!(result, Ok(5));

        let mut buffer2 = [0u8; 5];

        let result = limit_datagram.recv_from(&mut buffer2).await;
        assert!(start.elapsed() > Duration::from_millis(900));

        // 验证结果
        assert_eq!(result, Ok(5));
    }

    // 测试Writing状态完成后回到Idle状态
    #[tokio::test]
    async fn test_send_to_complete_writing_state() {
        let mock_datagram = MockDatagram::new()
            .with_send_result(Ok(5))
            .with_send_result(Ok(5));
        let limit_ref = MockLimitRef::new(None, Some(10)); // 有写限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        // 第一次调用进入Writing状态
        let buffer1 = [0u8; 5];
        let _ = limit_datagram.send_to(&buffer1).await;

        // 第二次调用完成Writing状态，回到Idle
        let buffer2 = [0u8; 5];
        let _ = limit_datagram.send_to(&buffer2).await;

        // 验证状态回到Idle
        match limit_datagram.write_state {
            WriteState::Idle => (),
            _ => panic!("Expected WriteState::Idle"),
        }
    }

    // 测试Reading状态完成后回到Idle状态
    #[tokio::test]
    async fn test_recv_from_complete_reading_state() {
        let mock_datagram = MockDatagram::new()
            .with_recv_result(Ok(5))
            .with_recv_result(Ok(5));
        let limit_ref = MockLimitRef::new(Some(10), None); // 有读限制
        let mut limit_datagram = LimitDatagram::new(mock_datagram, limit_ref);

        // 第一次调用进入Reading状态
        let mut buffer1 = [0u8; 5];
        let _ = limit_datagram.recv_from(&mut buffer1).await;

        // 第二次调用完成Reading状态，回到Idle
        let mut buffer2 = [0u8; 5];
        let _ = limit_datagram.recv_from(&mut buffer2).await;

        // 验证状态回到Idle
        match limit_datagram.read_state {
            ReadState::Idle => (),
            _ => panic!("Expected ReadState::Idle"),
        }
    }
}
