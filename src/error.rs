pub use sfo_result::err as sfoio_err;
pub use sfo_result::into_err as into_sfoio_err;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SfoIOErrorCode {
    Failed,
    CmdReturnFailed,
}

pub type SfoIOResult<T> = sfo_result::Result<T, SfoIOErrorCode>;
#[allow(dead_code)]
pub type SfoIOError = sfo_result::Error<SfoIOErrorCode>;
