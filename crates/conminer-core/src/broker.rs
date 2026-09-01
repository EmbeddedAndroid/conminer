//! Console broker: one reader per tty, fanned out to every consumer.
//!
//! # Why this exists
//!
//! Every consumer used to open its OWN ser2net connection to the same console:
//! minerd for capture, dashd for the web terminal, mcpd for probes and
//! `run_command`. ser2net does fan out TCP clients, so that mostly worked -- but
//! it makes every consumer a party to the same scarce resource, and when the
//! device open fails there is no single place that knows. Measured on the rig:
//! the RIDE's AP console delivered 0 bytes through ser2net to all of them while
//! the tty itself produced 102190 bytes in the same window, and each consumer
//! independently concluded "the board is quiet".
//!
//! minerd already holds a permanent connection to every device, because that is
//! what capture is. So it is the natural owner: it reads once and republishes,
//! and everyone else subscribes here instead of adding another reader.
//!
//! # What this is NOT
//!
//! Not a ser2net replacement. ser2net still owns the tty, the line settings and
//! transmit. This only removes the *duplicate readers*, which is the part that
//! made contention everyone's problem and nobody's job.
//!
//! Transmit deliberately does not go through here: TX goes straight to ser2net,
//! by explicit design choice, so the broker stays a one-way fan-out and a broker
//! outage can never block a keystroke reaching a board.
//!
//! # Transport
//!
//! A Unix socket in the shared run dir, which every container already mounts.
//! Protocol, deliberately trivial so a human can drive it with `socat`:
//!
//! ```text
//! client -> "<device canonical name>\n"
//! server -> raw console bytes, forever
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

/// How many chunks a slow subscriber may fall behind before it starts losing
/// them.
///
/// A subscriber that cannot keep up MUST lose data rather than stall the
/// publisher: capture is the system of record and a wedged web terminal must
/// never be able to block it. `broadcast` gives exactly that -- laggards get a
/// `Lagged` error and resume at the newest chunk.
const BACKLOG: usize = 1024;

/// The fan-out registry: one channel per device.
#[derive(Default)]
pub struct Hub {
    channels: Mutex<HashMap<String, broadcast::Sender<Arc<[u8]>>>>,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn channel(&self, device: &str) -> broadcast::Sender<Arc<[u8]>> {
        let mut g = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(device.to_string())
            .or_insert_with(|| broadcast::channel(BACKLOG).0)
            .clone()
    }

    /// Publish one chunk. Never blocks and never fails: with no subscribers the
    /// send is simply dropped, which is the normal case on a quiet rig.
    pub fn publish(&self, device: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.channel(device).send(Arc::from(bytes));
    }

    pub fn subscribe(&self, device: &str) -> broadcast::Receiver<Arc<[u8]>> {
        self.channel(device).subscribe()
    }

    /// Number of live subscribers, for metrics and tests.
    pub fn subscriber_count(&self, device: &str) -> usize {
        self.channel(device).receiver_count()
    }
}

/// Default socket path inside the shared run dir.
pub fn socket_path(run_dir: &Path) -> PathBuf {
    run_dir.join("broker.sock")
}

/// Serve subscribers until `shutdown` flips.
pub async fn serve(
    hub: Arc<Hub>,
    path: PathBuf,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A socket file left behind by a killed process would make bind fail
    // forever, which would silently cost every consumer its console.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let hub = hub.clone();
                tokio::spawn(async move {
                    let _ = serve_one(hub, stream).await;
                });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn serve_one(hub: Arc<Hub>, stream: UnixStream) -> std::io::Result<()> {
    let (rx, mut tx) = stream.into_split();
    let mut lines = BufReader::new(rx).lines();
    let Some(device) = lines.next_line().await? else {
        return Ok(());
    };
    let device = device.trim().to_string();
    if device.is_empty() {
        return Ok(());
    }

    let mut sub = hub.subscribe(&device);
    loop {
        match sub.recv().await {
            Ok(chunk) => tx.write_all(&chunk).await?,
            // The subscriber fell behind. Say so rather than pretending the
            // board went quiet -- a silent gap is indistinguishable from an idle
            // console, and that ambiguity has cost real debugging time here.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(device = %device, missed = n, "broker subscriber lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

/// Subscribe to a device's console stream.
///
/// Returns the connected stream, from which the caller reads raw console bytes.
pub async fn connect(path: &Path, device: &str) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(path).await?;
    stream
        .write_all(format!("{}\n", device.trim()).as_bytes())
        .await?;
    stream.flush().await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn a_chunk_reaches_every_subscriber() {
        let hub = Hub::new();
        let mut a = hub.subscribe("/dev/x");
        let mut b = hub.subscribe("/dev/x");
        hub.publish("/dev/x", b"hello");

        assert_eq!(&*a.try_recv().unwrap(), b"hello");
        assert_eq!(
            &*b.try_recv().unwrap(),
            b"hello",
            "a second consumer must not have to open its own connection to see this"
        );
    }

    #[test]
    fn devices_do_not_bleed_into_each_other() {
        let hub = Hub::new();
        let mut other = hub.subscribe("/dev/other");
        hub.publish("/dev/x", b"for x");
        assert!(
            other.try_recv().is_err(),
            "a device's bytes must never reach another device's subscriber"
        );
    }

    /// Capture is the system of record. A stuck web terminal must lose bytes
    /// rather than stall the publisher.
    #[test]
    fn a_subscriber_that_never_reads_cannot_stall_the_publisher() {
        let hub = Hub::new();
        let _slow = hub.subscribe("/dev/x");
        for i in 0..(BACKLOG * 3) {
            hub.publish("/dev/x", format!("chunk {i}").as_bytes());
        }
        // Publishing stayed non-blocking and the newest data is still flowing.
        let mut fresh = hub.subscribe("/dev/x");
        hub.publish("/dev/x", b"latest");
        assert_eq!(&*fresh.try_recv().unwrap(), b"latest");
    }

    #[test]
    fn publishing_with_no_subscribers_is_fine() {
        let hub = Hub::new();
        hub.publish("/dev/nobody", b"data"); // must not panic
        assert_eq!(hub.subscriber_count("/dev/nobody"), 0);
    }

    #[tokio::test]
    async fn a_client_over_the_socket_receives_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path());
        let hub = Hub::new();
        let (stop, rx) = tokio::sync::watch::channel(false);

        let h = hub.clone();
        let p = path.clone();
        tokio::spawn(async move { serve(h, p, rx).await });

        // Wait for bind rather than sleeping a fixed amount.
        let mut client = None;
        for _ in 0..100 {
            if let Ok(c) = connect(&path, "/dev/x").await {
                client = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut client = client.expect("broker must accept a subscriber");

        // Wait until the server has registered us, then publish.
        for _ in 0..100 {
            if hub.subscriber_count("/dev/x") > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        hub.publish("/dev/x", b"BOOT LOG LINE");

        let mut buf = [0u8; 13];
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_exact(&mut buf),
        )
        .await
        .expect("stream must deliver")
        .expect("read");
        assert_eq!(&buf[..n], b"BOOT LOG LINE");

        let _ = stop.send(true);
    }

    /// A socket left behind by a killed minerd must not lock every consumer out
    /// of every console until someone deletes a file by hand.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_block_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path());
        std::fs::write(&path, b"stale").unwrap();

        let (stop, rx) = tokio::sync::watch::channel(false);
        let h = Hub::new();
        let p = path.clone();
        let task = tokio::spawn(async move { serve(h, p, rx).await });

        let mut ok = false;
        for _ in 0..100 {
            if connect(&path, "/dev/x").await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ok, "a stale socket file must be replaced, not fatal");
        let _ = stop.send(true);
        let _ = task.await;
    }
}
