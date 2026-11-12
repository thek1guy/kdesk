//! Command line options and the portal restore token on disk.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub const HELP: &str = "\
kdesk-host - use your Android device as a wired, touch-enabled second screen

USAGE:
    kdesk-host [OPTIONS]

OPTIONS:
    -p, --port <PORT>        TCP port to listen on (localhost only)  [default: 7420]
    -b, --bitrate <KBPS>     H.264 bitrate in kbit/s                 [default: 12000]
    -e, --encoder <NAME>     auto | x264 | va | nv                   [default: auto]
        --input <MODE>       eis | touch | pointer                    [default: eis]
                               eis     multitouch through the portal's input channel,
                                       placed inside the virtual screen (falls back
                                       to touch if unavailable)
                               touch   plain portal touch calls
                               pointer single-finger mouse
        --touch-offset       Add the virtual screen's position to touch coordinates
                             (try this if touches land on the wrong monitor)
        --forget             Delete the saved portal permission and exit
    -h, --help               Print this help

Connect the device with USB debugging on, then:
    adb reverse tcp:7420 tcp:7420
and open kDesk on the device.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPref {
    Auto,
    X264,
    Va,
    Nv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Eis,
    Touch,
    Pointer,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bitrate_kbps: u32,
    pub encoder: EncoderPref,
    pub input: InputMode,
    pub touch_offset: bool,
    pub forget: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: kdesk_proto::DEFAULT_PORT,
            bitrate_kbps: 12_000,
            encoder: EncoderPref::Auto,
            input: InputMode::Eis,
            touch_offset: false,
            forget: false,
        }
    }
}

/// Parse arguments (without argv[0]). `Ok(None)` means help was printed.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Option<Config>> {
    let mut cfg = Config::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        // Accept both `--port 1` and `--port=1`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = |name: &str| -> Result<String> {
            match inline.clone().or_else(|| it.next()) {
                Some(v) => Ok(v),
                None => bail!("{name} needs a value"),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "-p" | "--port" => {
                cfg.port = value("--port")?.parse().context("--port must be 1-65535")?;
            }
            "-b" | "--bitrate" => {
                cfg.bitrate_kbps = value("--bitrate")?
                    .parse()
                    .context("--bitrate must be a number in kbit/s")?;
                if cfg.bitrate_kbps == 0 {
                    bail!("--bitrate must be greater than 0");
                }
            }
            "-e" | "--encoder" => {
                cfg.encoder = match value("--encoder")?.as_str() {
                    "auto" => EncoderPref::Auto,
                    "x264" => EncoderPref::X264,
                    "va" => EncoderPref::Va,
                    "nv" => EncoderPref::Nv,
                    other => bail!("unknown encoder '{other}' (auto, x264, va, nv)"),
                };
            }
            "--input" => {
                cfg.input = match value("--input")?.as_str() {
                    "eis" => InputMode::Eis,
                    "touch" => InputMode::Touch,
                    "pointer" => InputMode::Pointer,
                    other => bail!("unknown input mode '{other}' (eis, touch, pointer)"),
                };
            }
            "--touch-offset" => cfg.touch_offset = true,
            "--forget" => cfg.forget = true,
            other => bail!("unknown option '{other}' (try --help)"),
        }
    }
    Ok(Some(cfg))
}

fn token_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("kdesk").join("restore_token"))
}

/// The portal hands out a token after the first approval so later runs can
/// skip the dialog. Missing file is not an error.
pub fn load_restore_token() -> Option<String> {
    let text = std::fs::read_to_string(token_path().ok()?).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub fn save_restore_token(token: &str) -> Result<()> {
    let path = token_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, token).with_context(|| format!("writing {}", path.display()))
}

pub fn forget_restore_token() -> Result<bool> {
    let path = token_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Config>> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let c = parse(&[]).unwrap().unwrap();
        assert_eq!(c.port, 7420);
        assert_eq!(c.bitrate_kbps, 12_000);
        assert_eq!(c.encoder, EncoderPref::Auto);
    }

    #[test]
    fn flags_both_styles() {
        let c = parse(&[
            "--port=9000",
            "-b",
            "20000",
            "-e",
            "x264",
            "--touch-offset",
            "--forget",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.bitrate_kbps, 20_000);
        assert_eq!(c.encoder, EncoderPref::X264);
        assert!(c.touch_offset);
        assert!(c.forget);
        assert_eq!(c.input, InputMode::Eis);
    }

    #[test]
    fn input_mode() {
        let c = parse(&["--input", "pointer"]).unwrap().unwrap();
        assert_eq!(c.input, InputMode::Pointer);
        let c = parse(&["--input=touch"]).unwrap().unwrap();
        assert_eq!(c.input, InputMode::Touch);
        assert!(parse(&["--input", "pen"]).is_err());
    }

    #[test]
    fn errors_and_help() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--port"]).is_err());
        assert!(parse(&["--port", "99999"]).is_err());
        assert!(parse(&["--bitrate", "0"]).is_err());
        assert!(parse(&["--encoder", "magic"]).is_err());
        assert!(parse(&["--help"]).unwrap().is_none());
    }
}
