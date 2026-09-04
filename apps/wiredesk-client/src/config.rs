use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use wiredesk_core::BluetoothConfig;
use wiredesk_transport::bluetooth::BluetoothFactoryConfig;
use wiredesk_transport::{SerialFactoryConfig, TransportConfig};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ClientConfig {
    pub port: String,
    pub baud: u32,
    pub width: u16,
    pub height: u16,
    pub client_name: String,
    /// Persisted identity of the preferred fullscreen target —
    /// `monitor_identity(m)` format ("Studio Display (5120×2880 @ 0,0)").
    /// Combines `NSScreen.localizedName` with the screen size **and** the
    /// global-coordinate origin so two physically identical displays (same
    /// name, same resolution — e.g. a dual Studio Display 5K setup) round-trip
    /// distinctly. macOS doesn't allow two NSScreen frames to overlap, so
    /// origin is always unique per connected display. `None` → use the
    /// display the window currently sits on. `#[serde(default)]` on the
    /// struct makes a missing field deserialize as `None`, so existing
    /// configs round-trip safely without migration.
    ///
    /// Stored as an identity string (not an `NSScreen::screens()` index)
    /// because ordinals aren't stable across reboot / dock event / hot-plug
    /// — a saved index stays in-range but silently points at a different
    /// physical display. Identity-based resolution survives reboots and
    /// re-orderings; if the user renames the display in System Settings,
    /// changes its resolution, or moves it to a different physical position,
    /// the saved preference falls back to "active monitor" until they
    /// re-pick.
    ///
    /// **Backward compatibility:** uses a custom deserializer
    /// (`deserialize_preferred_monitor`) that accepts either a string
    /// (current format, including legacy label-only "Studio Display
    /// (5120×2880)" values that simply fail to resolve and fall back to
    /// "active monitor" — user re-picks once) or a TOML integer (very-early
    /// format from before the type was `String` — silently discarded as
    /// `None`). Without this, upgrading an old config would fail to parse
    /// and `load_from` would wipe ALL fields (port/baud/size/name) back to
    /// defaults — losing the user's settings on first run after upgrade.
    #[serde(deserialize_with = "deserialize_preferred_monitor")]
    pub preferred_monitor: Option<String>,
    /// Send images from Mac → Host. When false, the poll thread skips
    /// `get_image()` entirely (text continues to sync as before). Useful to
    /// isolate one direction during diagnostics or when image sync misbehaves.
    #[serde(default = "default_true")]
    pub send_images: bool,
    /// Accept incoming images from Host → Mac. When false, incoming
    /// `ClipOffer{format=PNG_IMAGE}` is rejected on receipt (state stays clean).
    #[serde(default = "default_true")]
    pub receive_images: bool,
    /// Send text from Mac → Host. Useful to disable when an app like
    /// Whispr Flow / dictation tools writes transcribed text into the
    /// macOS clipboard on every utterance — without this toggle every
    /// dictation message turns into a clipboard sync.
    #[serde(default = "default_true")]
    pub send_text: bool,
    /// Accept incoming text from Host → Mac.
    #[serde(default = "default_true")]
    pub receive_text: bool,
    /// Accept incoming files from Host → Mac. When false, incoming
    /// `ClipOffer{format=FILE}` is rejected on receipt — the host sees a
    /// `ClipDecline` and `IncomingClipboard` stays idle (no reassembly
    /// state). Wired through to `IncomingClipboard.receive_files` at startup
    /// (Task 8 follow-up from Task 7a). Backwards-compatible default `true`:
    /// pre-existing config files without the field deserialize to "on", so
    /// users don't lose the feature on upgrade.
    #[serde(default = "default_true")]
    pub receive_files: bool,
    /// Send files from Mac → Host. **Opt-in: default `false`.** Unlike text,
    /// a file copy can be large and is rarely meant to leave the Mac, so the
    /// user enables this explicitly per session. When false the poll thread
    /// skips the file-URL probe entirely (a plain Cmd+C on a file never
    /// touches the wire). Missing field in an older TOML → `false` (the safe
    /// default), so upgrading never starts leaking files without consent.
    #[serde(default)]
    pub send_files: bool,
    /// Compensate Karabiner-Elements `left_command ↔ left_option` swap when
    /// forwarding modifiers and detecting local hotkeys. With Karabiner
    /// remapping the two keys at the HID level (so the same physical
    /// keyboard works identically on macOS and Windows), our CGEventTap
    /// sees Cmd where the user pressed Option and vice versa — Cmd+V then
    /// arrives on Host as Alt+V instead of the paste combo. Enabling this
    /// re-swaps modifiers locally so Host gets the user-intended scancodes.
    #[serde(default)]
    pub swap_option_command: bool,

    /// Persisted geometry of the small (non-fullscreen) window: outer
    /// position + inner size in logical points, winit/egui coordinates
    /// (top-left origin, y down — same system as [`crate::monitor`]).
    ///
    /// Without this the window has no start position at all and AppKit
    /// places it wherever it likes — on a multi-display desk that means a
    /// different monitor on almost every launch. All four are written
    /// together by [`save_window_geometry`] once the window has been still
    /// for a moment; a partial set (any field `None`) is ignored on load,
    /// so a hand-edited or truncated TOML falls back to the default size
    /// instead of restoring half a rectangle.
    ///
    /// Fullscreen is deliberately never sampled — in fullscreen the outer
    /// rect is the whole screen, and restoring *that* would reopen a
    /// screen-sized window on the next cold start.
    #[serde(default)]
    pub window_x: Option<i32>,
    #[serde(default)]
    pub window_y: Option<i32>,
    #[serde(default)]
    pub window_w: Option<i32>,
    #[serde(default)]
    pub window_h: Option<i32>,

    /// Which transport to open on startup. `"serial"` (default) uses the
    /// existing USB-Serial path. `"bluetooth"` opens the BLE Central and
    /// scans for a peer matching `bluetooth.peer_name` / `bluetooth.service_uuid`.
    #[serde(default = "default_transport")]
    pub transport: String,

    /// If the primary transport fails to open, fall back to this one.
    /// Currently only `"serial"` makes sense as a fallback. `None` (or
    /// missing field) = no fallback, error out and exit.
    #[serde(default)]
    pub transport_fallback: Option<String>,

    /// Bluetooth-specific settings. Used only when `transport == "bluetooth"`.
    #[serde(default)]
    pub bluetooth: BluetoothConfig,
}

fn default_true() -> bool {
    true
}

fn default_transport() -> String {
    "serial".to_string()
}

/// Custom deserializer for `preferred_monitor` that accepts either a string
/// (current format, e.g. "Studio Display (5120×2880 @ 0,0)") or a legacy TOML
/// integer (the field was `Option<usize>` in earlier builds — silently
/// dropped as `None` so the user re-picks via Settings).
///
/// Without this, a bare `Option::<String>` would refuse to parse an integer
/// and the whole TOML file would fail at the struct level, dragging
/// `load_from` into its "parse error → defaults" fallback path and wiping
/// every other persisted field. See test
/// `legacy_integer_preferred_monitor_keeps_other_fields`.
fn deserialize_preferred_monitor<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        S(String),
        // Legacy `Option<usize>` — accepted but silently discarded. `i64`
        // because TOML integers are signed; the original field was usize but
        // we never want to keep the value, just to avoid failing the parse.
        #[allow(dead_code)]
        I(i64),
    }
    Ok(match Option::<StringOrInt>::deserialize(d)? {
        Some(StringOrInt::S(s)) => Some(s),
        _ => None,
    })
}

/// First-run serial port, before the user picks one in Settings.
///
/// Nothing about a default port is portable: macOS names the FTDI/CH34x
/// adapter `/dev/cu.usbserial-NNN` (the number follows the physical USB
/// socket), Windows hands out `COMn`. Neither guess is reliable — the
/// adapter may well be somewhere else — but a wrong guess of the right
/// *shape* fails visibly and is one dropdown click from correct, whereas a
/// `/dev/...` path on Windows reads like a bug in the app.
#[cfg(target_os = "windows")]
pub const DEFAULT_PORT: &str = "COM3";
#[cfg(not(target_os = "windows"))]
pub const DEFAULT_PORT: &str = "/dev/cu.usbserial-120";

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT.to_string(),
            baud: 115_200,
            width: 2560,
            height: 1440,
            client_name: "wiredesk-client".to_string(),
            preferred_monitor: None,
            send_images: true,
            receive_images: true,
            send_text: true,
            receive_text: true,
            receive_files: true,
            send_files: false,
            swap_option_command: false,
            window_x: None,
            window_y: None,
            window_w: None,
            window_h: None,
            transport: default_transport(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        }
    }
}

#[allow(dead_code)] // wired up in later tasks of the launcher-ui plan
impl ClientConfig {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("WireDesk")
            .join("config.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<ClientConfig>(&s) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!(
                        "config parse error at {}: {e}; using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!(
                    "config read error at {}: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, s)
    }
}

/// Default inner size of the chrome window, used on first run and whenever
/// the persisted geometry fails [`sane_window_geometry`].
pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (520.0, 760.0);

/// Reject a persisted geometry that would open the window somewhere the
/// user can't reach it — NaN/infinite coordinates from a corrupt TOML, a
/// degenerate size, or a position thousands of points off any plausible
/// desktop (a config carried over from a machine with a very different
/// display layout).
///
/// Returns the geometry unchanged when it passes. This is a coarse filter,
/// not a monitor-bounds check: the display list isn't necessarily settled
/// this early in startup, so the "did it land off-screen?" check runs later
/// against live monitors (see `WireDeskApp::rescue_offscreen_window`).
pub fn sane_window_geometry(x: f32, y: f32, w: f32, h: f32) -> Option<(f32, f32, f32, f32)> {
    // A window narrower/shorter than this can't show the chrome at all, and
    // one larger than any real display is a sign of a garbled value.
    const MIN_W: f32 = 320.0;
    const MIN_H: f32 = 240.0;
    const MAX_SIDE: f32 = 20_000.0;
    // Generous enough for a tall multi-display wall, tight enough to catch
    // a coordinate that has clearly gone wrong.
    const MAX_ORIGIN: f32 = 30_000.0;

    if ![x, y, w, h].iter().all(|v| v.is_finite()) {
        return None;
    }
    if !(MIN_W..=MAX_SIDE).contains(&w) || !(MIN_H..=MAX_SIDE).contains(&h) {
        return None;
    }
    if x.abs() > MAX_ORIGIN || y.abs() > MAX_ORIGIN {
        return None;
    }
    Some((x, y, w, h))
}

/// Geometry to open the window with: the persisted rectangle when all four
/// fields are present and sane, otherwise `None` (caller uses
/// [`DEFAULT_WINDOW_SIZE`] and lets the window manager pick a position).
pub fn restore_window_geometry(cfg: &ClientConfig) -> Option<(f32, f32, f32, f32)> {
    let (x, y, w, h) = (cfg.window_x?, cfg.window_y?, cfg.window_w?, cfg.window_h?);
    sane_window_geometry(x as f32, y as f32, w as f32, h as f32)
}

/// Persist just the four geometry fields, re-reading the on-disk config
/// first.
///
/// Deliberately does **not** write the app's in-memory `ClientConfig`: the
/// Settings panel edits an unsaved buffer (`pending_config`) that only the
/// Save button is allowed to commit. Serialising the live struct here would
/// silently publish those pending edits the moment the user nudged the
/// window. Re-loading from disk and patching four fields keeps the two
/// write paths independent — the cost is one small read per debounced move.
pub fn save_window_geometry(x: f32, y: f32, w: f32, h: f32) -> io::Result<()> {
    save_window_geometry_to(&ClientConfig::config_path(), x, y, w, h)
}

/// Round a logical-point coordinate to the whole point stored in the TOML.
/// `f32 as i32` truncates toward zero, which would drift a window one point
/// left/up on every save at negative coordinates.
fn round_point(v: f32) -> i32 {
    v.round() as i32
}

/// Path-parameterised form of [`save_window_geometry`] so the read-modify-write
/// behaviour is testable against a temp file.
pub fn save_window_geometry_to(path: &Path, x: f32, y: f32, w: f32, h: f32) -> io::Result<()> {
    let mut cfg = ClientConfig::load_from(path);
    cfg.window_x = Some(round_point(x));
    cfg.window_y = Some(round_point(y));
    cfg.window_w = Some(round_point(w));
    cfg.window_h = Some(round_point(h));
    cfg.save_to(path)
}

/// Merge `ClientConfig` (from TOML / defaults) with parsed CLI args.
/// CLI values explicitly provided by the user (CommandLine / EnvVariable
/// sources) override TOML; default-only sources fall back to TOML.
pub fn merge_args(matches: &ArgMatches, mut cfg: ClientConfig) -> ClientConfig {
    if from_user(matches.value_source("port")) {
        if let Some(v) = matches.get_one::<String>("port") {
            cfg.port = v.clone();
        }
    }
    if from_user(matches.value_source("baud")) {
        if let Some(v) = matches.get_one::<u32>("baud") {
            cfg.baud = *v;
        }
    }
    if from_user(matches.value_source("name")) {
        if let Some(v) = matches.get_one::<String>("name") {
            cfg.client_name = v.clone();
        }
    }
    if from_user(matches.value_source("transport")) {
        if let Some(v) = matches.get_one::<String>("transport") {
            cfg.transport = v.clone();
        }
    }
    cfg
}

fn from_user(src: Option<ValueSource>) -> bool {
    matches!(
        src,
        Some(ValueSource::CommandLine) | Some(ValueSource::EnvVariable)
    )
}

/// Build the transport-layer's `TransportConfig` from a `ClientConfig`.
/// Mirrors the host-side helper — see `apps/wiredesk-host/src/config.rs`.
/// Decide which transport the client can actually open, given what the
/// config asks for.
///
/// The one case where the answer is not "what you asked for" is BLE on a
/// Windows client. `wiredesk-transport` picks the GATT role from the target
/// OS — macOS is the Central (scanner), Windows is the Peripheral (server) —
/// because until now only the host ran on Windows. A Windows *client* would
/// therefore start advertising, exactly like the host it is trying to reach,
/// and two peripherals never connect: `open()` succeeds, nothing links up,
/// and the failure is silent.
///
/// So we downgrade to serial and say why. Returns the transport name plus an
/// optional message for the log.
///
/// Pure, and takes the platform as an argument, so both branches are testable
/// on any host.
pub fn resolve_transport(requested: &str, client_is_windows: bool) -> (String, Option<String>) {
    if client_is_windows && requested.eq_ignore_ascii_case("bluetooth") {
        return (
            "serial".to_string(),
            Some(
                "transport=bluetooth is not supported on the Windows client                  (the BLE role is fixed to Peripheral there, same as the host)                  — falling back to serial"
                    .to_string(),
            ),
        );
    }
    (requested.to_string(), None)
}

pub fn to_transport_config(cfg: &ClientConfig) -> TransportConfig {
    let (transport, note) = resolve_transport(&cfg.transport, cfg!(target_os = "windows"));
    if let Some(note) = note {
        log::warn!("{note}");
    }
    TransportConfig {
        transport,
        serial: SerialFactoryConfig {
            port: cfg.port.clone(),
            baud: cfg.baud,
        },
        bluetooth: BluetoothFactoryConfig {
            service_uuid: cfg.bluetooth.service_uuid.clone(),
            peer_name: cfg.bluetooth.peer_name.clone(),
            mtu: cfg.bluetooth.mtu,
            connect_timeout_secs: cfg.bluetooth.connect_timeout_secs,
            reconnect_max_attempts: cfg.bluetooth.reconnect_max_attempts,
            require_encryption: cfg.bluetooth.require_encryption,
        },
        fallback: cfg.transport_fallback.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::CommandFactory;
    use tempfile::tempdir;

    #[test]
    fn default_port_has_the_shape_of_this_platform() {
        // A macOS device path on Windows (or vice versa) can never open, and
        // reads as a bug rather than "pick your port".
        if cfg!(target_os = "windows") {
            assert!(DEFAULT_PORT.starts_with("COM"), "{DEFAULT_PORT}");
        } else {
            assert!(DEFAULT_PORT.starts_with("/dev/"), "{DEFAULT_PORT}");
        }
    }

    #[test]
    fn resolve_transport_passes_serial_through() {
        for windows in [false, true] {
            let (t, note) = resolve_transport("serial", windows);
            assert_eq!(t, "serial");
            assert!(note.is_none());
        }
    }

    #[test]
    fn resolve_transport_keeps_bluetooth_on_mac() {
        let (t, note) = resolve_transport("bluetooth", false);
        assert_eq!(t, "bluetooth");
        assert!(note.is_none());
    }

    #[test]
    fn resolve_transport_downgrades_bluetooth_on_windows() {
        let (t, note) = resolve_transport("bluetooth", true);
        assert_eq!(t, "serial");
        let note = note.expect("a silent downgrade would be worse than none");
        assert!(note.contains("Windows"), "{note}");
    }

    #[test]
    fn resolve_transport_is_case_insensitive() {
        // config.toml is hand-edited; "Bluetooth" must not slip past the
        // check and start advertising.
        let (t, _) = resolve_transport("Bluetooth", true);
        assert_eq!(t, "serial");
    }

    #[test]
    fn resolve_transport_leaves_unknown_values_alone() {
        // Unknown names are the factory's business to reject, with its own
        // error message; silently rewriting them would hide a typo.
        let (t, note) = resolve_transport("carrier-pigeon", true);
        assert_eq!(t, "carrier-pigeon");
        assert!(note.is_none());
    }

    #[test]
    fn defaults_match_hardcodes() {
        let cfg = ClientConfig::default();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.width, 2560);
        assert_eq!(cfg.height, 1440);
        assert_eq!(cfg.client_name, "wiredesk-client");
        assert!(cfg.preferred_monitor.is_none());
        assert!(cfg.send_images);
        assert!(cfg.receive_images);
        assert!(cfg.receive_files);
        assert!(!cfg.send_files, "send_files is opt-in, default off");
        assert_eq!(cfg.transport, "serial");
        assert!(cfg.transport_fallback.is_none());
        assert_eq!(cfg.bluetooth, BluetoothConfig::default());
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = ClientConfig {
            port: "/dev/cu.wch-1".to_string(),
            baud: 57_600,
            width: 1920,
            height: 1080,
            client_name: "test-client".to_string(),
            preferred_monitor: None,
            send_images: true,
            receive_images: true,
            send_text: true,
            receive_text: true,
            receive_files: true,
            send_files: true,
            swap_option_command: false,
            window_x: None,
            window_y: None,
            window_w: None,
            window_h: None,
            transport: "serial".to_string(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        };
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn toml_transport_bluetooth_section_roundtrips() {
        let cfg = ClientConfig {
            transport: "bluetooth".to_string(),
            transport_fallback: Some("serial".to_string()),
            bluetooth: BluetoothConfig {
                service_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                peer_name: "TestHost".to_string(),
                mtu: 244,
                connect_timeout_secs: 5,
                reconnect_max_attempts: 3,
                require_encryption: true,
            },
            ..ClientConfig::default()
        };
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded.transport, "bluetooth");
        assert_eq!(loaded.transport_fallback.as_deref(), Some("serial"));
        assert_eq!(loaded.bluetooth.peer_name, "TestHost");
        assert_eq!(loaded.bluetooth.mtu, 244);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn partial_toml_without_bluetooth_section_uses_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.toml");
        fs::write(
            &path,
            "port = \"/dev/cu.legacy\"\n\
             baud = 115200\n\
             width = 2560\n\
             height = 1440\n\
             client_name = \"legacy-client\"\n",
        )
        .unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.port, "/dev/cu.legacy");
        assert_eq!(cfg.transport, "serial");
        assert!(cfg.transport_fallback.is_none());
        assert_eq!(cfg.bluetooth, BluetoothConfig::default());
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg, ClientConfig::default());
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        assert!(!path.parent().unwrap().exists());
        let cfg = ClientConfig::default();
        cfg.save_to(&path).unwrap();
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn load_garbage_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not valid toml [[[[").unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg, ClientConfig::default());
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "port = \"/dev/cu.usbserial-999\"\n").unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.port, "/dev/cu.usbserial-999");
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.client_name, "wiredesk-client");
        // New `preferred_monitor` field: a TOML written before the field
        // existed must round-trip as `None` rather than fail to deserialize.
        assert!(cfg.preferred_monitor.is_none());
    }

    #[test]
    fn legacy_integer_preferred_monitor_keeps_other_fields() {
        // Backward-compat: a config written by an older build had
        // `preferred_monitor` as `Option<usize>`, so the TOML contained a
        // bare integer (`preferred_monitor = 2`). Without the custom
        // deserializer the whole struct would fail to parse, `load_from`
        // would fall through to `Default::default()`, and the user would
        // silently lose every other persisted field on first upgrade.
        // We accept-and-discard the integer (→ None), preserving everything
        // else.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.toml");
        fs::write(
            &path,
            "port = \"/dev/cu.legacy\"\n\
             baud = 57600\n\
             width = 1920\n\
             height = 1080\n\
             client_name = \"legacy-client\"\n\
             preferred_monitor = 2\n",
        )
        .unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.port, "/dev/cu.legacy");
        assert_eq!(cfg.baud, 57_600);
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.client_name, "legacy-client");
        // The legacy integer is silently dropped — user re-picks via Settings.
        assert!(cfg.preferred_monitor.is_none());
    }

    #[test]
    fn toml_roundtrip_preferred_monitor() {
        let dir = tempdir().unwrap();

        // Case 1: None — the implicit default. Should survive roundtrip.
        let cfg_none = ClientConfig {
            preferred_monitor: None,
            ..ClientConfig::default()
        };
        let path = dir.path().join("none.toml");
        cfg_none.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded, cfg_none);
        assert!(loaded.preferred_monitor.is_none());

        // Case 2: Some(name) — a real display name. Should survive roundtrip.
        let cfg_some = ClientConfig {
            preferred_monitor: Some("Studio Display".to_string()),
            ..ClientConfig::default()
        };
        let path = dir.path().join("some.toml");
        cfg_some.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded, cfg_some);
        assert_eq!(loaded.preferred_monitor.as_deref(), Some("Studio Display"));
    }

    #[test]
    fn sane_geometry_accepts_ordinary_window() {
        assert_eq!(
            sane_window_geometry(1200.0, 240.0, 520.0, 760.0),
            Some((1200.0, 240.0, 520.0, 760.0))
        );
    }

    #[test]
    fn sane_geometry_accepts_negative_origin() {
        // A display to the left of / above the primary one has negative
        // winit coordinates — perfectly valid, must round-trip.
        assert!(sane_window_geometry(-2560.0, -300.0, 520.0, 760.0).is_some());
    }

    #[test]
    fn sane_geometry_rejects_nan_and_infinite() {
        assert!(sane_window_geometry(f32::NAN, 0.0, 520.0, 760.0).is_none());
        assert!(sane_window_geometry(0.0, f32::INFINITY, 520.0, 760.0).is_none());
        assert!(sane_window_geometry(0.0, 0.0, f32::NAN, 760.0).is_none());
    }

    #[test]
    fn sane_geometry_rejects_degenerate_size() {
        assert!(sane_window_geometry(0.0, 0.0, 0.0, 0.0).is_none());
        assert!(sane_window_geometry(0.0, 0.0, 100.0, 760.0).is_none());
        assert!(sane_window_geometry(0.0, 0.0, 520.0, 50.0).is_none());
    }

    #[test]
    fn sane_geometry_rejects_absurd_origin() {
        assert!(sane_window_geometry(500_000.0, 0.0, 520.0, 760.0).is_none());
    }

    #[test]
    fn restore_geometry_needs_all_four_fields() {
        // A half-written config (or one hand-edited to drop a field) must
        // fall back to the default size rather than restore a partial rect.
        let mut cfg = ClientConfig {
            window_x: Some(100),
            window_y: Some(100),
            window_w: Some(520),
            ..Default::default()
        };
        assert!(restore_window_geometry(&cfg).is_none());
        cfg.window_h = Some(760);
        assert_eq!(
            restore_window_geometry(&cfg),
            Some((100.0, 100.0, 520.0, 760.0))
        );
    }

    #[test]
    fn save_window_geometry_preserves_other_fields() {
        // The geometry writer must not clobber settings the user saved
        // through the Settings panel — it re-reads the file, patches four
        // fields and writes back.
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = ClientConfig {
            port: "/dev/cu.usbserial-999".to_string(),
            baud: 3_000_000,
            send_files: true,
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();

        save_window_geometry_to(&path, 12.0, 34.0, 520.0, 760.0).unwrap();

        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded.port, "/dev/cu.usbserial-999");
        assert_eq!(loaded.baud, 3_000_000);
        assert!(loaded.send_files);
        assert_eq!(
            restore_window_geometry(&loaded),
            Some((12.0, 34.0, 520.0, 760.0))
        );
    }

    #[test]
    fn save_window_geometry_creates_config_when_missing() {
        // First run: no config.toml yet. The write must still land, so the
        // very first "close where I left it" is honoured on the next launch.
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        save_window_geometry_to(&path, -100.0, 50.0, 600.0, 800.0).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(
            restore_window_geometry(&loaded),
            Some((-100.0, 50.0, 600.0, 800.0))
        );
    }

    #[test]
    fn config_without_window_fields_loads_as_none() {
        // Every config written before this feature lacks the four fields —
        // they must deserialize as None instead of failing the whole parse
        // (which would reset port/baud/toggles to defaults).
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "port = \"/dev/cu.usbserial-120\"\nbaud = 3000000\nwidth = 2560\nheight = 1440\nclient_name = \"wiredesk-client\"\n",
        )
        .unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.baud, 3_000_000);
        assert!(restore_window_geometry(&cfg).is_none());
    }

    fn toml_cfg() -> ClientConfig {
        ClientConfig {
            port: "/dev/cu.from-toml".to_string(),
            baud: 9_600,
            width: 1280,
            height: 720,
            client_name: "from-toml".to_string(),
            preferred_monitor: None,
            send_images: true,
            receive_images: true,
            send_text: true,
            receive_text: true,
            receive_files: true,
            send_files: false,
            swap_option_command: false,
            window_x: None,
            window_y: None,
            window_w: None,
            window_h: None,
            transport: "serial".to_string(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        }
    }

    #[test]
    fn merge_no_cli_args_keeps_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.from-toml");
        assert_eq!(merged.baud, 9_600);
        assert_eq!(merged.client_name, "from-toml");
    }

    #[test]
    fn merge_cli_port_overrides_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client", "--port", "/dev/cu.from-cli"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.from-cli");
        assert_eq!(merged.baud, 9_600); // not overridden — keeps TOML
        assert_eq!(merged.client_name, "from-toml");
    }

    #[test]
    fn merge_cli_all_fields_override_toml() {
        let matches = Args::command()
            .try_get_matches_from([
                "wiredesk-client",
                "--port",
                "/dev/cu.cli",
                "--baud",
                "57600",
                "--name",
                "cli-name",
            ])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.cli");
        assert_eq!(merged.baud, 57_600);
        assert_eq!(merged.client_name, "cli-name");
    }

    #[test]
    fn merge_cli_transport_overrides_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client", "--transport", "bluetooth"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.transport, "bluetooth");
        assert_eq!(merged.port, "/dev/cu.from-toml"); // unchanged
    }

    #[test]
    fn merge_no_transport_arg_keeps_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client"])
            .unwrap();
        let mut cfg = toml_cfg();
        cfg.transport = "bluetooth".to_string();
        let merged = merge_args(&matches, cfg);
        assert_eq!(merged.transport, "bluetooth");
    }

    #[test]
    fn config_roundtrip_with_receive_files() {
        // Task 8: explicit `receive_files = false` must survive a TOML
        // serialize → deserialize cycle. Without the field on the struct
        // the value would silently round-trip as `true` (the default),
        // breaking the Settings checkbox state across host restarts.
        let dir = tempdir().unwrap();
        let path = dir.path().join("rfiles.toml");
        let cfg = ClientConfig {
            receive_files: false,
            ..ClientConfig::default()
        };
        cfg.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert!(!loaded.receive_files);
        assert_eq!(loaded, cfg);

        // And the inverse — explicit `true` survives too (sanity check
        // that the field isn't being skipped on serialize/elided on read).
        let cfg_on = ClientConfig {
            receive_files: true,
            ..ClientConfig::default()
        };
        let path_on = dir.path().join("rfiles_on.toml");
        cfg_on.save_to(&path_on).unwrap();
        let loaded_on = ClientConfig::load_from(&path_on);
        assert!(loaded_on.receive_files);
    }

    #[test]
    fn config_back_compat_missing_receive_files() {
        // Task 8: a TOML file written before the field existed must load
        // with `receive_files = true` (default-on, preserves pre-Task-8
        // behaviour). Without `#[serde(default = "default_true")]` on the
        // field the deserializer would fail the whole struct and `load_from`
        // would wipe every other persisted field back to defaults.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.toml");
        fs::write(
            &path,
            "port = \"/dev/cu.legacy\"\n\
             baud = 57600\n\
             width = 1920\n\
             height = 1080\n\
             client_name = \"legacy-client\"\n\
             send_images = false\n\
             receive_images = false\n",
        )
        .unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.port, "/dev/cu.legacy");
        assert_eq!(cfg.baud, 57_600);
        assert!(!cfg.send_images);
        assert!(!cfg.receive_images);
        // The missing field falls back to the default-on closure.
        assert!(cfg.receive_files);
        // send_files is opt-in: a TOML without the field must load as `false`
        // so upgrading never silently starts sending files to the host.
        assert!(!cfg.send_files);
    }

    #[test]
    fn config_roundtrip_with_send_files() {
        // Explicit `send_files = true` must survive a serialize → deserialize
        // cycle, and the opt-in default (`false`) must round-trip too.
        let dir = tempdir().unwrap();
        let cfg_on = ClientConfig {
            send_files: true,
            ..ClientConfig::default()
        };
        let path_on = dir.path().join("sfiles_on.toml");
        cfg_on.save_to(&path_on).unwrap();
        let loaded_on = ClientConfig::load_from(&path_on);
        assert!(loaded_on.send_files);
        assert_eq!(loaded_on, cfg_on);

        let cfg_off = ClientConfig::default(); // send_files == false
        let path_off = dir.path().join("sfiles_off.toml");
        cfg_off.save_to(&path_off).unwrap();
        let loaded_off = ClientConfig::load_from(&path_off);
        assert!(!loaded_off.send_files);
    }

    #[test]
    fn to_transport_config_serial_passes_port_baud() {
        let cfg = ClientConfig {
            port: "/dev/cu.test".to_string(),
            baud: 921_600,
            transport: "serial".to_string(),
            ..ClientConfig::default()
        };
        let tc = to_transport_config(&cfg);
        assert_eq!(tc.transport, "serial");
        assert_eq!(tc.serial.port, "/dev/cu.test");
        assert_eq!(tc.serial.baud, 921_600);
        assert!(tc.fallback.is_none());
    }

    #[test]
    fn to_transport_config_bluetooth_carries_bt_fields() {
        let cfg = ClientConfig {
            transport: "bluetooth".to_string(),
            transport_fallback: Some("serial".to_string()),
            bluetooth: BluetoothConfig {
                service_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                peer_name: "TestHost".to_string(),
                mtu: 244,
                connect_timeout_secs: 5,
                reconnect_max_attempts: 3,
                require_encryption: true,
            },
            ..ClientConfig::default()
        };
        let tc = to_transport_config(&cfg);
        assert_eq!(tc.transport, "bluetooth");
        assert_eq!(
            tc.bluetooth.service_uuid,
            "11111111-2222-3333-4444-555555555555"
        );
        assert_eq!(tc.bluetooth.peer_name, "TestHost");
        assert_eq!(tc.bluetooth.mtu, 244);
        assert_eq!(tc.fallback.as_deref(), Some("serial"));
    }
}
