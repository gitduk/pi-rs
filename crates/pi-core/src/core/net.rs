//! The client the models are reached with, built once for the whole run: a
//! proxy identifies the app by the name on each request, and a wire that builds
//! its own client sends none.

use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use reqwest::Client;

/// Shared by every wire and by discovery, so a run keeps one connection pool
/// and one name. No total timeout: a stream may outlast any bound worth fixing.
///
/// The failure is kept rather than re-attempted: a machine that cannot build a
/// client cannot build the next one either.
pub fn client() -> Result<Client> {
    static CLIENT: LazyLock<std::result::Result<Client, String>> = LazyLock::new(|| {
        Client::builder()
            .user_agent(pi_store::USER_AGENT)
            .build()
            .map_err(|e| e.to_string())
    });
    match &*CLIENT {
        Ok(client) => Ok(client.clone()),
        Err(reason) => Err(anyhow!("{reason}")),
    }
}
