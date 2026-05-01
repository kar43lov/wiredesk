mod clipboard;
mod injector;
mod session;
mod shell;

use clap::Parser;
use wiredesk_core::error::Result;

#[derive(Parser)]
#[command(name = "wiredesk-host", about = "WireDesk host agent")]
struct Args {
    /// Serial port (e.g., COM3 on Windows, /dev/ttyUSB0 on Linux)
    #[arg(short, long)]
    port: String,

    /// Baud rate
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Host display name
    #[arg(long, default_value = "wiredesk-host")]
    name: String,

    /// Screen width (auto-detected on Windows)
    #[arg(long, default_value = "1920")]
    width: u16,

    /// Screen height (auto-detected on Windows)
    #[arg(long, default_value = "1080")]
    height: u16,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    log::info!("WireDesk Host Agent");
    log::info!("serial: {} @ {} baud", args.port, args.baud);
    log::info!("screen: {}x{}", args.width, args.height);

    let transport = wiredesk_transport::serial::SerialTransport::open(&args.port, args.baud)?;

    #[cfg(target_os = "windows")]
    let inj = injector::WindowsInjector::new()?;
    #[cfg(not(target_os = "windows"))]
    let inj = injector::MockInjector::default();

    let mut sess = session::Session::new(transport, inj, args.name, args.width, args.height);

    log::info!("waiting for client...");
    loop {
        match sess.tick() {
            Ok(_) => {}
            Err(wiredesk_core::error::WireDeskError::Transport(ref msg)) if msg.contains("timeout") => continue,
            // Protocol errors (bad magic, CRC mismatch, COBS decode) are
            // recoverable: the framing layer already discarded the bad bytes,
            // so just go back to recv() and try the next frame.
            Err(wiredesk_core::error::WireDeskError::Protocol(ref msg)) => {
                log::warn!("dropping bad frame: {msg}");
            }
            Err(e) => {
                log::error!("session error: {e}");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}
