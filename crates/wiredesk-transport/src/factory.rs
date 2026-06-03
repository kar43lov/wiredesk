//! Transport factory — picks `SerialTransport` or `BluetoothLeTransport`
//! based on a runtime config string. Runs an optional fallback if the
//! primary transport fails to open. Lives in `wiredesk-transport` so both
//! `wiredesk-host` and `wiredesk-client` can share it without duplicating
//! the logic.

use wiredesk_core::error::{Result, WireDeskError};

use crate::bluetooth::{BluetoothFactoryConfig, BluetoothLeTransport};
use crate::serial::SerialTransport;
use crate::transport::Transport;

/// Tagged copy of the serial-specific knobs from `HostConfig` /
/// `ClientConfig`. The factory takes its own struct so the apps can
/// resolve their config however they like (TOML, CLI override, env)
/// without dragging app types into the transport crate.
#[derive(Clone, Debug)]
pub struct SerialFactoryConfig {
    pub port: String,
    pub baud: u32,
}

/// Top-level config for `open_transport`. `transport` selects the primary
/// channel; `fallback` (optional) names the channel to retry on primary
/// failure. Today only `"serial"` is a meaningful fallback — `"bluetooth"`
/// would just fail again — but the field is shaped as `Option<String>` so
/// the schema doesn't bake the assumption in.
#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub transport: String,
    pub serial: SerialFactoryConfig,
    pub bluetooth: BluetoothFactoryConfig,
    pub fallback: Option<String>,
}

/// Open the configured transport. On `transport == "bluetooth"` failure and
/// `fallback == Some("serial")`, log a warning and try the serial path. The
/// caller (apps' main.rs) is responsible for surfacing the final error if
/// both attempts fail.
pub fn open_transport(cfg: &TransportConfig) -> Result<Box<dyn Transport>> {
    match cfg.transport.as_str() {
        "serial" => {
            let t = SerialTransport::open(&cfg.serial.port, cfg.serial.baud)?;
            Ok(Box::new(t))
        }
        "bluetooth" => match BluetoothLeTransport::open(&cfg.bluetooth) {
            Ok(t) => Ok(Box::new(t)),
            Err(primary_err) => {
                if cfg.fallback.as_deref() == Some("serial") {
                    log::warn!(
                        "bluetooth transport failed ({primary_err}); falling back to serial"
                    );
                    let t = SerialTransport::open(&cfg.serial.port, cfg.serial.baud)?;
                    Ok(Box::new(t))
                } else {
                    Err(primary_err)
                }
            }
        },
        other => Err(WireDeskError::Transport(format!(
            "unknown transport '{other}' (expected 'serial' or 'bluetooth')"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt_cfg() -> BluetoothFactoryConfig {
        // Definitely-not-our-real-service UUID — keeps tests deterministic
        // when a real WireDesk host is advertising in radio range.
        BluetoothFactoryConfig {
            service_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
            peer_name: "TestHost".to_string(),
            mtu: 247,
            connect_timeout_secs: 1,
            reconnect_max_attempts: 1,
        }
    }

    fn serial_cfg_invalid() -> SerialFactoryConfig {
        // Path that won't open on any platform we run tests on.
        SerialFactoryConfig {
            port: "/dev/null/does-not-exist".to_string(),
            baud: 115_200,
        }
    }

    /// `Box<dyn Transport>` doesn't implement `Debug`, so `Result.unwrap_err()`
    /// won't compile — pull the error out by hand.
    fn expect_err(result: Result<Box<dyn Transport>>) -> WireDeskError {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn unknown_transport_errors() {
        let cfg = TransportConfig {
            transport: "ftdi".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: None,
        };
        let err = expect_err(open_transport(&cfg));
        let msg = err.to_string();
        assert!(
            msg.contains("unknown transport") && msg.contains("ftdi"),
            "expected unknown-transport error, got: {msg}"
        );
    }

    #[test]
    fn empty_transport_errors() {
        // Empty string is also unknown — be explicit so a config typo
        // doesn't silently pick a default.
        let cfg = TransportConfig {
            transport: "".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: None,
        };
        let err = expect_err(open_transport(&cfg));
        assert!(err.to_string().contains("unknown transport"));
    }

    #[test]
    fn serial_transport_attempts_serial_open() {
        // We can't easily open a real serial port in tests, but we can
        // verify the factory tried the serial path by checking the error
        // message — SerialTransport::open would return a port-open error,
        // not the "unknown transport" / "BLE pending" messages.
        let cfg = TransportConfig {
            transport: "serial".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: None,
        };
        let err = expect_err(open_transport(&cfg));
        let msg = err.to_string();
        assert!(
            msg.contains("serial open"),
            "expected SerialTransport::open error, got: {msg}"
        );
    }

    // These three tests assert the factory's *fallback* behaviour, which is
    // only observable when `BluetoothLeTransport::open()` fails. That holds on
    // the Mac/Central side (open() scans for a peer and times out when none is
    // advertising). On Windows the BLE side is a GATT *Peripheral* — open()
    // just builds the service and starts advertising, which succeeds with no
    // peer present, so open() returns Ok and there's no error path to assert.
    // Ignored on Windows rather than deleted: the fallback logic itself is
    // cross-platform and still worth covering on the Central side.
    #[test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "BLE open() is success-on-advertise on the Win Peripheral side; no error path to assert"
    )]
    fn bluetooth_transport_without_fallback_returns_ble_error() {
        // No advertising peer in the test environment, so BLE open() will
        // fail. Without fallback the factory surfaces the BLE error
        // directly. Acceptable error origins:
        //   - "BLE: ..." (Mac/Win real impls — scan timeout, missing
        //     adapter, etc.)
        //   - "Task 7" (Windows placeholder before that task lands)
        //   - "not supported" (Linux dev box via stub.rs)
        let cfg = TransportConfig {
            transport: "bluetooth".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: None,
        };
        let err = expect_err(open_transport(&cfg));
        let msg = err.to_string();
        let is_ble_origin = msg.contains("BLE")
            || msg.contains("Task 7")
            || msg.contains("not supported");
        assert!(is_ble_origin, "expected BLE-origin error, got: {msg}");
        // Confirm we did NOT accidentally route through the serial path
        // (would surface "serial open: ...").
        assert!(
            !msg.contains("serial open"),
            "BLE failure must not fall through to serial without fallback, got: {msg}"
        );
    }

    #[test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "BLE open() is success-on-advertise on the Win Peripheral side; fallback never triggers"
    )]
    fn bluetooth_init_fail_falls_back_to_serial() {
        // BLE fails (impl pending) → fallback "serial" kicks in → serial
        // also fails (invalid port) → final error is from SerialTransport,
        // proving the fallback path executed end-to-end.
        let cfg = TransportConfig {
            transport: "bluetooth".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: Some("serial".to_string()),
        };
        let err = expect_err(open_transport(&cfg));
        let msg = err.to_string();
        assert!(
            msg.contains("serial open"),
            "expected fallback path to surface serial error, got: {msg}"
        );
    }

    #[test]
    #[cfg_attr(
        target_os = "windows",
        ignore = "BLE open() is success-on-advertise on the Win Peripheral side; no error path to assert"
    )]
    fn unknown_fallback_value_does_not_recurse() {
        // Only "serial" is a valid fallback string. Anything else means
        // "no fallback" — the primary BLE error surfaces directly without
        // attempting a second open.
        let cfg = TransportConfig {
            transport: "bluetooth".to_string(),
            serial: serial_cfg_invalid(),
            bluetooth: bt_cfg(),
            fallback: Some("ftdi".to_string()),
        };
        let err = expect_err(open_transport(&cfg));
        let msg = err.to_string();
        // BLE-origin error, not "serial open" — fallback string was
        // ignored because it isn't "serial".
        assert!(
            !msg.contains("serial open"),
            "unrecognised fallback should NOT trigger serial retry, got: {msg}"
        );
    }
}
