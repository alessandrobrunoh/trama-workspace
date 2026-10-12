use trama_daemon::DaemonError;

#[tokio::main]
async fn main() {
    // Claude Code runs `trama-daemon approval-hook` as its PermissionRequest
    // hook; it must never start a second daemon.
    if std::env::args().nth(1).as_deref() == Some(trama_daemon::approval::APPROVAL_HOOK_SUBCOMMAND)
    {
        std::process::exit(trama_daemon::approval::run_hook().await);
    }
    // Claude Code runs `trama-daemon mcp` as Trama's stdio MCP server.
    if std::env::args().nth(1).as_deref() == Some(trama_daemon::mcp::MCP_SUBCOMMAND) {
        std::process::exit(trama_daemon::mcp::run_server().await);
    }
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
