#[cfg(target_os = "macos")]
fn main() {
    match wiredesk_browser_cli::Service::start() {
        Ok(service) => {
            eprintln!(
                "Viewer: {}\nCDP: {}",
                service.viewer_url(),
                wiredesk_browser_cli::CDP_URL
            );
            while service.is_running() {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            eprintln!("wd-browser: browser service stopped");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("wd-browser: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("wd-browser currently requires the macOS WireDesk GUI IPC relay");
    std::process::exit(1);
}
