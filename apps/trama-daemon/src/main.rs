use trama_daemon::DaemonError;

#[tokio::main]
async fn main() {
    if let Err(err) = trama_daemon::run().await {
        match err {
            DaemonError::Io(io) if io.kind() == std::io::ErrorKind::AddrInUse => {
                eprintln!("trama-daemon: socket already in use");
            }
            other => eprintln!("trama-daemon: {other}"),
        }
        std::process::exit(1);
    }
}
