//! PipeWire -> H.264 Annex-B access units, via GStreamer.
//!
//! ```text
//! pipewiresrc ! queue(leaky) ! videoconvert ! <encoder> ! h264parse ! appsink
//! ```
//!
//! Everything is tuned for latency, not quality-per-bit: no B-frames, tiny
//! queues that drop the oldest frame, SPS/PPS repeated before every IDR so a
//! client can join at any keyframe.

use kdesk_proto::Message;

/// What the frame consumer wants the encoder to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    Continue,
    /// The consumer dropped frames or just joined and needs an IDR.
    WantKeyframe,
}

#[cfg(feature = "gst")]
pub use imp::Capture;
#[cfg(not(feature = "gst"))]
pub use stub::Capture;

#[cfg(feature = "gst")]
mod imp {
    use super::{Feedback, Message};
    use crate::config::EncoderPref;
    use anyhow::{anyhow, bail, Context, Result};
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;
    use tracing::{debug, error, info, warn};

    pub struct Capture {
        pipeline: gst::Pipeline,
        stop: Arc<AtomicBool>,
        bus_thread: Option<JoinHandle<()>>,
        /// Kept open for the pipeline's whole life.
        _fd: Option<OwnedFd>,
    }

    #[derive(Clone, Copy)]
    enum Enc {
        Va,
        Nv,
        X264,
    }

    impl Enc {
        fn factory(self) -> &'static str {
            match self {
                Enc::Va => "vah264enc",
                Enc::Nv => "nvh264enc",
                Enc::X264 => "x264enc",
            }
        }
    }

    fn available(name: &str) -> bool {
        gst::ElementFactory::find(name).is_some()
    }

    fn pick_encoder(pref: EncoderPref) -> Result<Enc> {
        let want = match pref {
            EncoderPref::Va => Some(Enc::Va),
            EncoderPref::Nv => Some(Enc::Nv),
            EncoderPref::X264 => Some(Enc::X264),
            EncoderPref::Auto => None,
        };
        if let Some(enc) = want {
            if !available(enc.factory()) {
                bail!(
                    "encoder element '{}' isn't installed (gst-inspect-1.0 {})",
                    enc.factory(),
                    enc.factory()
                );
            }
            return Ok(enc);
        }
        for enc in [Enc::Va, Enc::Nv, Enc::X264] {
            if available(enc.factory()) {
                return Ok(enc);
            }
        }
        bail!(
            "no H.264 encoder found. Install gst-plugins-ugly (x264enc) or \
             gst-plugins-bad (vah264enc / nvh264enc)"
        )
    }

    /// Set a property from a string, but only if the element has it. GStreamer
    /// encoders differ a lot between versions and vendors.
    fn set(el: &gst::Element, name: &str, value: &str) {
        if el.find_property(name).is_some() {
            el.set_property_from_str(name, value);
        } else {
            debug!("{} has no property '{name}', skipping", el.name());
        }
    }

    fn make(factory: &str, name: &str) -> Result<gst::Element> {
        gst::ElementFactory::make(factory)
            .name(name)
            .build()
            .with_context(|| format!("GStreamer element '{factory}' is missing (install the PipeWire GStreamer plugin: gst-plugin-pipewire / gstreamer1.0-pipewire)"))
    }

    impl Capture {
        /// Start capturing PipeWire node `node_id` on the portal-provided `fd`.
        /// `on_frame` runs on a GStreamer thread for every access unit.
        pub fn start(
            fd: OwnedFd,
            node_id: u32,
            pref: EncoderPref,
            bitrate_kbps: u32,
            on_frame: impl Fn(Message) -> Feedback + Send + Sync + 'static,
            on_error: impl Fn(String) + Send + 'static,
        ) -> Result<Self> {
            gst::init().context("initialising GStreamer")?;

            let src = make("pipewiresrc", "src")?;
            set(&src, "fd", &fd.as_raw_fd().to_string());
            set(&src, "path", &node_id.to_string());
            set(&src, "do-timestamp", "true");
            // A virtual monitor only produces frames when something changes.
            // Re-send the last frame periodically so a newly joined client
            // gets a picture (and a forced keyframe can actually be produced)
            // on a still screen.
            set(&src, "keepalive-time", "100");

            Self::with_source(src, Some(fd), pref, bitrate_kbps, on_frame, on_error)
        }

        /// Same pipeline, any source element. Lets tests run without PipeWire.
        pub(super) fn with_source(
            src: gst::Element,
            fd: Option<OwnedFd>,
            pref: EncoderPref,
            bitrate_kbps: u32,
            on_frame: impl Fn(Message) -> Feedback + Send + Sync + 'static,
            on_error: impl Fn(String) + Send + 'static,
        ) -> Result<Self> {
            let enc_kind = pick_encoder(pref)?;
            info!("encoder: {} @ {} kbit/s", enc_kind.factory(), bitrate_kbps);

            let queue = make("queue", "q")?;
            set(&queue, "leaky", "downstream");
            set(&queue, "max-size-buffers", "2");
            set(&queue, "max-size-bytes", "0");
            set(&queue, "max-size-time", "0");

            let convert = make("videoconvert", "convert")?;
            let enc = make(enc_kind.factory(), "enc")?;
            let kbps = bitrate_kbps.to_string();
            set(&enc, "bitrate", &kbps);
            match enc_kind {
                Enc::X264 => {
                    set(&enc, "tune", "zerolatency");
                    set(&enc, "speed-preset", "ultrafast");
                    set(&enc, "bframes", "0");
                    set(&enc, "key-int-max", "120");
                    set(&enc, "byte-stream", "true");
                }
                Enc::Va => {
                    set(&enc, "b-frames", "0");
                    set(&enc, "key-int-max", "120");
                    set(&enc, "rate-control", "cbr");
                }
                Enc::Nv => {
                    set(&enc, "zerolatency", "true");
                    set(&enc, "bframes", "0");
                    set(&enc, "gop-size", "120");
                    set(&enc, "rc-mode", "cbr");
                }
            }

            let parse = make("h264parse", "parse")?;
            // -1: put SPS/PPS before every IDR.
            set(&parse, "config-interval", "-1");

            let sink_el = make("appsink", "sink")?;
            let sink = sink_el
                .clone()
                .downcast::<gst_app::AppSink>()
                .map_err(|_| anyhow!("appsink element has the wrong type"))?;
            sink.set_caps(Some(
                &gst::Caps::builder("video/x-h264")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            ));
            sink.set_sync(false);
            sink.set_max_buffers(2);
            sink.set_drop(true);

            let pipeline = gst::Pipeline::with_name("kdesk");
            pipeline.add_many([&src, &queue, &convert, &enc, &parse, &sink_el])?;
            gst::Element::link_many([&src, &queue, &convert, &enc, &parse, &sink_el])
                .context("linking the capture pipeline")?;

            let key_target = sink.downgrade();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                        let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                        let mut flags = 0u8;
                        if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                            flags |= kdesk_proto::FLAG_KEYFRAME;
                        }
                        if buffer.flags().contains(gst::BufferFlags::HEADER) {
                            flags |= kdesk_proto::FLAG_CONFIG;
                        }
                        let pts_us = buffer.pts().map(|t| t.useconds()).unwrap_or(0);

                        let msg = Message::Video {
                            pts_us,
                            flags,
                            data: map.as_slice().to_vec(),
                        };
                        if on_frame(msg) == Feedback::WantKeyframe {
                            if let Some(sink) = key_target.upgrade() {
                                request_keyframe(sink.upcast_ref());
                            }
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

            let bus = pipeline.bus().context("pipeline has no bus")?;
            let stop = Arc::new(AtomicBool::new(false));
            let stop_bus = stop.clone();
            let bus_thread =
                std::thread::Builder::new()
                    .name("kdesk-bus".into())
                    .spawn(move || {
                        while !stop_bus.load(Ordering::Relaxed) {
                            let Some(msg) = bus.timed_pop_filtered(
                                gst::ClockTime::from_mseconds(250),
                                &[gst::MessageType::Error, gst::MessageType::Eos],
                            ) else {
                                continue;
                            };
                            match msg.view() {
                                gst::MessageView::Error(e) => {
                                    let who = e
                                        .src()
                                        .map(|s| s.path_string().to_string())
                                        .unwrap_or_default();
                                    error!("pipeline error from {who}: {}", e.error());
                                    on_error(format!("{}: {}", who, e.error()));
                                }
                                gst::MessageView::Eos(_) => {
                                    warn!("capture stream ended");
                                    on_error("the virtual screen stream ended".into());
                                }
                                _ => {}
                            }
                            break;
                        }
                    })?;

            pipeline
                .set_state(gst::State::Playing)
                .context("starting the capture pipeline")?;

            Ok(Self {
                pipeline,
                stop,
                bus_thread: Some(bus_thread),
                _fd: fd,
            })
        }
    }

    /// Ask the encoder (via the sink, upstream) for an IDR with fresh SPS/PPS.
    fn request_keyframe(sink: &gst::Element) {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if !sink.send_event(ev) {
            debug!("keyframe request was not handled");
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = self.pipeline.set_state(gst::State::Null);
            if let Some(t) = self.bus_thread.take() {
                let _ = t.join();
            }
            // Give PipeWire a moment to let go before the fd closes.
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Mutex;

        /// Runs the real encode chain on a test pattern and checks what a
        /// client would receive: Annex-B, SPS/PPS before the IDR, and a fresh
        /// IDR on request.
        #[test]
        fn encodes_annexb_and_honours_keyframe_requests() {
            gst::init().unwrap();
            if !available("x264enc") {
                eprintln!("x264enc not installed, skipping");
                return;
            }
            let src = gst::ElementFactory::make("videotestsrc")
                .property_from_str("is-live", "true")
                .property_from_str("pattern", "ball")
                .build()
                .unwrap();

            type Frames = Vec<(u8, Vec<u8>)>;
            let frames: Arc<Mutex<Frames>> = Arc::default();
            let seen = frames.clone();
            let asked = Arc::new(AtomicBool::new(false));
            let asked_cb = asked.clone();
            let cap = Capture::with_source(
                src,
                None,
                EncoderPref::X264,
                4000,
                move |msg| {
                    let Message::Video { flags, data, .. } = msg else {
                        return Feedback::Continue;
                    };
                    let mut v = seen.lock().unwrap();
                    v.push((flags, data));
                    // After 20 frames, ask once for a keyframe.
                    if v.len() == 20 && !asked_cb.swap(true, Ordering::Relaxed) {
                        return Feedback::WantKeyframe;
                    }
                    Feedback::Continue
                },
                |e| panic!("pipeline error: {e}"),
            )
            .unwrap();

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let n = frames.lock().unwrap().len();
                if n >= 60 || std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(cap);

            let frames = frames.lock().unwrap();
            assert!(frames.len() >= 30, "only {} frames arrived", frames.len());

            let (first_flags, first) = &frames[0];
            assert_ne!(
                first_flags & kdesk_proto::FLAG_KEYFRAME,
                0,
                "stream must open on an IDR"
            );
            assert_eq!(&first[..4], &[0, 0, 0, 1], "Annex-B start code");
            // SPS (nal type 7) must come before the IDR (type 5) in-band.
            let nal_types: Vec<u8> = first
                .windows(5)
                .filter(|w| w[..4] == [0, 0, 0, 1])
                .map(|w| w[4] & 0x1f)
                .collect();
            let sps = nal_types
                .iter()
                .position(|&t| t == 7)
                .expect("SPS in first AU");
            let idr = nal_types
                .iter()
                .position(|&t| t == 5)
                .expect("IDR in first AU");
            assert!(sps < idr);

            let keys = frames
                .iter()
                .filter(|(f, _)| f & kdesk_proto::FLAG_KEYFRAME != 0)
                .count();
            assert!(
                keys >= 2,
                "expected a second IDR after the request, got {keys}"
            );
        }
    }
}

#[cfg(not(feature = "gst"))]
mod stub {
    use super::{Feedback, Message};
    use crate::config::EncoderPref;
    use anyhow::{bail, Result};
    use std::os::fd::OwnedFd;

    pub struct Capture;

    impl Capture {
        pub fn start(
            _fd: OwnedFd,
            _node_id: u32,
            _pref: EncoderPref,
            _bitrate_kbps: u32,
            _on_frame: impl Fn(Message) -> Feedback + Send + Sync + 'static,
            _on_error: impl Fn(String) + Send + 'static,
        ) -> Result<Self> {
            bail!("kdesk-host was built without the `gst` feature, so it cannot encode video")
        }
    }
}
