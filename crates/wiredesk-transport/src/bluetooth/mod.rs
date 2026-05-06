//! Bluetooth Low Energy transport (Plan C).
//!
//! Adds an alternative to `SerialTransport` that uses BLE GATT instead of
//! USB-Serial. The Mac runs a BLE Central, Win11 runs a BLE Peripheral —
//! the same custom service is exposed by both with two characteristics:
//!
//! - **TX char (Notify, Win→Mac)** — host pushes packets to client.
//! - **RX char (WriteWithResponse, Mac→Win)** — client sends packets to host.
//!
//! The actual platform-specific implementations live in [`mac`] and [`win`]
//! modules, gated by `cfg(target_os = ...)`. A trivial [`stub`] keeps the
//! crate buildable on platforms where neither btleplug nor windows-rs BLE
//! support exists (Linux dev machines).
//!
//! Implementation tasks land in this module incrementally — see
//! `docs/plans/20260506-bluetooth-le-transport.md`.

pub mod fragment;
pub mod reconnect;
pub mod runtime;
pub mod uuids;

/// Runtime configuration handed to `BluetoothLeTransport::open`.
///
/// Owned by the transport layer (not the per-app config crates) so that the
/// factory can construct one from either `HostConfig` or `ClientConfig`. The
/// shared `BluetoothConfig` in `wiredesk-core` (Task 2) is the source of
/// truth for the field defaults.
#[derive(Clone, Debug)]
pub struct BluetoothFactoryConfig {
    pub service_uuid: String,
    pub peer_name: String,
    pub mtu: u16,
    pub connect_timeout_secs: u32,
    pub reconnect_max_attempts: u32,
}

// Concrete BluetoothLeTransport is provided by exactly one of these
// platform-fenced submodules. They all expose the same public API surface.
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "windows")]
mod win;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub;

#[cfg(target_os = "macos")]
pub use mac::BluetoothLeTransport;
#[cfg(target_os = "windows")]
pub use win::BluetoothLeTransport;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use stub::BluetoothLeTransport;
