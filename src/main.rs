mod auth;
mod cli;
mod client;
mod complete;
mod config;
mod error;
mod help;
mod loose;
mod manifest;
mod output;
mod params;
mod table;

fn main() {
    // Exit quietly when the reader goes away (`bias help | head`) instead of panicking on write.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = cli::run(args) {
        error.print();
        std::process::exit(error.exit_code());
    }
}
