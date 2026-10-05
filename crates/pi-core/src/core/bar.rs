//! Running the bar script; what it prints is read by `pi_store::bar`.

use std::path::Path;
use std::time::Duration;

use pi_store::bar::{Layout, parse};

// A first run builds the script and its dependencies, which takes a while.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Run the script at `path`, `input` on its stdin, and read what it printed.
pub async fn run(path: &Path, input: Vec<u8>, ctx: &tool::Ctx) -> Result<Layout, String> {
    toolbox::scripts::run_script(path, input, TIMEOUT, ctx)
        .await
        .and_then(|out| parse(&out))
}
