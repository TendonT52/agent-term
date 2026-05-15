mod cli;
mod client;
mod daemon;
mod doctor;
mod grep;
mod ids;
mod ipc;
mod meta;
mod pty;
mod slice;
mod state;
mod summary;
mod tail;
mod wait;

use std::env;
use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    #[cfg(unix)]
    unsafe {
        // Rust ignores SIGPIPE by default and turns broken pipes into panics
        // on println! / write!. Restore default disposition so a closed pipe
        // exits the process cleanly instead — matters when stdout is piped
        // to head, grep, etc.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if env::var("AGENT_TERMINAL_DAEMON").as_deref() == Ok("1") {
        return run_daemon_mode();
    }

    let cli = cli::Cli::parse();
    cli::run(cli)
}

fn run_daemon_mode() -> ExitCode {
    let id = env::var("AGENT_TERMINAL_ID").unwrap_or_else(|_| "default".to_string());

    let argv: Vec<String> = match env::var("AGENT_TERMINAL_CMD") {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(daemon::run_daemon(&id, argv)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}
