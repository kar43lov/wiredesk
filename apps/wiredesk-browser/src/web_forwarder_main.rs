#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = wiredesk_browser_cli::web_forwarder::run_launchd() {
        eprintln!("wiredesk-web-forwarder: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The browser address helper requires macOS launchd");
    std::process::exit(1);
}
