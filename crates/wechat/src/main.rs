//! `pi-wechat`: run by pi as a tool, it bridges the session to a WeChat chat.
//! Put a shim that execs it in `~/.pi/tools/`; see `examples/tools/wechat`.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match wechat::bridge::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}
