#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::io;
use std::io::Error;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub trait SpeedStat: 'static + Send + Sync {
    fn get_write_speed(&self) -> u64;
    fn get_write_sum_size(&self) -> u64;
    fn get_read_speed(&self) -> u64;
    fn get_read_sum_size(&self) -> u64;
}

pub trait TimePicker: 'static + Sync + Send {
    fn now() -> u128;
}

pub struct SystemTimePicker;

impl TimePicker for SystemTimePicker {
    fn now() -> u128 {
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis()
    }
}

struct DataItem {
    size: u64,
    time: u64,
}

pub(crate) struct SpeedState<T: TimePicker> {
    sum_size: u64,
    last_time: u128,
    data_items: Vec<DataItem>,
    _time_picker: PhantomData<T>,
}

impl<T: TimePicker> SpeedState<T> {
    fn new() -> SpeedState<T> {
        SpeedState {
            sum_size: 0,
            last_time: T::now(),
            data_items: vec![],
            _time_picker: Default::default(),
        }
    }

    pub fn add_data(&mut self, size: u64) {
        self.sum_size += size;
        let now = T::now();
        self.clear_invalid_item(now);

        if now / 1000 == self.last_time / 1000 {
            if self.data_items.len() == 0 {
                self.data_items.push(DataItem {
                    size,
                    time: (now / 1000) as u64,
                });
            } else {
                let last_item = self.data_items.last_mut().unwrap();
                if last_item.time == (now / 1000) as u64 {
                    last_item.size += size;
                } else {
                    self.data_items.push(DataItem {
                        size,
                        time: (now / 1000) as u64,
                    });
                }
            }
        } else {
            let duration = now - self.last_time;
            let mut pos = 0;
            let mut offset = 1000 - self.last_time % 1000;
            let mut sec = (self.last_time / 1000) as u64;
            while pos < duration {
                let mut weight = offset;
                if pos + offset > duration {
                    weight = duration - pos;
                }
                let data_size = (size as u128 * weight / duration) as u64;
                if self.data_items.len() == 0 {
                    self.data_items.push(DataItem {
                        size: data_size,
                        time: sec,
                    });
                } else {
                    let last_item = self.data_items.last_mut().unwrap();
                    if last_item.time == sec {
                        last_item.size += data_size;
                    } else {
                        self.data_items.push(DataItem {
                            size: data_size,
                            time: sec,
                        })
                    }
                }
                pos += offset;
                offset = 1000;
                sec += 1;
                println!("{} {} {}", pos, offset, duration);
            }

        }
        self.last_time = now;
    }

    pub fn clear_invalid_item(&mut self, now: u128) {
        let now = (now / 1000) as u64;
        self.data_items.retain(|item| {
            (now - item.time) <= 5
        });
    }

    pub fn get_speed(&self) -> u64 {
        let now = (T::now() / 1000) as u64;
        let mut sum_size = 0;
        for item in self.data_items.iter() {
            if (now - item.time) <= 5 && now != item.time {
                sum_size += item.size;
            }
        }

        sum_size / 5
    }

    pub fn get_sum_size(&self) -> u64 {
        self.sum_size
    }
}

struct SpeedStatImpl<T: TimePicker> {
    upload_state: Mutex<SpeedState<T>>,
    download_state: Mutex<SpeedState<T>>,
}

impl<T: TimePicker> SpeedStatImpl<T> {
    pub fn new() -> SpeedStatImpl<T> {
        SpeedStatImpl {
            upload_state: Mutex::new(SpeedState::new()),
            download_state: Mutex::new(SpeedState::new()),
        }
    }

    pub fn add_upload_data(&self, size: u64) {
        self.upload_state.lock().unwrap().add_data(size);
    }

    pub fn add_download_data(&self, size: u64) {
        self.download_state.lock().unwrap().add_data(size);
    }
}

impl<T: TimePicker> SpeedStat for SpeedStatImpl<T> {
    fn get_write_speed(&self) -> u64 {
        self.upload_state.lock().unwrap().get_speed()
    }

    fn get_write_sum_size(&self) -> u64 {
        self.upload_state.lock().unwrap().get_sum_size()
    }

    fn get_read_speed(&self) -> u64 {
        self.download_state.lock().unwrap().get_speed()
    }

    fn get_read_sum_size(&self) -> u64 {
        self.download_state.lock().unwrap().get_sum_size()
    }
}

pub struct StatStream<T: AsyncRead + AsyncWrite + Unpin + Send + 'static, S: TimePicker = SystemTimePicker> {
    stream: T,
    stat: Arc<SpeedStatImpl<S>>,
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> StatStream<T> {
    pub fn new(stream: T) -> StatStream<T> {
        StatStream {
            stream,
            stat: Arc::new(SpeedStatImpl::new()),
        }
    }

}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static, S: TimePicker> StatStream<T, S> {
    pub(crate) fn new_test(stream: T) -> StatStream<T, S> {
        StatStream {
            stream,
            stat: Arc::new(SpeedStatImpl::new()),
        }
    }

    pub fn get_speed_stat(&self) -> Arc<dyn SpeedStat> {
        self.stat.clone()
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static, S: TimePicker> AsyncRead for StatStream<T, S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Ready(res) => {
                if res.is_ok() {
                    self.stat.add_download_data(buf.filled().len() as u64);
                }
                Poll::Ready(res)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static, S: TimePicker> AsyncWrite for StatStream<T, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Ready(res) => {
                if res.is_ok() {
                    self.stat.add_upload_data(buf.len() as u64);
                }
                Poll::Ready(res)
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_speed_state_new() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        set_mock_time(1000);
        let state: SpeedState<MockTimePicker> = SpeedState::new();

        assert_eq!(state.sum_size, 0);
        assert_eq!(state.last_time, 1000);
        assert_eq!(state.data_items.len(), 0);
    }

    #[test]
    fn test_add_data_same_second() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        set_mock_time(1500);
        let mut state: SpeedState<MockTimePicker> = SpeedState::new();

        state.add_data(100);
        assert_eq!(state.sum_size, 100);
        assert_eq!(state.data_items.len(), 1);
        assert_eq!(state.data_items[0].size, 100);
        assert_eq!(state.data_items[0].time, 1); // 1000 / 1000 = 1

        state.add_data(200);
        assert_eq!(state.sum_size, 300);
        assert_eq!(state.data_items.len(), 1);
        assert_eq!(state.data_items[0].size, 300); // 合并到同一秒
    }

    #[test]
    fn test_add_data_different_seconds() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        set_mock_time(1000);
        let mut state: SpeedState<MockTimePicker> = SpeedState::new();

        state.add_data(100);
        advance_mock_time(2000); // 时间前进到3000ms
        state.add_data(200);

        assert_eq!(state.sum_size, 300);
        assert_eq!(state.data_items.len(), 2);
        assert_eq!(state.data_items[0].size, 200);
        assert_eq!(state.data_items[0].time, 1);
        assert_eq!(state.data_items[1].size, 100);
        assert_eq!(state.data_items[1].time, 2);

        assert_eq!(state.get_speed(), 60);
        advance_mock_time(500);
        assert_eq!(state.get_speed(), 60);
        //
        state.add_data(300);
        assert_eq!(state.sum_size, 600);
        assert_eq!(state.get_speed(), 60);
        advance_mock_time(500);
        assert_eq!(state.get_speed(), 120);
    }

    #[test]
    fn test_add_data_cross_seconds_distribution() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        // 测试跨秒时数据如何分配到不同的秒中
        set_mock_time(1500); // 1.5秒
        let mut state: SpeedState<MockTimePicker> = SpeedState::new();
        advance_mock_time(1500);

        // 从1500ms到3000ms，增加1500ms，跨越2个完整的秒(2s和3s)
        // 1.5s到2s有500ms，2s到3s有1000ms
        // 总共1500ms，添加300字节数据
        state.add_data(300);
        advance_mock_time(1500);

        // 应该创建两个数据项: 一个在第2秒，一个在第3秒
        // 第2秒应该有 300 * 500/1500 = 100 字节
        // 第3秒应该有 300 * 1000/1500 = 200 字节
        assert_eq!(state.data_items.len(), 2);
        assert_eq!(state.data_items[0].time, 1);
        assert_eq!(state.data_items[0].size, 100);
        assert_eq!(state.data_items[1].time, 2);
        assert_eq!(state.data_items[1].size, 200);

        // 测试跨秒时数据如何分配到不同的秒中
        set_mock_time(1500); // 1.5秒
        let mut state: SpeedState<MockTimePicker> = SpeedState::new();
        advance_mock_time(2000);

        // 从1500ms到3000ms，增加1500ms，跨越2个完整的秒(2s和3s)
        // 1.5s到2s有500ms，2s到3s有1000ms
        // 总共1500ms，添加300字节数据
        state.add_data(400);
        advance_mock_time(1500);

        // 应该创建两个数据项: 一个在第2秒，一个在第3秒
        // 第2秒应该有 300 * 500/1500 = 100 字节
        // 第3秒应该有 300 * 1000/1500 = 200 字节
        assert_eq!(state.data_items.len(), 3);
        assert_eq!(state.data_items[0].time, 1);
        assert_eq!(state.data_items[0].size, 100);
        assert_eq!(state.data_items[1].time, 2);
        assert_eq!(state.data_items[1].size, 200);
        assert_eq!(state.data_items[2].time, 3);
        assert_eq!(state.data_items[2].size, 100);
    }

    #[test]
    fn test_clear_invalid_item() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        let mut state: SpeedState<MockTimePicker> = SpeedState::new();

        // 添加几个不同时间的数据项
        state.data_items.push(DataItem { size: 100, time: 5 }); // 5秒时的数据，应该被清除
        state.data_items.push(DataItem { size: 200, time: 7 }); // 7秒时的数据，应该保留
        state.data_items.push(DataItem { size: 300, time: 8 }); // 8秒时的数据，应该保留

        set_mock_time(11000); // 当前时间10秒
        state.clear_invalid_item(MockTimePicker::now());

        assert_eq!(state.data_items.len(), 2);
        assert_eq!(state.data_items[0].time, 7);
        assert_eq!(state.data_items[1].time, 8);
    }

    #[test]
    fn test_get_speed() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        set_mock_time(10000); // 10秒
        let mut state: SpeedState<MockTimePicker> = SpeedState::new();

        state.add_data(100);
        advance_mock_time(1000);
        state.add_data(200);
        advance_mock_time(1000);
        state.add_data(300);
        advance_mock_time(1000);
        state.add_data(400);
        advance_mock_time(1000);
        state.add_data(500);
        advance_mock_time(1000);
        state.add_data(600);
        advance_mock_time(1000);
        state.add_data(700);

        let speed = state.get_speed();
        // 应该计算 300+400+500+600+700 = 2500 字节在4秒内 => 2500/5 = 500 bytes/sec
        assert_eq!(speed, 500);
    }

    #[test]
    fn test_get_sum_size() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        let mut state: SpeedState<MockTimePicker> = SpeedState::new();
        state.add_data(100);
        state.add_data(200);
        assert_eq!(state.get_sum_size(), 300);
    }

    #[test]
    fn test_speed_stat_impl() {
        // Mock TimePicker for testing
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        let stat: SpeedStatImpl<MockTimePicker> = SpeedStatImpl::new();

        stat.add_upload_data(100);
        stat.add_download_data(200);

        // 由于没有时间流逝，速度为0
        assert_eq!(stat.get_write_speed(), 0);
        assert_eq!(stat.get_read_speed(), 0);

        // 模拟时间流逝后再次检查
        set_mock_time(5000);
        stat.add_upload_data(500);
        stat.add_download_data(1000);
        set_mock_time(6000);
        stat.add_upload_data(0);
        stat.add_download_data(0);

        // 现在应该有速度了
        assert!(stat.get_write_speed() > 0);
        assert!(stat.get_read_speed() > 0);
    }

    // 注意: StatStream的测试需要tokio运行时，这里省略了复杂的AsyncRead/AsyncWrite mock
    #[tokio::test]
    async fn test_stat_stream_creation() {
        static MOCK_TIME: AtomicU64 = AtomicU64::new(0);

        struct MockTimePicker;

        impl TimePicker for MockTimePicker {
            fn now() -> u128 {
                MOCK_TIME.load(Ordering::Relaxed) as u128
            }
        }

        // Helper function to set mock time
        fn set_mock_time(time_ms: u64) {
            MOCK_TIME.store(time_ms, Ordering::Relaxed);
        }

        // Helper function to advance mock time
        fn advance_mock_time(delta_ms: u64) {
            MOCK_TIME.fetch_add(delta_ms, Ordering::Relaxed);
        }

        // 创建一个简单的mock stream用于测试
        struct MockStream {
            future: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
        }

        impl AsyncRead for MockStream {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                if self.future.is_none() {
                    self.future = Some(Box::pin(tokio::time::sleep(Duration::from_millis(10))));
                }
                match Pin::new(self.future.as_mut().unwrap()).poll(_cx) {
                    Poll::Ready(_) => {
                        self.future = None;
                        _buf.set_filled(10);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }

        impl AsyncWrite for MockStream {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<Result<usize, io::Error>> {
                if self.future.is_none() {
                    self.future = Some(Box::pin(tokio::time::sleep(Duration::from_millis(10))));
                }
                match Pin::new(self.future.as_mut().unwrap()).poll(_cx) {
                    Poll::Ready(_) => {
                        self.future = None;
                        Poll::Ready(Ok(buf.len()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Result<(), io::Error>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>
            ) -> Poll<Result<(), Error>> {
                Poll::Ready(Ok(()))
            }
        }

        impl Unpin for MockStream {}

        let stream = MockStream{
            future: None,
        };
        let mut stat_stream = StatStream::<_, MockTimePicker>::new_test(stream);
        let speed_stat = stat_stream.get_speed_stat();
        let mut upload_size = 0;
        let mut download_size = 0;
        let mut buf = vec![0u8; 4096];
        advance_mock_time(500);
        for i in 0..100 {
            let size = stat_stream.write(&buf).await.unwrap();
            stat_stream.flush().await.unwrap();
            upload_size += size;
            let size = stat_stream.read(&mut buf).await.unwrap();
            download_size += size;
            advance_mock_time(1000);
            if i < 5 {
                assert_eq!(speed_stat.get_write_speed(), (upload_size / 5) as u64);
                assert_eq!(speed_stat.get_read_speed(), (download_size / 5) as u64);
            } else {
                assert_eq!(speed_stat.get_write_sum_size(), upload_size as u64);
                assert_eq!(speed_stat.get_read_sum_size(), download_size as u64);
                assert_eq!(speed_stat.get_write_speed(), (4096 * 5 - 2048)/5);
                assert_eq!(speed_stat.get_read_speed(), (10 * 5 - 5)/5);
            }
        }
        stat_stream.shutdown().await.unwrap();
    }
}
