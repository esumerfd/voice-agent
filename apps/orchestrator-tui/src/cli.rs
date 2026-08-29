//! CLI argument surface for `orchestrator-tui`.
//!
//! `--port` is flag-only (mirrors `orchestrator-cli/src/cli.rs`'s D-02/D-06
//! precedent): deliberately no environment-variable override -- the
//! daemon's WS port must never be silently overridden by an inherited
//! environment variable on the client side.

use clap::Parser;

#[derive(Parser)]
#[command(name = "orchestrator-tui")]
pub struct Cli {
    /// WS port of the running `orchestratord` daemon to connect to
    /// (flag-only -- no environment-variable override).
    #[arg(long, default_value_t = shared::DEFAULT_PORT)]
    pub port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_shared_default_port() {
        let cli = Cli::parse_from(["orchestrator-tui"]);
        assert_eq!(cli.port, shared::DEFAULT_PORT);
    }

    #[test]
    fn port_accepts_explicit_override() {
        let cli = Cli::parse_from(["orchestrator-tui", "--port", "9999"]);
        assert_eq!(cli.port, 9999);
    }
}
