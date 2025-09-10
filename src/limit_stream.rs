#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::io::Error;
use std::num::NonZeroU32;
use std::pin::{Pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use governor::{clock, RateLimiter};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use nonzero_ext::nonzero;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub trait Limit: 'static + Sync + Send {
    fn read_limit(&self) -> Option<usize>;
    fn write_limit(&self) -> Option<usize>;
}
pub type LimitRef = Arc<dyn Limit>;

enum ReadState {
    Idle,
    Waiting(Option<(Pin<Box<dyn Future<Output=()> + Send + 'static>>, usize, usize)>),
    Reading(Option<(usize, usize)>),
}

enum WriteState {
    Idle,
    Waiting(Option<(Pin<Box<dyn Future<Output=()> + Send + 'static>>, usize, usize)>),
    Writing(Option<(usize, usize)>),
}

pub struct LimitStream<S: AsyncRead + AsyncWrite + Unpin + Send> {
    stream: S,
    limit: LimitRef,
    read_limiter: Option<RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware>>,
    write_limiter: Option<RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware>>,
    read_state: ReadState,
    write_state: WriteState,
    limit_quota: NonZeroU32,
    allow_burst: NonZeroU32,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> LimitStream<S> {
    pub fn new(stream: S, limit: LimitRef) -> Self {
        LimitStream {
            stream,
            limit,
            read_limiter: None,
            write_limiter: None,
            read_state: ReadState::Idle,
            write_state: WriteState::Idle,
            limit_quota: nonzero!(10u32),
            allow_burst: nonzero!(1u32),
        }
    }

    pub fn set_limit_quota(&mut self, piece_count: u32) {
        self.limit_quota = NonZeroU32::new(piece_count).unwrap_or(nonzero!(10u32));
    }

    pub fn set_allow_burst(&mut self, piece_count: u32) {
        self.allow_burst = NonZeroU32::new(piece_count).unwrap_or(nonzero!(1u32));
    }

    pub fn raw_stream(&mut self) -> &mut S {
        &mut self.stream
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncRead for LimitStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.read_state {
            ReadState::Idle => {
                if let Some(read_limit) = self.limit.read_limit() {
                    if self.read_limiter.is_none() {
                        self.read_limiter = Some(RateLimiter::direct(governor::Quota::per_second(self.limit_quota).allow_burst(self.allow_burst)));
                    }
                    let mut read_len = read_limit / self.limit_quota.get() as usize;
                    if read_len == 0 {
                        read_len = 1;
                    }
                    let mut readded_len = 0;

                    let read_limiter: &'static RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware> = unsafe {
                        std::mem::transmute(self.read_limiter.as_ref().unwrap())
                    };
                    let mut waiting_future = Box::pin(read_limiter.until_ready());
                    match Pin::new(&mut waiting_future).poll(cx) {
                        Poll::Ready(_) => {
                            let mut read_buf = if read_len <= buf.remaining() {
                                buf.take(read_len)
                            } else {
                                buf.take(buf.remaining())
                            };
                            match Pin::new(&mut self.stream).poll_read(cx, &mut read_buf) {
                                Poll::Ready(Ok(())) => {
                                    let len = read_buf.filled().len();
                                    readded_len += len;
                                    buf.advance(len);
                                    if readded_len >= read_len {
                                        self.read_state = ReadState::Idle;
                                    } else {
                                        self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                    }
                                    Poll::Ready(Ok(()))
                                },
                                Poll::Ready(Err(e)) => {
                                    self.read_state = ReadState::Idle;
                                    Poll::Ready(Err(e))
                                },
                                Poll::Pending => {
                                    self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                    Poll::Pending
                                }
                            }
                        }
                        Poll::Pending => {
                            self.read_state = ReadState::Waiting(Some((waiting_future, read_len, readded_len)));
                            Poll::Pending
                        }
                    }
                } else {
                    match Pin::new(&mut self.stream).poll_read(cx, buf) {
                        Poll::Ready(Ok(())) => {
                            self.read_state = ReadState::Idle;
                            Poll::Ready(Ok(()))
                        },
                        Poll::Ready(Err(e)) => {
                            self.read_state = ReadState::Idle;
                            Poll::Ready(Err(e))
                        },
                        Poll::Pending => {
                            self.read_state = ReadState::Reading(None);
                            Poll::Pending
                        }
                    }
                }
            },
            ReadState::Waiting(ref mut state) => {
                let (mut rx, read_len, mut readded_len) = state.take().unwrap();
                match Pin::new(&mut rx).poll(cx) {
                    Poll::Ready(_) => {
                        let mut read_buf = if (read_len - readded_len) <= buf.remaining() {
                            buf.take(read_len - readded_len)
                        } else {
                            buf.take(buf.remaining())
                        };
                        match Pin::new(&mut self.stream).poll_read(cx, &mut read_buf) {
                            Poll::Ready(Ok(())) => {
                                let len = read_buf.filled().len();
                                readded_len += len;
                                buf.advance(len);
                                if readded_len >= read_len {
                                    self.read_state = ReadState::Idle;
                                } else {
                                    self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                }
                                Poll::Ready(Ok(()))
                            }
                            Poll::Ready(Err(e)) => {
                                self.read_state = ReadState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                Poll::Pending
                            }
                        }
                    }
                    Poll::Pending => {
                        self.read_state = ReadState::Waiting(Some((rx, read_len, readded_len)));
                        Poll::Pending
                    }
                }
            },
            ReadState::Reading(ref mut state) => {
                match state.take() {
                    Some((read_len, mut readded_len)) => {
                        let mut read_buf = if (read_len - readded_len) <= buf.remaining() {
                            buf.take(read_len - readded_len)
                        } else {
                            buf.take(buf.remaining())
                        };
                        match Pin::new(&mut self.stream).poll_read(cx, &mut read_buf) {
                            Poll::Ready(Ok(())) => {
                                let len = read_buf.filled().len();
                                readded_len += len;
                                buf.advance(len);
                                if readded_len >= read_len {
                                    self.read_state = ReadState::Idle;
                                } else {
                                    self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                }
                                Poll::Ready(Ok(()))
                            }
                            Poll::Ready(Err(e)) => {
                                self.read_state = ReadState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.read_state = ReadState::Reading(Some((read_len, readded_len)));
                                Poll::Pending
                            }
                        }
                    },
                    None => {
                        match Pin::new(&mut self.stream).poll_read(cx, buf) {
                            Poll::Ready(Ok(())) => {
                                self.read_state = ReadState::Idle;
                                Poll::Ready(Ok(()))
                            },
                            Poll::Ready(Err(e)) => {
                                self.read_state = ReadState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.read_state = ReadState::Reading(None);
                                Poll::Pending
                            }
                        }
                    }
                }

            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncWrite for LimitStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        match self.write_state {
            WriteState::Idle => {
                if let Some(limit) = self.limit.write_limit() {
                    if self.write_limiter.is_none() {
                        self.write_limiter = Some(RateLimiter::direct(governor::Quota::per_second(self.limit_quota).allow_burst(self.allow_burst)));
                    }
                    let mut write_len = limit / self.limit_quota.get() as usize;
                    if write_len == 0 {
                        write_len = 1;
                    }
                    let mut written_len = 0;
                    let write_limiter: &'static RateLimiter<NotKeyed, InMemoryState, clock::DefaultClock, NoOpMiddleware> = unsafe {
                        std::mem::transmute(self.write_limiter.as_ref().unwrap())
                    };
                    let mut waiting_future = Box::pin(write_limiter.until_ready());
                    match Pin::new(&mut waiting_future).poll(cx) {
                        Poll::Ready(_) => {
                            let write_buf = if write_len <= buf.len() {
                                &buf[..write_len]
                            } else {
                                buf
                            };
                            match Pin::new(&mut self.stream).poll_write(cx, write_buf) {
                                Poll::Ready(Ok(len)) => {
                                    written_len += len;
                                    if written_len >= write_len {
                                        self.write_state = WriteState::Idle;
                                    } else {
                                        self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                    }
                                    Poll::Ready(Ok(written_len))
                                },
                                Poll::Ready(Err(e)) => {
                                    self.read_state = ReadState::Idle;
                                    Poll::Ready(Err(e))
                                },
                                Poll::Pending => {
                                    self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                    Poll::Pending
                                }
                            }
                        },
                        Poll::Pending => {
                            self.write_state = WriteState::Waiting(Some((waiting_future, write_len, written_len)));
                            Poll::Pending
                        }
                    }

                } else {
                    match Pin::new(&mut self.stream).poll_write(cx, buf) {
                        Poll::Ready(Ok(len)) => {
                            self.write_state = WriteState::Idle;
                            Poll::Ready(Ok(len))
                        },
                        Poll::Ready(Err(e)) => {
                            self.write_state = WriteState::Idle;
                            Poll::Ready(Err(e))
                        },
                        Poll::Pending => {
                            self.write_state = WriteState::Writing(None);
                            Poll::Pending
                        }
                    }
                }
            }
            WriteState::Waiting(ref mut state) => {
                let (mut waiting_future, write_len, mut written_len) = state.take().unwrap();
                match Pin::new(&mut waiting_future).poll(cx) {
                    Poll::Ready(_) => {
                        let write_buf = if write_len - written_len <= buf.len() {
                            &buf[..(write_len - written_len)]
                        } else {
                            buf
                        };
                        match Pin::new(&mut self.stream).poll_write(cx, write_buf) {
                            Poll::Ready(Ok(len)) => {
                                written_len += len;
                                if written_len >= write_len {
                                    self.write_state = WriteState::Idle;
                                } else {
                                    self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                }
                                Poll::Ready(Ok(len))
                            },
                            Poll::Ready(Err(e)) => {
                                self.write_state = WriteState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                Poll::Pending
                            }
                        }
                    }
                    Poll::Pending => {
                        self.write_state = WriteState::Waiting(Some((waiting_future, write_len, written_len)));
                        Poll::Pending
                    }
                }
            }
            WriteState::Writing(ref mut state) => {
                match state.take() {
                    Some((write_len, mut written_len)) => {
                        let write_buf = if write_len - written_len <= buf.len() {
                            &buf[..(write_len - written_len)]
                        } else {
                            buf
                        };
                        match Pin::new(&mut self.stream).poll_write(cx, write_buf) {
                            Poll::Ready(Ok(len)) => {
                                written_len += len;
                                if written_len >= write_len {
                                    self.write_state = WriteState::Idle;
                                } else {
                                    self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                }
                                Poll::Ready(Ok(len))
                            },
                            Poll::Ready(Err(e)) => {
                                self.write_state = WriteState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.write_state = WriteState::Writing(Some((write_len, written_len)));
                                Poll::Pending
                            }
                        }
                    },
                    None => {
                        match Pin::new(&mut self.stream).poll_write(cx, buf) {
                            Poll::Ready(Ok(len)) => {
                                self.write_state = WriteState::Idle;
                                Poll::Ready(Ok(len))
                            },
                            Poll::Ready(Err(e)) => {
                                self.write_state = WriteState::Idle;
                                Poll::Ready(Err(e))
                            },
                            Poll::Pending => {
                                self.write_state = WriteState::Writing(None);
                                Poll::Pending
                            }
                        }
                    }
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::future::poll_fn;
    use super::*;
    use std::io::{Error, ErrorKind};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use futures::task::noop_waker;

    // Mock stream implementation for testing
    struct MockStream {
        read_data: Vec<u8>,
        read_pos: usize,
        read_should_pending: bool,
        read_error: Option<Error>,
        write_should_pending: bool,
        write_error: Option<Error>,
        written_data: Vec<u8>,
    }

    impl MockStream {
        fn new(read_data: Vec<u8>) -> Self {
            Self {
                read_data,
                read_pos: 0,
                read_should_pending: false,
                read_error: None,
                write_should_pending: false,
                write_error: None,
                written_data: Vec::new(),
            }
        }

        fn with_read_pending(mut self) -> Self {
            self.read_should_pending = true;
            self
        }

        fn with_read_error(mut self, error: Error) -> Self {
            self.read_error = Some(error);
            self
        }

        fn with_write_pending(mut self) -> Self {
            self.write_should_pending = true;
            self
        }

        fn with_write_error(mut self, error: Error) -> Self {
            self.write_error = Some(error);
            self
        }

        fn written_data(&self) -> &[u8] {
            &self.written_data
        }
    }

    impl AsyncRead for MockStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>
        ) -> Poll<std::io::Result<()>> {
            if let Some(error) = self.read_error.take() {
                return Poll::Ready(Err(error));
            }

            if self.read_should_pending {
                return Poll::Pending;
            }

            let remaining = self.read_data.len() - self.read_pos;
            if remaining == 0 {
                return Poll::Ready(Ok(()));
            }

            let to_copy = std::cmp::min(remaining, buf.remaining());
            buf.put_slice(&self.read_data[self.read_pos..self.read_pos + to_copy]);
            self.read_pos += to_copy;

            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for MockStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8]
        ) -> Poll<Result<usize, Error>> {
            if let Some(error) = self.write_error.take() {
                return Poll::Ready(Err(error));
            }

            if self.write_should_pending {
                return Poll::Pending;
            }

            self.written_data.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Poll::Ready(Ok(()))
        }
    }

    // Mock limit implementation
    struct MockLimit {
        read_limit: Option<usize>,
        write_limit: Option<usize>,
    }

    impl MockLimit {
        fn new(read_limit: Option<usize>, write_limit: Option<usize>) -> Self {
            Self { read_limit, write_limit }
        }
    }

    impl Limit for MockLimit {
        fn read_limit(&self) -> Option<usize> {
            self.read_limit
        }

        fn write_limit(&self) -> Option<usize> {
            self.write_limit
        }
    }

    #[test]
    fn test_new_limit_stream() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(Some(100), Some(100)));
        let mut limit_stream = LimitStream::new(mock_stream, limit.clone());
        limit_stream.set_allow_burst(2);

        assert_eq!(limit_stream.limit.read_limit(), Some(100));
        assert_eq!(limit_stream.limit.write_limit(), Some(100));
        assert!(limit_stream.read_limiter.is_none());
        assert!(limit_stream.write_limiter.is_none());
        assert_eq!(limit_stream.allow_burst.get(), 2u32);
    }

    #[tokio::test]
    async fn test_read_without_limit() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制读取
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn test_read_without_limit1() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]).with_read_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 3];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制读取
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        limit_stream.raw_stream().read_should_pending = false;
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1, 2, 3]);
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[4, 5]);
    }

    #[tokio::test]
    async fn test_read_without_limit2() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]).with_read_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 3];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制读取
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        limit_stream.raw_stream().read_should_pending = false;
        let error = Error::new(ErrorKind::Other, "read error");
        limit_stream.raw_stream().read_error = Some(error);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());

        if let Poll::Ready(ret) = result {
            assert!(ret.is_err());
        }
    }

    #[tokio::test]
    async fn test_read_without_limit_err() {
        let error = Error::new(ErrorKind::Other, "read error");
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]).with_read_error(error);
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制读取
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        match result {
            Poll::Ready(ret) => assert!(ret.is_err()),
            Poll::Pending => panic!("Expected ready"),
        }
    }

    #[tokio::test]
    async fn test_read_with_limit() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(1), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次读取应该等待令牌
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1]);

        let start = Instant::now();
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[2]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(1900));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[3]);
    }

    #[tokio::test]
    async fn test_read_with_limit2() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(2), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次读取应该等待令牌
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1, 2]);

        let start = Instant::now();
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[3, 4]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(1900));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[5]);
    }

    #[tokio::test]
    async fn test_read_with_limit3() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(2), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let mut buffer = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[2]);

        let start = Instant::now();
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[3]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_read(cx, &mut read_buf)
        }).await;
        assert!(start.elapsed() < Duration::from_millis(1100));
        assert!(result.is_ok());
        assert_eq!(read_buf.filled(), &[4]);
    }

    #[tokio::test]
    async fn test_read_with_limit4() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(1), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(2);

        let mut buffer = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        tokio::time::sleep(Duration::from_millis(600)).await;
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[2]);

    }

    #[tokio::test]
    async fn test_read_with_limit5() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(2), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let mut buffer = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1]);
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[2]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        limit_stream.raw_stream().read_should_pending = true;
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        limit_stream.raw_stream().read_should_pending = false;
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[3]);
        let error = Error::new(ErrorKind::Other, "read error");
        limit_stream.raw_stream().read_error = Some(error);
        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());

        if let Poll::Ready(Err(e)) = result {
            assert_eq!(e.kind(), ErrorKind::Other);
        }
    }

    #[tokio::test]
    async fn test_read_with_limit6() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(1), None)); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(2);

        let mut buffer = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        assert_eq!(read_buf.filled(), &[1]);

        let mut read_buf = ReadBuf::new(&mut buffer);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        tokio::time::sleep(Duration::from_millis(600)).await;
        let error = Error::new(ErrorKind::Other, "read error");
        limit_stream.raw_stream().read_error = Some(error);
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());

        if let Poll::Ready(Err(e)) = result {
            assert_eq!(e.kind(), ErrorKind::Other);
        }
    }

    #[tokio::test]
    async fn test_write_without_limit() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制写入
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());

        if let Poll::Ready(Ok(written)) = result {
            assert_eq!(written, 5);
        }
    }

    #[tokio::test]
    async fn test_write_without_limit2() {
        let error = Error::new(ErrorKind::Other, "write error");
        let mock_stream = MockStream::new(vec![]).with_write_error(error);
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试无限制写入
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());

        if let Poll::Ready(ret) = result {
            assert!(ret.is_err());
            if let Err(e) = ret {
                assert_eq!(e.kind(), ErrorKind::Other);
            }
        }
    }

    #[tokio::test]
    async fn test_write_without_limit3() {
        let mock_stream = MockStream::new(vec![]).with_write_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        limit_stream.raw_stream().write_should_pending = false;

        // 测试无限制写入
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());

        if let Poll::Ready(Ok(written)) = result {
            assert_eq!(written, 5);
        }
    }

    #[tokio::test]
    async fn test_write_without_limit4() {
        let mock_stream = MockStream::new(vec![]).with_write_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        limit_stream.raw_stream().write_should_pending = false;
        let ererror = Error::new(ErrorKind::Other, "write error");
        limit_stream.raw_stream().write_error = Some(ererror);

        // 测试无限制写入
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());

        if let Poll::Ready(ret) = result {
            assert!(ret.is_err());
            if let Err(e) = ret {
                assert_eq!(e.kind(), ErrorKind::Other);
            }
        }
    }

    #[tokio::test]
    async fn test_write_with_limit() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, Some(1))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次写入应该等待令牌
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        // 由于使用了实际的RateLimiter，可能返回Pending或Ready
        assert!(result.is_ready());
        if let Poll::Ready(Ok(written)) = result {
            assert_eq!(written, 1);
        }

        // 第一次写入应该等待令牌
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());
        let start = Instant::now();
        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_write(cx, &data)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1);

        let result = poll_fn(|cx| {
            Pin::new(&mut limit_stream).poll_write(cx, &data)
        }).await;
        assert!(start.elapsed() >= Duration::from_millis(1900));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_write_with_limit1() {
        let mock_stream = MockStream::new(vec![]).with_write_pending();
        let limit = Arc::new(MockLimit::new(None, Some(1))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        // 由于使用了实际的RateLimiter，可能返回Pending或Ready
        assert!(result.is_pending());
        limit_stream.raw_stream().write_should_pending = false;

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        tokio::time::sleep(Duration::from_millis(1100)).await;
        limit_stream.raw_stream().write_should_pending = true;

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());
        limit_stream.raw_stream().write_should_pending = false;
        let error = Error::new(ErrorKind::Other, "write error");
        limit_stream.raw_stream().write_error = Some(error);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(Err(e)) = result {
            assert_eq!(e.kind(), ErrorKind::Other);
        }
    }

    #[tokio::test]
    async fn test_write_with_limit2() {
        let mock_stream = MockStream::new(vec![]).with_write_pending();
        let limit = Arc::new(MockLimit::new(None, Some(2))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        // 由于使用了实际的RateLimiter，可能返回Pending或Ready
        assert!(result.is_pending());
        limit_stream.raw_stream().write_should_pending = false;

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn test_write_with_limit3() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, Some(2))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        // 由于使用了实际的RateLimiter，可能返回Pending或Ready
        assert!(result.is_pending());
        limit_stream.raw_stream().write_should_pending = false;

        tokio::time::sleep(Duration::from_millis(1100)).await;
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn test_write_with_limit4() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, Some(2))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        tokio::time::sleep(Duration::from_millis(1100)).await;
        limit_stream.raw_stream().write_should_pending = true;
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        limit_stream.raw_stream().write_should_pending = false;
        let error = Error::new(ErrorKind::Other, "write error");
        limit_stream.raw_stream().write_error = Some(error);
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_err());
        }
    }

    #[tokio::test]
    async fn test_write_with_limit5() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, Some(2))); // 100 bytes per second limit
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let data = [1];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_ok());
            assert_eq!(ret.unwrap(), 1);
        }

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());

        tokio::time::sleep(Duration::from_millis(1100)).await;

        let error = Error::new(ErrorKind::Other, "write error");
        limit_stream.raw_stream().write_error = Some(error);
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());
        if let Poll::Ready(ret) = result {
            assert!(ret.is_err());
        }
    }

    #[tokio::test]
    async fn test_read_error_propagation() {
        let error = Error::new(ErrorKind::Other, "read error");
        let mock_stream = MockStream::new(vec![]).with_read_error(error);
        let limit = Arc::new(MockLimit::new(Some(100), None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());

        if let Poll::Ready(Err(e)) = result {
            assert_eq!(e.kind(), ErrorKind::Other);
        }
    }

    #[tokio::test]
    async fn test_write_error_propagation() {
        let error = Error::new(ErrorKind::Other, "write error");
        let mock_stream = MockStream::new(vec![]).with_write_error(error);
        let limit = Arc::new(MockLimit::new(None, Some(100)));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_ready());

        if let Poll::Ready(Err(e)) = result {
            assert_eq!(e.kind(), ErrorKind::Other);
        }
    }

    #[tokio::test]
    async fn test_read_limit_pending_handling() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]).with_read_pending();
        let limit = Arc::new(MockLimit::new(Some(1), None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次应该返回Pending
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
    }

    #[tokio::test]
    async fn test_read_limit_pending_handling2() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(1), None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        limit_stream.set_limit_quota(1);

        let mut buffer = [0u8; 1];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次应该返回Pending
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_ready());
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
    }

    #[tokio::test]
    async fn test_read_pending_handling() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]).with_read_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次应该返回Pending
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
        let result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(result.is_pending());
    }

    #[tokio::test]
    async fn test_write_pending_handling() {
        let mock_stream = MockStream::new(vec![]).with_write_pending();
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let data = [1, 2, 3, 4, 5];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 第一次应该返回Pending
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());
        let result = Pin::new(&mut limit_stream).poll_write(&mut cx, &data);
        assert!(result.is_pending());
    }

    #[tokio::test]
    async fn test_flush_and_shutdown() {
        let mock_stream = MockStream::new(vec![]);
        let limit = Arc::new(MockLimit::new(None, None));
        let mut limit_stream = LimitStream::new(mock_stream, limit);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 测试flush
        let flush_result = Pin::new(&mut limit_stream).poll_flush(&mut cx);
        assert!(flush_result.is_ready());

        // 测试shutdown
        let shutdown_result = Pin::new(&mut limit_stream).poll_shutdown(&mut cx);
        assert!(shutdown_result.is_ready());
    }

    #[tokio::test]
    async fn test_mixed_read_write() {
        let mock_stream = MockStream::new(vec![1, 2, 3, 4, 5]);
        let limit = Arc::new(MockLimit::new(Some(100), Some(100)));
        let mut limit_stream = LimitStream::new(mock_stream, limit);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // 先写入数据
        let write_data = [6, 7, 8, 9, 10];
        let write_result = Pin::new(&mut limit_stream).poll_write(&mut cx, &write_data);
        assert!(write_result.is_ready());

        // 再读取数据
        let mut buffer = [0u8; 10];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let read_result = Pin::new(&mut limit_stream).poll_read(&mut cx, &mut read_buf);
        assert!(read_result.is_ready());
    }
}
