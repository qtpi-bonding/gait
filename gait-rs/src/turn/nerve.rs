//! The Nerve — the brain↔spine IPC channel.
//!
//! Wire format: `[version: u8][len: u32 BE][Signal proto bytes]`.
//! One `Signal` per frame; signals carry a `corr_id` for multiplexing parallel
//! `fire()` calls. The reader task demuxes by `corr_id` into per-call oneshots
//! so concurrent calls don't race the read stream.
#![allow(dead_code)] // Phase 3 seam: used by RelaySynapse + spine (Task 5 / Task 4a)

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};

use gait_proto::gait::sandbox::v1::Signal;

/// Version byte prefix on every frame — cheap wire versioning.
const NERVE_VERSION: u8 = 1;

/// Write one `Signal` as a length-prefixed binary protobuf frame.
///
/// Format: `[NERVE_VERSION: u8][len: u32 BE][Signal bytes]`.
pub async fn write_signal<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    signal: &Signal,
) -> anyhow::Result<()> {
    let bytes = signal.encode_to_vec();
    let len = bytes.len() as u32;
    w.write_u8(NERVE_VERSION).await.context("write version")?;
    w.write_u32(len).await.context("write len")?;
    w.write_all(&bytes).await.context("write signal bytes")?;
    Ok(())
}

/// Read one `Signal` frame.
pub async fn read_signal<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Signal> {
    let version = r.read_u8().await.context("read version")?;
    if version != NERVE_VERSION {
        anyhow::bail!("nerve: unsupported version {version} (expected {NERVE_VERSION})");
    }
    let len = r.read_u32().await.context("read len")? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.context("read signal bytes")?;
    Signal::decode(buf.as_slice()).context("decode Signal")
}

// ── Demux reader ─────────────────────────────────────────────────────────────

/// Per-call pending map: corr_id → waiter.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Signal>>>>;

/// The write half of the Nerve, guarded by a mutex so multiple `fire()` calls can
/// write concurrently without interleaving bytes.
#[derive(Clone)]
pub struct NerveWriter {
    inner: Arc<Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
    next_corr_id: Arc<AtomicU64>,
    pending: Pending,
}

impl NerveWriter {
    /// Allocate the next correlation ID.
    pub fn alloc_corr_id(&self) -> u64 {
        self.next_corr_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a oneshot that will receive the `Feedback` (or `Append` ack) for
    /// the given `corr_id`, then write `signal` to the pipe.
    pub async fn send_and_register(
        &self,
        signal: &Signal,
    ) -> anyhow::Result<oneshot::Receiver<Signal>> {
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            p.insert(signal.corr_id, tx);
        }
        {
            let mut w = self.inner.lock().await;
            write_signal(&mut *w, signal).await?;
        }
        Ok(rx)
    }

    /// Write a one-way signal that does NOT expect a reply (e.g. `Append`).
    pub async fn send_one_way(&self, signal: &Signal) -> anyhow::Result<()> {
        let mut w = self.inner.lock().await;
        write_signal(&mut *w, signal).await?;
        w.flush().await.context("flush nerve")?;
        Ok(())
    }
}

/// Spawn the Nerve reader task: reads `Signal`s from `read_half`, dispatches each to
/// the matching `oneshot::Sender` registered by the writer. Returns a `NerveWriter`
/// bound to `write_half` + the shared pending map.
///
/// The reader task exits when the read half closes (spine exited / pipe broken).
/// Callers can `await` the `JoinHandle` to detect that; for a single-turn brain the
/// handle is simply dropped.
pub fn start_nerve<R, W>(read_half: R, write_half: W) -> (NerveWriter, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let writer = NerveWriter {
        inner: Arc::new(Mutex::new(Box::new(write_half))),
        next_corr_id: Arc::new(AtomicU64::new(1)),
        pending: pending.clone(),
    };

    let handle = tokio::spawn(async move {
        let mut r = read_half;
        while let Ok(sig) = read_signal(&mut r).await {
            let sender = {
                let mut p = pending.lock().await;
                p.remove(&sig.corr_id)
            };
            if let Some(tx) = sender {
                let _ = tx.send(sig);
            }
            // Unknown corr_id: silently drop (e.g. one-way Append acks from spine).
        }
        // EOF or pipe broken — reader exits cleanly.
    });

    (writer, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_proto::gait::sandbox::v1::{signal, Impulse};

    /// Verify that two signals with different `corr_id`s survive a round-trip
    /// through `write_signal` / `read_signal` over a `tokio::io::duplex` pipe.
    #[tokio::test]
    async fn round_trip_two_signals() {
        let (mut client, mut server) = tokio::io::duplex(4096);

        let impulse1 = Signal {
            corr_id: 42,
            body: Some(signal::Body::Impulse(Impulse {
                tool: "read".into(),
                ..Default::default()
            })),
        };
        let impulse2 = Signal {
            corr_id: 99,
            body: Some(signal::Body::Impulse(Impulse {
                tool: "write".into(),
                ..Default::default()
            })),
        };

        write_signal(&mut client, &impulse1).await.unwrap();
        write_signal(&mut client, &impulse2).await.unwrap();

        let r1 = read_signal(&mut server).await.unwrap();
        let r2 = read_signal(&mut server).await.unwrap();

        assert_eq!(r1.corr_id, 42);
        assert_eq!(r2.corr_id, 99);
        match r1.body.unwrap() {
            signal::Body::Impulse(i) => assert_eq!(i.tool, "read"),
            _ => panic!("wrong body"),
        }
        match r2.body.unwrap() {
            signal::Body::Impulse(i) => assert_eq!(i.tool, "write"),
            _ => panic!("wrong body"),
        }
    }

    /// Verify that two responses arriving OUT of order are correctly demuxed by
    /// the reader task via `start_nerve` + `NerveWriter::send_and_register`.
    #[tokio::test]
    async fn out_of_order_demux() {
        use gait_proto::gait::sandbox::v1::Feedback;

        // brain side (client) ↔ spine side (server).
        let (brain_read, spine_write) = tokio::io::duplex(4096);
        let (spine_read, brain_write) = tokio::io::duplex(4096);

        // Start nerve on the brain side.
        let (writer, _handle) = start_nerve(brain_read, brain_write);

        // Send two requests from the brain (corr_id 1 and 2).
        let corr1 = writer.alloc_corr_id();
        let corr2 = writer.alloc_corr_id();

        let sig1 = Signal {
            corr_id: corr1,
            body: Some(signal::Body::Impulse(Impulse {
                tool: "read".into(),
                ..Default::default()
            })),
        };
        let sig2 = Signal {
            corr_id: corr2,
            body: Some(signal::Body::Impulse(Impulse {
                tool: "write".into(),
                ..Default::default()
            })),
        };

        let rx1 = writer.send_and_register(&sig1).await.unwrap();
        let rx2 = writer.send_and_register(&sig2).await.unwrap();

        // Spine echoes responses back in REVERSE order (2, then 1).
        let mut spine_r = spine_read;
        let mut spine_w = spine_write;
        let _s1 = read_signal(&mut spine_r).await.unwrap();
        let _s2 = read_signal(&mut spine_r).await.unwrap();

        let resp2 = Signal {
            corr_id: corr2,
            body: Some(signal::Body::Feedback(Feedback {
                result: None,
                events: vec![],
                denied: None,
                daemon: None,
            })),
        };
        let resp1 = Signal {
            corr_id: corr1,
            body: Some(signal::Body::Feedback(Feedback {
                result: None,
                events: vec![],
                denied: None,
                daemon: None,
            })),
        };
        write_signal(&mut spine_w, &resp2).await.unwrap();
        write_signal(&mut spine_w, &resp1).await.unwrap();

        // Brain side receives both regardless of order.
        let got1 = rx1.await.unwrap();
        let got2 = rx2.await.unwrap();
        assert_eq!(got1.corr_id, corr1);
        assert_eq!(got2.corr_id, corr2);
    }
}
