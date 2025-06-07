use tokio::io::{AsyncReadExt, AsyncWriteExt};
use shlex::Shlex;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use crate::error::{into_sfoio_err, sfoio_err, SfoIOErrorCode, SfoIOResult};

pub struct QAProcess {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    child: Child,
}

impl QAProcess {
    pub fn new(mut child: Child) -> Self {
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
        }
    }

    pub async fn answer(&mut self, question: &str, answer: &str) -> SfoIOResult<()> {
        // self.stdin.as_mut().unwrap().write_all(answer.as_bytes()).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
        log::info!("{} start", question);
        let mut offset = 0;
        let mut buf = [0u8; 4096];
        let mut error_buf = [0u8; 4096];
        let mut error_offset = 0;
        loop {
            if offset == buf.len() || error_offset == error_buf.len() {
                return Err(sfoio_err!(SfoIOErrorCode::Failed, "Buffer overflow"));
            }

            tokio::select! {
                ret = self.stderr.as_mut().unwrap().read(&mut error_buf[error_offset..error_offset+1]) => {
                    match ret {
                        Ok(len) => {
                            if len == 0 {
                                return Err(sfoio_err!(SfoIOErrorCode::Failed, "EOF"));
                            }
                            error_offset += len;
                            let current = String::from_utf8_lossy(&error_buf[..error_offset]).to_string();
                            // log::info!("current err:{}", current);
                            if current.ends_with(question) {
                                let stdin = self.stdin.as_mut().ok_or(sfoio_err!(SfoIOErrorCode::Failed, "Failed to get stdin"))?;
                                // log::info!("write:{}", answer);
                                stdin.write_all(answer.as_bytes()).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
                                stdin.write_all("\n".as_bytes()).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
                                // log::info!("write:{} finish", answer);
                                break;
                            }
                        },
                        Err(e) => {
                            return Err(into_sfoio_err!(SfoIOErrorCode::Failed)(e))
                        }
                    }
                },
                ret = self.stdout.as_mut().unwrap().read(&mut buf[offset..offset+1]) => {
                    match ret {
                        Ok(len) => {
                            if len == 0 {
                                return Err(sfoio_err!(SfoIOErrorCode::Failed, "EOF"));
                            }
                            offset += len;
                            let current = String::from_utf8_lossy(&buf[..offset]).to_string();
                            // log::info!("current:{}", current);
                            if current.ends_with(question) {
                                let stdin = self.stdin.as_mut().ok_or(sfoio_err!(SfoIOErrorCode::Failed, "Failed to get stdin"))?;
                                // log::info!("write:{}", answer);
                                stdin.write_all(answer.as_bytes()).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
                                stdin.write_all("\n".as_bytes()).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
                                // log::info!("write:{} finish", answer);
                                break;
                            }
                        },
                        Err(e) => {
                            return Err(into_sfoio_err!(SfoIOErrorCode::Failed)(e))
                        }
                    }
                }
                _ = self.child.wait() => {
                    break;
                }
            }
        }
        log::info!("{} complete", question);
        Ok(())
    }

    pub async fn wait(&mut self) -> SfoIOResult<()> {
        let status = self.child.wait().await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
        if status.success() {
            Ok(())
        } else {
            let stderr = self.stderr.as_mut().ok_or(sfoio_err!(SfoIOErrorCode::Failed, "Failed to get stderr"))?;
            let mut error = Vec::new();
            stderr.read_to_end(&mut error).await.map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;

            Err(sfoio_err!(SfoIOErrorCode::Failed, "{}", String::from_utf8_lossy(error.as_slice())))
        }
    }
}

pub async fn execute(cmd: &str) -> SfoIOResult<Vec<u8>> {
    log::info!("{}", cmd);
    let mut lexer = Shlex::new(cmd);
    let args: Vec<String> = lexer.by_ref().collect();
    let output = tokio::process::Command::new(args[0].as_str())
        .args(&args[1..])
        .output()
        .await
        .map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;
    if output.status.success() {
        log::info!("success:{}", String::from_utf8_lossy(output.stdout.as_slice()));
        Ok(output.stdout)
    } else {
        Err(sfoio_err!(SfoIOErrorCode::CmdReturnFailed, "{}", String::from_utf8_lossy(output.stderr.as_slice())))
    }
}

pub async fn spawn(cmd: &str) -> SfoIOResult<QAProcess> {
    log::info!("{}\n", cmd);
    let mut lexer = Shlex::new(cmd);
    let args: Vec<String> = lexer.by_ref().collect();
    let child = tokio::process::Command::new(args[0].as_str())
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(into_sfoio_err!(SfoIOErrorCode::Failed))?;

    Ok(QAProcess::new(child))
}
