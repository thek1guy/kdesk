# kDesk

Use an Android device as a **wired, touch-enabled second screen for Linux**.
Think Deskreen (but over a cable, not Wi-Fi) plus spacedesk (but with a Linux
host and real touch input).

```
 Linux host (Rust)                                  Android client (Kotlin)
 ┌──────────────────────────────────┐   USB / adb   ┌───────────────────────┐
 │ xdg portal: virtual monitor      │   reverse     │ MediaCodec -> Surface │
 │ PipeWire -> GStreamer -> H.264 ──┼───────────────┼─> decode + draw       │
 │ RemoteDesktop portal  <── touch ─┼───────────────┼── touch events        │
 └──────────────────────────────────┘   TCP :7420   └───────────────────────┘
```

## Status

| Part | State |
|------|-------|
| `kdesk-proto` wire protocol | done, unit tested |
| `kdesk-host` encode pipeline (GStreamer, x264/VA/NVENC) | done, tested on a test pattern: opens on an IDR with in-band SPS/PPS, honours keyframe requests |
| `kdesk-host` backpressure (drop until next keyframe) | done, unit tested |
| `kdesk-host` portal session + touch injection | written against ashpd 0.14, **not yet run on a real KDE session** |
| Android client (`android/`, Kotlin, package `org.k1.kdesk`) | written, **not compiled yet** (no Android SDK where I built it); open in Android Studio and fix whatever it flags |

## How it works

1. The phone connects to the host. The host listens on `127.0.0.1:7420` only;
   `adb reverse` tunnels the phone's `localhost:7420` to it over USB.
2. The host asks the desktop portal for a **virtual screen**. On KDE Plasma
   this is the "Share virtual screen" option in the screen sharing dialog.
   Tick *Allow the application to do this without asking next time* and the
   permission is stored, so later runs skip the dialog.
3. KWin creates the monitor, the host reads it from PipeWire and encodes
   low-latency H.264 (no B-frames, SPS/PPS before every IDR).
4. Touches from the phone are sent to the RemoteDesktop portal in the
   virtual screen's coordinate space, so they land on that monitor.
5. If the phone can't keep up, the host drops frames until the next keyframe
   instead of building up lag.

The protocol is documented at the top of
[`crates/kdesk-proto/src/lib.rs`](crates/kdesk-proto/src/lib.rs).

## Android app

Open the `android/` folder in Android Studio and let it sync (it may offer to
bump the Gradle/AGP versions, which is fine). No AndroidX or Compose, plain
framework Views only. Run it on a device or in Waydroid.

- `Protocol.kt` mirrors the Rust protocol; `ProtocolTest.kt` asserts the same bytes the Rust tests do.
- `HostConnection.kt` dials `127.0.0.1:7420` and reconnects until stopped.
- `VideoDecoder.kt` is MediaCodec into the SurfaceView. It starts on a keyframe and, when it falls behind, drops everything and asks the host for a new one.
- `MainActivity.kt` is the full-screen letterboxed video; multitouch is normalised over the video rectangle.

## Build

You need Rust, plus the GStreamer development packages and plugins.

Debian / Ubuntu:

```sh
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
  gstreamer1.0-pipewire
```

Arch:

```sh
sudo pacman -S gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
  gst-plugins-ugly gst-plugin-pipewire
```

Fedora (x264 comes from RPM Fusion):

```sh
sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel \
  gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-ugly \
  gstreamer1-plugin-pipewire
```

Your desktop also needs a portal backend that can make virtual monitors:
`xdg-desktop-portal-kde` on Plasma 6 (the GNOME portal works too in recent
versions).

```sh
cargo build --release
./target/release/kdesk-host --help
```

`cargo build --no-default-features -p kdesk-host` builds without GStreamer
(everything except video encoding), which is handy for working on the rest.

## Run

1. On the Android device: enable **Developer options -> USB debugging**.
2. Plug it in and check `adb devices` shows it as `device`.
3. On the PC:

   ```sh
   adb reverse tcp:7420 tcp:7420
   ./target/release/kdesk-host
   ```

4. Open kDesk on the device. The desktop asks you to approve the virtual
   screen the first time; after that it appears and disappears with the
   connection.

Options:

| Flag | Meaning |
|------|---------|
| `-p, --port` | Port to listen on (default 7420, change the `adb reverse` to match) |
| `-b, --bitrate` | H.264 kbit/s (default 12000; USB has plenty of room, raise it for sharper text) |
| `-e, --encoder` | `auto`, `x264`, `va` (VA-API), `nv` (NVENC). `auto` prefers hardware |
| `--forget` | Delete the saved portal permission |

Logging goes through `RUST_LOG` (e.g. `RUST_LOG=debug`).

## Testing with Waydroid

Waydroid is a fine way to iterate without a phone:

1. `waydroid session start`, then `waydroid status` for the container IP.
2. In Waydroid's settings enable Developer options and USB/ADB debugging.
3. `adb connect <waydroid-ip>:5555`, then `adb reverse tcp:7420 tcp:7420`.
4. Run `kdesk-host` and open the app inside Waydroid.

Clicking in the Waydroid window produces touch events, so this exercises the
whole touch path. It can't tell you anything about real USB latency, so plug in
a phone for that.

## Security note

The host only listens on `127.0.0.1`, but anything running as any user on your
machine can connect to that port and inject touch input into the virtual
screen while a session is open. If that matters to you, a per-run token in the
Hello message is the next thing to add.

## Layout

```
crates/kdesk-proto   wire protocol (shared types, encode/decode, tests)
crates/kdesk-host    portal.rs   virtual screen + touch via ashpd
                     capture.rs  PipeWire -> H.264 via GStreamer
                     server.rs   TCP server, backpressure, session lifecycle
                     config.rs   flags, saved portal permission
```
