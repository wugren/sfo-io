use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use std::cmp;
use std::future::Future;
use std::io;
use std::io::prelude::*;
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::task::{spawn_blocking, JoinHandle};
use crate::buf::{Buf, DEFAULT_MAX_BUF_SIZE};

#[derive(Debug)]
pub struct ReadBlocking<T> {
    inner: Option<T>,
    state: State<T>,
}

#[derive(Debug)]
pub struct WriteBlocking<T> {
    inner: Option<T>,
    state: State<T>,
    flush_state: FlushState<T>,
}

#[derive(Debug)]
enum State<T> {
    Idle(Option<Buf>),
    Busy(JoinHandle<(io::Result<usize>, Buf, T)>),
}

#[derive(Debug)]
enum FlushState<T> {
    Idle,
    Busy(JoinHandle<(io::Result<()>, T)>),
}

impl<T> ReadBlocking<T> {
    pub fn new(inner: T) -> ReadBlocking<T> {
        ReadBlocking {
            inner: Some(inner),
            state: State::Idle(Some(Buf::with_capacity(0))),
        }
    }
}

impl<T> WriteBlocking<T> {
    pub fn new(inner: T) -> WriteBlocking<T> {
        WriteBlocking {
            inner: Some(inner),
            state: State::Idle(Some(Buf::with_capacity(0))),
            flush_state: FlushState::Idle,
        }
    }
}

impl<T> AsyncRead for ReadBlocking<T>
where
    T: Read + Unpin + Send + 'static,
{
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
                    self.state = State::Busy(spawn_blocking(move || {
                        // SAFETY: the requirements are satisfied by `Blocking::new`.
                        let res = unsafe { buf.read_from(&mut inner, max_buf_size) };
                        (res, buf, inner)
                    }));
                }
                State::Busy(ref mut rx) => {
                    let (res, mut buf, inner) = ready!(Pin::new(rx).poll(cx))?;
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

impl<T> AsyncWrite for WriteBlocking<T>
where
    T: Write + Unpin + Send + 'static,
{
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

                    let _n = buf.copy_from(src, DEFAULT_MAX_BUF_SIZE);
                    let mut inner = self.inner.take().unwrap();

                    self.state = State::Busy(spawn_blocking(move || {
                        let n = buf.len();
                        let res = buf.write_to(&mut inner).map(|()| n);

                        (res, buf, inner)
                    }));
                }
                State::Busy(ref mut rx) => {
                    let (res, buf, inner) = ready!(Pin::new(rx).poll(cx))?;
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

                    self.flush_state = FlushState::Busy(spawn_blocking(move || {
                        let res = inner.flush();
                        (res, inner)
                    }));
                }
                FlushState::Busy(ref mut rx) => {
                    let (res, inner) = ready!(Pin::new(rx).poll(cx))?;
                    self.flush_state = FlushState::Idle;
                    self.inner = Some(inner);

                    return Poll::Ready(res);
                }
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}
