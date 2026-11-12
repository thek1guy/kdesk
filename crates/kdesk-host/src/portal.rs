//! xdg-desktop-portal session: a virtual monitor to extend onto, plus touch
//! injection into it.
//!
//! One RemoteDesktop session carries both halves, so the compositor shows a
//! single dialog (on KDE: the "Share virtual screen" picker) and touch
//! coordinates are relative to that stream, which means they land on the
//! virtual monitor with no extra mapping.

use anyhow::{bail, Context, Result};
use ashpd::desktop::{
    remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode, Session,
};
use kdesk_proto::TouchAction;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{debug, info, warn};

use crate::config::{self, InputMode};
use crate::eis::{Cmd, EisTouch, StreamHint};

pub struct PortalSession {
    remote: RemoteDesktop,
    session: Session<RemoteDesktop>,
    node_id: u32,
    width: u32,
    height: u32,
    /// Where the portal says the virtual screen sits in the global layout.
    /// Only used when `touch_offset` is on.
    position: (i32, i32),
    touch_offset: bool,
    input: InputMode,
    /// Set when EIS is up; touches then bypass the plain portal calls.
    eis: Option<EisTouch>,
    /// Pointer mode: the finger currently acting as the mouse (-1 = none).
    primary: AtomicI32,
    touch_enabled: bool,
    /// Held so the PipeWire connection outlives the capture pipeline.
    pipewire_fd: Option<OwnedFd>,
}

impl PortalSession {
    /// Ask the compositor for a virtual monitor (and touch access to it).
    /// May block on a user dialog the first time.
    pub async fn open(touch_offset: bool, input: InputMode) -> Result<Self> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        info!(
            "desktop: {}",
            if desktop.is_empty() {
                "unknown"
            } else {
                &desktop
            }
        );

        let remote = RemoteDesktop::new()
            .await
            .context("connecting to the RemoteDesktop portal (is xdg-desktop-portal running?)")?;
        let screencast = Screencast::new()
            .await
            .context("connecting to the ScreenCast portal")?;

        let sources = screencast.available_source_types().await?;
        if !sources.contains(SourceType::VIRTUAL) {
            bail!(
                "this desktop's portal can't create virtual monitors (source types: {sources:?}). \
                 KDE Plasma 6 and recent GNOME can."
            );
        }

        let session = remote.create_session(Default::default()).await?;

        let token = config::load_restore_token();
        if token.is_some() {
            info!("found a saved portal permission, the dialog may be skipped");
        }
        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(match input {
                        InputMode::Eis | InputMode::Touch => DeviceType::TOUCHSCREEN,
                        InputMode::Pointer => DeviceType::POINTER,
                    })
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?
            .response()?;
        screencast
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::EMBEDDED)
                    .set_sources(SourceType::VIRTUAL)
                    .set_multiple(false),
            )
            .await?
            .response()?;

        info!("waiting for the desktop to confirm the virtual screen...");
        let started = remote
            .start(&session, None, StartOptions::default())
            .await?
            .response()
            .context("the virtual screen request was cancelled or denied")?;

        if let Some(new_token) = started.restore_token() {
            if let Err(e) = config::save_restore_token(new_token) {
                warn!("couldn't save the portal permission: {e:#}");
            }
        }

        let wanted = match input {
            InputMode::Eis | InputMode::Touch => DeviceType::TOUCHSCREEN,
            InputMode::Pointer => DeviceType::POINTER,
        };
        let touch_enabled = started.devices().contains(wanted);
        if !touch_enabled {
            warn!("{input:?} access was not granted, video only");
        }
        info!("input mode: {input:?}");

        let stream = started
            .streams()
            .first()
            .context("the portal started but returned no stream")?;
        let node_id = stream.pipe_wire_node_id();
        let (width, height) = match stream.size() {
            Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
            _ => {
                warn!("portal didn't report a stream size, assuming 1920x1080");
                (1920, 1080)
            }
        };
        let position = stream.position().unwrap_or((0, 0));
        info!("virtual screen is up: node {node_id}, {width}x{height}");
        info!(
            "portal says: position={:?} size={:?} source_type={:?} id={:?} mapping_id={:?}",
            stream.position(),
            stream.size(),
            stream.source_type(),
            stream.id(),
            stream.mapping_id()
        );
        if touch_offset {
            info!("touch offset is on: adding {position:?} to touch coordinates");
        }

        // Best touch route first: EIS, placed inside the virtual screen.
        let mut input = input;
        let mut eis = None;
        if input == InputMode::Eis && touch_enabled {
            let hint = StreamHint {
                mapping_id: stream.mapping_id().map(str::to_owned),
                position: stream.position(),
                size: (width, height),
            };
            let connected = match remote.connect_to_eis(&session, Default::default()).await {
                Ok(fd) => EisTouch::connect(fd, hint).await,
                Err(e) => Err(e.into()),
            };
            match connected {
                Ok(t) => eis = Some(t),
                Err(e) => {
                    warn!("EIS touch unavailable ({e:#}), using plain portal touch instead");
                    input = InputMode::Touch;
                }
            }
        } else if input == InputMode::Eis {
            input = InputMode::Touch;
        }

        let pipewire_fd = screencast
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .context("opening the PipeWire remote")?;

        Ok(Self {
            remote,
            session,
            node_id,
            width,
            height,
            position,
            touch_offset,
            input,
            eis,
            primary: AtomicI32::new(-1),
            touch_enabled,
            pipewire_fd: Some(pipewire_fd),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Hand the PipeWire fd to the capture pipeline. Keep the returned value
    /// alive as long as the pipeline runs.
    pub fn take_pipewire_fd(&mut self) -> Option<OwnedFd> {
        self.pipewire_fd.take()
    }

    /// Inject one touch event. `x`/`y` are normalised 0..=65535 over the
    /// stream; `pointer_id` is used as the touch slot.
    pub async fn touch(&self, action: TouchAction, pointer_id: u8, x: u16, y: u16) -> Result<()> {
        if !self.touch_enabled {
            return Ok(());
        }
        let slot = pointer_id as u32;
        let mut px = x as f64 / 65535.0 * self.width as f64;
        let mut py = y as f64 / 65535.0 * self.height as f64;
        if self.touch_offset {
            px += self.position.0 as f64;
            py += self.position.1 as f64;
        }
        if let Some(eis) = &self.eis {
            let (nx, ny) = (x as f32 / 65535.0, y as f32 / 65535.0);
            eis.send(match action {
                TouchAction::Down => Cmd::Down { id: slot, nx, ny },
                TouchAction::Move => Cmd::Motion { id: slot, nx, ny },
                TouchAction::Up => Cmd::Up { id: slot },
                TouchAction::Cancel => Cmd::Cancel { id: slot },
            });
            return Ok(());
        }
        if action == TouchAction::Down {
            debug!("touch down slot {slot} -> ({px:.0}, {py:.0})");
        }
        if self.input == InputMode::Pointer {
            return self.pointer(action, pointer_id, px, py).await;
        }
        match action {
            TouchAction::Down => {
                self.remote
                    .notify_touch_down(
                        &self.session,
                        self.node_id,
                        slot,
                        px,
                        py,
                        Default::default(),
                    )
                    .await?
            }
            TouchAction::Move => {
                self.remote
                    .notify_touch_motion(
                        &self.session,
                        self.node_id,
                        slot,
                        px,
                        py,
                        Default::default(),
                    )
                    .await?
            }
            TouchAction::Up | TouchAction::Cancel => {
                self.remote
                    .notify_touch_up(&self.session, slot, Default::default())
                    .await?
            }
        }
        Ok(())
    }

    /// Single-finger mouse emulation: the first finger down is the mouse,
    /// extra fingers are ignored until it lifts.
    async fn pointer(&self, action: TouchAction, pointer_id: u8, px: f64, py: f64) -> Result<()> {
        const BTN_LEFT: i32 = 0x110;
        let id = pointer_id as i32;
        match action {
            TouchAction::Down => {
                if self
                    .primary
                    .compare_exchange(-1, id, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Ok(());
                }
                self.remote
                    .notify_pointer_motion_absolute(
                        &self.session,
                        self.node_id,
                        px,
                        py,
                        Default::default(),
                    )
                    .await?;
                self.remote
                    .notify_pointer_button(
                        &self.session,
                        BTN_LEFT,
                        KeyState::Pressed,
                        Default::default(),
                    )
                    .await?;
            }
            TouchAction::Move => {
                if self.primary.load(Ordering::Acquire) == id {
                    self.remote
                        .notify_pointer_motion_absolute(
                            &self.session,
                            self.node_id,
                            px,
                            py,
                            Default::default(),
                        )
                        .await?;
                }
            }
            TouchAction::Up | TouchAction::Cancel => {
                if self
                    .primary
                    .compare_exchange(id, -1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.remote
                        .notify_pointer_button(
                            &self.session,
                            BTN_LEFT,
                            KeyState::Released,
                            Default::default(),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Release a slot without caring whether it was down (cleanup path).
    pub async fn release(&self, pointer_id: u8) {
        if !self.touch_enabled {
            return;
        }
        if let Some(eis) = &self.eis {
            eis.send(Cmd::Up {
                id: pointer_id as u32,
            });
        } else if self.input == InputMode::Pointer {
            let _ = self.pointer(TouchAction::Up, pointer_id, 0.0, 0.0).await;
        } else {
            let _ = self
                .remote
                .notify_touch_up(&self.session, pointer_id as u32, Default::default())
                .await;
        }
    }

    /// End the session; the compositor removes the virtual monitor.
    pub async fn close(self) {
        if let Err(e) = self.session.close().await {
            warn!("closing the portal session: {e}");
        }
    }
}
