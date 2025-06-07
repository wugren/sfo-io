use std::{cmp, io};
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use crate::buf::{Buf, DEFAULT_MAX_BUF_SIZE};

#[async_trait::async_trait]
pub trait SimpleAsyncRead: Send + 'static + Unpin {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
}

#[async_trait::async_trait]
pub trait SimpleAsyncWrite: Send + 'static + Unpin {
    async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>;
    async fn flush(&mut self) -> std::io::Result<()>;
}

enum State<T> {
    Idle(Option<Buf>),
    Busy(Pin<Box<dyn Future<Output=(std::io::Result<usize>, Buf, T)>>>),
}

enum FlushState<T> {
    Idle,
    Busy(Pin<Box<dyn Future<Output=(std::io::Result<()>, T)>>>),
}

pub struct SimpleAsyncReadHolder<T: SimpleAsyncRead> {
    inner: Option<T>,
    state: State<T>,
}

impl<T: SimpleAsyncRead> SimpleAsyncReadHolder<T> {
    pub fn new(inner: T) -> SimpleAsyncReadHolder<T> {
        SimpleAsyncReadHolder {
            inner: Some(inner),
            state: State::Idle(Some(Buf::with_capacity(0))),
        }
    }
}

impl <T: SimpleAsyncRead> AsyncRead for SimpleAsyncReadHolder<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.state {
                State::Idle(ref mut buf_cell) => {
                    let mut buf = buf_cell.take().unwrap();

                    if !buf.is_empty() {
                        buf.copy_to(dst);
                        *buf_cell = Some(buf);
                        return Poll::Ready(Ok(()));
                    }

                    let mut inner = self.inner.take().unwrap();
                    let max_buf_size = cmp::min(dst.remaining(), DEFAULT_MAX_BUF_SIZE);
                    self.state = State::Busy(Box::pin(async move {
                        let ret = unsafe {buf.read_from_async(&mut inner, max_buf_size).await };
                        (ret, buf, inner)
                    }));
                }
                State::Busy(ref mut rx) => {
                    let (res, mut buf, inner) = ready!(Pin::new(rx).poll(cx));
                    self.inner = Some(inner);

                    match res {
                        Ok(_) => {
                            buf.copy_to(dst);
                            self.state = State::Idle(Some(buf));
                            return Poll::Ready(Ok(()));
                        }
                        Err(e) => {
                            assert!(buf.is_empty());

                            self.state = State::Idle(Some(buf));
                            return Poll::Ready(Err(e));
                        }
                    }
                }
            }
        }
    }
}

pub struct SimpleAsyncWriteHolder<T: SimpleAsyncWrite> {
    inner: Option<T>,
    state: State<T>,
    flush_state: FlushState<T>,
}

impl<T: SimpleAsyncWrite> SimpleAsyncWriteHolder<T> {
    pub fn new(inner: T) -> SimpleAsyncWriteHolder<T> {
        SimpleAsyncWriteHolder {
            inner: Some(inner),
            state: State::Idle(Some(Buf::with_capacity(0))),
            flush_state: FlushState::Idle,
        }
    }
}

impl<T: SimpleAsyncWrite> AsyncWrite for SimpleAsyncWriteHolder<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            match self.state {
                State::Idle(ref mut buf_cell) => {
                    let mut buf = buf_cell.take().unwrap();

                    assert!(buf.is_empty());

                    buf.copy_from(src, DEFAULT_MAX_BUF_SIZE);
                    let mut inner = self.inner.take().unwrap();

                    self.state = State::Busy(Box::pin(async move {
                        let res = buf.write_to_async(&mut inner).await;

                        (res, buf, inner)
                    }));
                }
                State::Busy(ref mut rx) => {
                    let (res, buf, inner) = ready!(Pin::new(rx).poll(cx));
                    self.state = State::Idle(Some(buf));
                    self.inner = Some(inner);

                    // If error, return
                    return Poll::Ready(res);
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        loop {
            match self.flush_state {
                // The buffer is not used here
                FlushState::Idle => {
                        let mut inner = self.inner.take().unwrap();

                        self.flush_state = FlushState::Busy(Box::pin(async move {
                            let res = inner.flush().await;
                            (res, inner)
                        }));
                }
                FlushState::Busy(ref mut rx) => {
                    let (res, inner) = ready!(Pin::new(rx).poll(cx));
                    self.flush_state = FlushState::Idle;
                    self.inner = Some(inner);

                    // If error, return
                    return Poll::Ready(res);
                }
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}
