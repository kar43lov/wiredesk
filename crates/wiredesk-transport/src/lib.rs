pub mod bluetooth;
pub mod detect;
pub mod factory;
pub mod mock;
pub mod serial;
pub mod transport;

pub use bluetooth::{uuids, BluetoothLeTransport};
pub use detect::{
    classify_ports, enumerate_ports_now, target_indices, AdapterKind, DetectedPort, FTDI_VID,
    WCH_VID,
};
pub use factory::{open_transport, SerialFactoryConfig, TransportConfig};
