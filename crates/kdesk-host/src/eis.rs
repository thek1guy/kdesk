//! Touch injection through the portal's EIS (libei) channel.
//!
//! The simple `NotifyTouch*` portal calls leave it to the compositor to work
//! out which monitor a touch belongs to, and on KDE that picks the wrong one
//! for a virtual screen. EIS instead hands us a touchscreen device with
//! *regions* (rectangles in global logical coordinates), each of which can
//! carry a `mapping_id` that matches a screencast stream. We pick the region
//! that belongs to our virtual screen and place every touch inside it
//! ourselves, so there is nothing left to guess.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reis::{
    ei,
    enumflags2::BitFlags,
    event::{DeviceCapability, EiEvent},
};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// What we know about the virtual screen, used to pick the right region.
#[derive(Debug, Clone)]
pub struct StreamHint {
    /// The portal stream's `mapping_id`, if it reported one.
    pub mapping_id: Option<String>,
    /// The portal's idea of the stream's global position and size.
    pub position: Option<(i32, i32)>,
    pub size: (u32, u32),
}

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Down { id: u32, nx: f32, ny: f32 },
    Motion { id: u32, nx: f32, ny: f32 },
    Up { id: u32 },
    Cancel { id: u32 },
}

pub struct EisTouch {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl EisTouch {
    /// Connect, wait for a touchscreen device and pick its region for the
    /// virtual screen. Fails (so the caller can fall back) if there is no
    /// touch device within a few seconds.
    pub async fn connect(fd: OwnedFd, hint: StreamHint) -> Result<Self> {
        let (ready_tx, ready_rx) = oneshot::channel::<Result<String, String>>();
        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();

        // reis' event types are not `Send`, so the whole EIS session lives on
        // its own thread with a single-threaded runtime.
        std::thread::Builder::new()
            .name("kdesk-eis".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("couldn't start the EIS runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(run(fd, hint, ready_tx, rx));
            })
            .context("spawning the EIS thread")?;

        match ready_rx.await {
            Ok(Ok(_)) => Ok(Self { tx }),
            Ok(Err(why)) => bail!("{why}"),
            Err(_) => bail!("the EIS thread ended before a touch device was ready"),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

async fn run(
    fd: OwnedFd,
    hint: StreamHint,
    ready_tx: oneshot::Sender<Result<String, String>>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let stream = UnixStream::from(fd);
    let context = match ei::Context::new(stream) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("couldn't wrap the EIS socket: {e}")));
            return;
        }
    };
    let (connection, mut events) = match context
        .handshake_tokio("kdesk", ei::handshake::ContextType::Sender)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("EIS handshake failed: {e}")));
            return;
        }
    };
    info!("EIS: connected to the compositor's input channel");

    let mut ready_tx = Some(ready_tx);
    let started = Instant::now();
    // Set once a usable touch device is resumed.
    let mut active: Option<Active> = None;
    let mut sequence = 0u32;

    loop {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else {
                    if let Some(t) = ready_tx.take() {
                        let _ = t.send(Err("the compositor closed the input channel".into()));
                    }
                    warn!("EIS: channel closed");
                    return;
                };
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => {
                        warn!("EIS: protocol error: {e}");
                        if let Some(t) = ready_tx.take() {
                            let _ = t.send(Err(format!("EIS protocol error: {e}")));
                        }
                        return;
                    }
                };
                match ev {
                    EiEvent::SeatAdded(e) => {
                        e.seat.bind_capabilities(BitFlags::from(DeviceCapability::Touch));
                        let _ = connection.flush();
                    }
                    EiEvent::DeviceAdded(e) => {
                        let d = &e.device;
                        info!(
                            "EIS: device {:?} ({} region(s))",
                            d.name(),
                            d.regions().len()
                        );
                        for r in d.regions() {
                            info!(
                                "EIS:   region x={} y={} {}x{} scale={} mapping_id={:?}",
                                r.x, r.y, r.width, r.height, r.scale, r.mapping_id
                            );
                        }
                    }
                    EiEvent::DeviceResumed(e) => {
                        if active.is_some() {
                            continue;
                        }
                        let d = e.device.clone();
                        let Some(touch) = d.interface::<ei::Touchscreen>() else {
                            continue;
                        };
                        match pick_region(d.regions(), &hint) {
                            Some((idx, why)) => {
                                let r = &d.regions()[idx];
                                let region = Region {
                                    x: r.x as f32,
                                    y: r.y as f32,
                                    w: r.width as f32,
                                    h: r.height as f32,
                                };
                                let msg = format!(
                                    "using region {idx} ({}x{} at {},{}) because {why}",
                                    r.width, r.height, r.x, r.y
                                );
                                info!("EIS: {msg}");
                                sequence += 1;
                                d.device().start_emulating(connection.serial(), sequence);
                                let _ = connection.flush();
                                active = Some(Active { device: d, touch, region });
                                if let Some(t) = ready_tx.take() {
                                    let _ = t.send(Ok(msg));
                                }
                            }
                            None => {
                                if let Some(t) = ready_tx.take() {
                                    let _ = t.send(Err(
                                        "found a touchscreen but no region that matches the virtual screen"
                                            .into(),
                                    ));
                                }
                            }
                        }
                    }
                    EiEvent::DevicePaused(e) => {
                        if active.as_ref().is_some_and(|a| a.device == e.device) {
                            warn!("EIS: touch device paused");
                            active = None;
                        }
                    }
                    EiEvent::Disconnected(_) => {
                        warn!("EIS: disconnected");
                        if let Some(t) = ready_tx.take() {
                            let _ = t.send(Err("disconnected from the input channel".into()));
                        }
                        return;
                    }
                    _ => {}
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return };
                if let Some(a) = &active {
                    let now = started.elapsed().as_micros() as u64;
                    a.apply(cmd, now, connection.serial());
                    let _ = connection.flush();
                } else {
                    debug!("EIS: dropping {cmd:?}, no active touch device");
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(5)), if ready_tx.is_some() => {
                if let Some(t) = ready_tx.take() {
                    let _ = t.send(Err("no touchscreen device appeared in 5 seconds".into()));
                }
            }
        }
    }
}

struct Region {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Active {
    device: reis::event::Device,
    touch: ei::Touchscreen,
    region: Region,
}

impl Active {
    fn apply(&self, cmd: Cmd, now_us: u64, serial: u32) {
        let place = |nx: f32, ny: f32| {
            (
                self.region.x + nx.clamp(0.0, 1.0) * (self.region.w - 1.0).max(0.0),
                self.region.y + ny.clamp(0.0, 1.0) * (self.region.h - 1.0).max(0.0),
            )
        };
        match cmd {
            Cmd::Down { id, nx, ny } => {
                let (x, y) = place(nx, ny);
                debug!("EIS: touch down {id} -> ({x:.0}, {y:.0})");
                self.touch.down(id, x, y);
            }
            Cmd::Motion { id, nx, ny } => {
                let (x, y) = place(nx, ny);
                self.touch.motion(id, x, y);
            }
            Cmd::Up { id } => self.touch.up(id),
            Cmd::Cancel { id } => self.touch.cancel(id),
        }
        self.device.device().frame(serial, now_us);
    }
}

/// Choose the region that belongs to the virtual screen. Returns the index
/// and a short human-readable reason.
fn pick_region(regions: &[reis::event::Region], hint: &StreamHint) -> Option<(usize, String)> {
    if regions.is_empty() {
        return None;
    }
    // 1. The mapping_id the portal gave the stream: this is what it is for.
    if let Some(id) = &hint.mapping_id {
        if let Some(i) = regions
            .iter()
            .position(|r| r.mapping_id.as_deref() == Some(id))
        {
            return Some((i, format!("its mapping_id matches the stream ({id})")));
        }
    }
    // 2. Same global position as the stream.
    if let Some((px, py)) = hint.position {
        if let Some(i) = regions
            .iter()
            .position(|r| r.x as i32 == px && r.y as i32 == py)
        {
            return Some((i, format!("it sits at the stream's position ({px},{py})")));
        }
    }
    // 3. Only one region claims the stream's size and isn't at the origin
    //    (the origin is normally the main monitor).
    let (w, h) = hint.size;
    let by_size: Vec<usize> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| r.width == w && r.height == h && (r.x != 0 || r.y != 0))
        .map(|(i, _)| i)
        .collect();
    if by_size.len() == 1 {
        return Some((
            by_size[0],
            format!("it's the only off-origin region of size {w}x{h}"),
        ));
    }
    // 4. A single region is all there is; the compositor already scoped it.
    if regions.len() == 1 {
        return Some((0, "it's the only region".into()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reis::event::Region as R;

    fn region(x: u32, y: u32, w: u32, h: u32, id: Option<&str>) -> R {
        R {
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            mapping_id: id.map(str::to_owned),
        }
    }

    fn hint(id: Option<&str>, pos: Option<(i32, i32)>) -> StreamHint {
        StreamHint {
            mapping_id: id.map(str::to_owned),
            position: pos,
            size: (1920, 1080),
        }
    }

    #[test]
    fn mapping_id_wins() {
        let regions = [
            region(0, 0, 1920, 1080, Some("main")),
            region(1920, 0, 1920, 1080, Some("virt")),
        ];
        let got = pick_region(&regions, &hint(Some("virt"), Some((0, 0)))).unwrap();
        assert_eq!(got.0, 1);
    }

    #[test]
    fn falls_back_to_position() {
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 1920, 1080, None),
        ];
        let got = pick_region(&regions, &hint(None, Some((1920, 0)))).unwrap();
        assert_eq!(got.0, 1);
    }

    #[test]
    fn falls_back_to_unique_off_origin_size() {
        let regions = [
            region(0, 0, 2560, 1440, None),
            region(2560, 0, 1920, 1080, None),
        ];
        let got = pick_region(&regions, &hint(None, None)).unwrap();
        assert_eq!(got.0, 1);
    }

    #[test]
    fn refuses_to_guess_between_look_alikes() {
        // Two monitors the same size as the stream, nothing else to go on:
        // better to fall back than to touch the wrong one.
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 1920, 1080, None),
            region(3840, 0, 1920, 1080, None),
        ];
        assert!(pick_region(&regions, &hint(None, None)).is_none());
    }

    #[test]
    fn single_region_is_used() {
        let regions = [region(0, 0, 1920, 1080, None)];
        assert_eq!(pick_region(&regions, &hint(None, None)).unwrap().0, 0);
    }

    #[test]
    fn no_regions_means_no_pick() {
        assert!(pick_region(&[], &hint(Some("x"), Some((0, 0)))).is_none());
    }
}
