use std::process::Command;

/// The daemon binary with a cleared environment.
pub fn daemon() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lrc-sync"));
    command.env_clear();
    // Without a system store (the Nix build sandbox) the HTTP client needs the CA bundle.
    if let Some(path) = std::env::var_os("SSL_CERT_FILE") {
        command.env("SSL_CERT_FILE", path);
    }
    command
}
