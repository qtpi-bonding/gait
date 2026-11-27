//! Helpers for the spine's read-error diagnostic: how the brain process ended
//! (`describe_exit`) and the last bytes of its forwarded stderr (`StderrTail`).

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How many trailing stderr bytes `StderrTail` keeps for the diagnostic record.
pub(super) const TAIL_BYTES: usize = 2048;

/// Keeps the last `TAIL_BYTES` bytes of the brain's forwarded stderr so a
/// read-error diagnostic can quote them lossily.
#[derive(Clone, Default)]
pub(super) struct StderrTail(Arc<Mutex<Vec<u8>>>);

impl StderrTail {
    /// Lossy UTF-8 of the kept tail bytes.
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Forward `from` to `to` chunk by chunk, remembering the last `TAIL_BYTES`
/// bytes in `tail`. Write errors are ignored (a closed destination must not
/// stop the reading); a read error ends the copy like end of file.
pub(super) async fn copy_keeping_tail<R, W>(mut from: R, mut to: W, tail: &StderrTail)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut chunk = [0u8; 4096];
    loop {
        match from.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                {
                    let mut tail = tail.0.lock().unwrap();
                    tail.extend_from_slice(&chunk[..n]);
                    let excess = tail.len().saturating_sub(TAIL_BYTES);
                    tail.drain(..excess);
                }
                if to.write_all(&chunk[..n]).await.is_err() {
                    continue;
                }
                let _ = to.flush().await;
            }
        }
    }
}

/// Describe how the brain process ended, without ever calling `child.wait()`
/// (which can hang forever on macOS — see `reap_brain` in lifecycle.rs).
/// Polls `try_wait` every 20 ms for at most 2 s.
pub(super) async fn describe_exit(child: &mut tokio::process::Child) -> String {
    use std::time::Duration;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.to_string(),
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    return "still running after 2s".to_string();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return format!("unknown ({error})"),
        }
    }
}

/// Wait at most 500 ms for the copy task. If it finished, set the option to
/// `None` (a finished JoinHandle must not be polled again); otherwise leave
/// it running.
pub(super) async fn settle(copy: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = copy {
        if tokio::time::timeout(std::time::Duration::from_millis(500), handle)
            .await
            .is_ok()
        {
            *copy = None;
        }
    }
}

/// How a brain that sent no signal ended. Its stderr tail is kept in
/// diagnostic.jsonl only.
pub(super) async fn name_early_exit(
    child: &mut tokio::process::Child,
    copy: &mut Option<tokio::task::JoinHandle<()>>,
    tail: &StderrTail,
    diag: &crate::turn::DiagnosticSink,
    brain_pid: u32,
) -> String {
    let brain_exit = describe_exit(child).await;
    settle(copy).await;
    let detail = format!(
        "brain_died_early brain_pid={brain_pid} brain_exit={brain_exit} stderr_tail={:?}",
        tail.text()
    );
    let rec = crate::diagnostic::DiagRecord::terminal(
        diag.session_id(),
        crate::diagnostic::Phase::TurnBegin,
        &detail,
    );
    diag.keep(&rec.to_json_line());
    brain_exit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_input() -> Vec<u8> {
        [vec![b'a'; 10_000], b"the-end".to_vec()].concat()
    }

    #[tokio::test]
    async fn stderr_is_forwarded_whole_and_only_its_tail_is_kept() {
        let input = long_input();
        let mut forwarded = Vec::new();
        let tail = StderrTail::default();
        copy_keeping_tail(&input[..], &mut forwarded, &tail).await;
        assert_eq!(forwarded, input);
        assert_eq!(tail.text().len(), TAIL_BYTES);
        assert!(tail.text().ends_with("the-end"));
    }

    #[tokio::test]
    async fn a_closed_destination_does_not_stop_the_reading() {
        let (closed, peer) = tokio::io::duplex(16);
        drop(peer);
        let tail = StderrTail::default();
        copy_keeping_tail(&long_input()[..], closed, &tail).await;
        assert!(tail.text().ends_with("the-end"));
    }
}
