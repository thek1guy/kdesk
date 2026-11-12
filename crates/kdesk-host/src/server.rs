//! TCP server. Listens on localhost only (the phone reaches it through
//! `adb reverse`), serves one client at a time.

use anyhow::{bail, Context, Result};
use kdesk_proto::{Message, TouchAction, CODEC_H264, HEADER_LEN, PROTOCOL_VERSION};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{tcp::OwnedReadHalf, TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::capture::{Capture, Feedback};
use crate::config::Config;
use crate::portal::PortalSession;

/// How many encoded frames may wait for the socket before we start dropping.
const QUEUE_DEPTH: usize = 12;
/// Don't ask the encoder for keyframes more often than this.
const KEYFRAME_COOLDOWN: Duration = Duration::from_millis(400);

pub async fn run(cfg: Config) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", cfg.port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}", cfg.port))?;
    info!(
        "listening on 127.0.0.1:{} - run: adb reverse tcp:{} tcp:{}",
        cfg.port, cfg.port, cfg.port
    );

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                info!("client connected from {peer}");
                match serve_client(stream, &cfg).await {
                    Ok(()) => info!("client disconnected"),
                    Err(e) => warn!("client session ended: {e:#}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
        }
    }
}

/// Decides, per frame, whether to forward or drop. After a drop it stays in
/// "dropping" mode until the next keyframe, so the client never decodes a
/// stream with a hole in it.
struct FrameSink {
    tx: mpsc::Sender<Message>,
    dropping: AtomicBool,
    last_request: Mutex<Instant>,
}

impl FrameSink {
    fn new(tx: mpsc::Sender<Message>) -> Self {
        Self {
            tx,
            // Start by waiting for an IDR so the client begins on one.
            dropping: AtomicBool::new(true),
            last_request: Mutex::new(Instant::now() - KEYFRAME_COOLDOWN),
        }
    }

    fn want_keyframe(&self) -> Feedback {
        let mut last = self.last_request.lock().unwrap();
        if last.elapsed() >= KEYFRAME_COOLDOWN {
            *last = Instant::now();
            Feedback::WantKeyframe
        } else {
            Feedback::Continue
        }
    }

    /// The client (re)started its decoder: hold frames back until an IDR, so
    /// it never starts decoding in the middle of a GOP.
    fn resync(&self) {
        self.dropping.store(true, Ordering::Relaxed);
        *self.last_request.lock().unwrap() = Instant::now() - KEYFRAME_COOLDOWN;
    }

    fn push(&self, msg: Message) -> Feedback {
        let is_key =
            matches!(&msg, Message::Video { flags, .. } if flags & kdesk_proto::FLAG_KEYFRAME != 0);
        if self.dropping.load(Ordering::Relaxed) {
            if !is_key {
                return self.want_keyframe();
            }
            self.dropping.store(false, Ordering::Relaxed);
        }
        match self.tx.try_send(msg) {
            Ok(()) => Feedback::Continue,
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("client is behind, dropping until the next keyframe");
                self.dropping.store(true, Ordering::Relaxed);
                self.want_keyframe()
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Feedback::Continue,
        }
    }
}

async fn read_message(r: &mut OwnedReadHalf) -> Result<Message> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header).await?;
    let (kind, len) = kdesk_proto::parse_header(header)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Message::decode(kind, &payload)?)
}

async fn serve_client(stream: TcpStream, cfg: &Config) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();

    // 1. Handshake.
    let hello = tokio::time::timeout(Duration::from_secs(10), read_message(&mut reader))
        .await
        .context("client didn't say hello in time")??;
    let Message::Hello {
        version,
        width,
        height,
        name,
    } = hello
    else {
        bail!("first message was not Hello");
    };
    if version != PROTOCOL_VERSION {
        bail!("client speaks protocol v{version}, this host speaks v{PROTOCOL_VERSION}");
    }
    info!("hello from '{name}' ({width}x{height})");

    // 2. Virtual screen. This is where the desktop shows its dialog.
    let mut portal = PortalSession::open(cfg.touch_offset, cfg.input).await?;
    let (sw, sh) = portal.size();
    let fd = portal
        .take_pipewire_fd()
        .context("portal gave no PipeWire fd")?;

    // 3. Capture -> channel.
    let (tx, mut rx) = mpsc::channel::<Message>(QUEUE_DEPTH);
    let sink = Arc::new(FrameSink::new(tx.clone()));
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<String>();
    let capture = Capture::start(
        fd,
        portal.node_id(),
        cfg.encoder,
        cfg.bitrate_kbps,
        {
            let sink = sink.clone();
            move |msg| sink.push(msg)
        },
        move |e| {
            let _ = err_tx.send(e);
        },
    );
    let capture = match capture {
        Ok(c) => c,
        Err(e) => {
            portal.close().await;
            return Err(e);
        }
    };

    // 4. Tell the client what's coming, then start the writer.
    writer
        .write_all(
            &Message::Config {
                version: PROTOCOL_VERSION,
                width: sw,
                height: sh,
                codec: CODEC_H264,
            }
            .encode(),
        )
        .await?;
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.write_all(&msg.encode()).await.is_err() {
                break;
            }
        }
    });

    // 5. Touch and pings in, until anything ends the session.
    let mut down: HashSet<u8> = HashSet::new();
    let result: Result<()> = loop {
        tokio::select! {
            msg = read_message(&mut reader) => {
                match msg {
                    Err(e) => break Err(e),
                    Ok(Message::Touch { action, pointer_id, x, y }) => {
                        // Ignore moves/ups for fingers we never saw go down.
                        let known = down.contains(&pointer_id);
                        let proceed = match action {
                            TouchAction::Down => { down.insert(pointer_id); true }
                            TouchAction::Move => known,
                            TouchAction::Up | TouchAction::Cancel => down.remove(&pointer_id),
                        };
                        if proceed {
                            if let Err(e) = portal.touch(action, pointer_id, x, y).await {
                                warn!("touch injection failed: {e:#}");
                            }
                        }
                    }
                    Ok(Message::Ping(t)) => {
                        let _ = tx.send(Message::Pong(t)).await;
                    }
                    Ok(Message::RequestKeyframe) => {
                        debug!("client asked for a keyframe");
                        sink.resync();
                    }
                    Ok(other) => debug!("ignoring unexpected message {other:?}"),
                }
            }
            Some(e) = err_rx.recv() => break Err(anyhow::anyhow!("capture failed: {e}")),
            _ = tokio::signal::ctrl_c() => break Ok(()),
        }
    };

    // 6. Teardown, in order: stop video, lift every finger, drop the monitor.
    drop(capture);
    for id in down {
        portal.release(id).await;
    }
    portal.close().await;
    writer_task.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(key: bool) -> Message {
        Message::Video {
            pts_us: 0,
            flags: if key { kdesk_proto::FLAG_KEYFRAME } else { 0 },
            data: vec![1],
        }
    }

    #[tokio::test]
    async fn starts_on_a_keyframe() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = FrameSink::new(tx);
        assert_eq!(sink.push(video(false)), Feedback::WantKeyframe);
        assert!(rx.try_recv().is_err(), "delta frame must not be forwarded");
        assert_eq!(sink.push(video(true)), Feedback::Continue);
        assert_eq!(sink.push(video(false)), Feedback::Continue);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn drops_until_keyframe_after_overflow() {
        let (tx, mut rx) = mpsc::channel(2);
        let sink = FrameSink::new(tx);
        sink.push(video(true));
        sink.push(video(false));
        // Queue (2) is full now.
        let fb = sink.push(video(false));
        assert!(sink.dropping.load(Ordering::Relaxed));
        assert_eq!(fb, Feedback::WantKeyframe);

        // Client catches up.
        while rx.try_recv().is_ok() {}
        // Deltas are still refused until a keyframe shows up.
        sink.push(video(false));
        assert!(rx.try_recv().is_err());
        sink.push(video(true));
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn resync_waits_for_the_next_keyframe() {
        let (tx, mut rx) = mpsc::channel(8);
        let sink = FrameSink::new(tx);
        sink.push(video(true));
        sink.push(video(false));
        while rx.try_recv().is_ok() {}

        sink.resync();
        // Deltas are held back and an IDR is requested immediately.
        assert_eq!(sink.push(video(false)), Feedback::WantKeyframe);
        assert!(rx.try_recv().is_err());
        assert_eq!(sink.push(video(true)), Feedback::Continue);
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn keyframe_requests_are_throttled() {
        let (tx, _rx) = mpsc::channel(2);
        let sink = FrameSink::new(tx);
        assert_eq!(sink.push(video(false)), Feedback::WantKeyframe);
        assert_eq!(sink.push(video(false)), Feedback::Continue);
    }
}
