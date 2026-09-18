use civil_workbench::api::{app, AppState};
use civil_workbench::config::Paths;
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    civil_workbench::config::load_env();

    let paths = Paths::detect();
    let port: u16 = std::env::var("CIVIL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8765);
    // CIVIL_HOST=0.0.0.0 opens the workbench to phones on the same LAN. Default stays loopback:
    // /api/local reads arbitrary files on this machine and there is no auth.
    let host: std::net::IpAddr = std::env::var("CIVIL_HOST")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
    let addr = SocketAddr::from((host, port));
    eprintln!(
        "Civil Buddy workbench (Rust)  kb={}  http://{addr}",
        paths.kb_root.display()
    );
    if !host.is_loopback() {
        eprintln!("warning: bound to {host}; anyone on this network can read /api/local and /api/file. Keep it on a trusted LAN.");
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("bind {addr} failed: {e}");
            std::process::exit(1);
        });
    axum::serve(listener, app(AppState::live(paths)))
        .await
        .expect("server");
}
