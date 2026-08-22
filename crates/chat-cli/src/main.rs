mod cli;
mod dispatch;

use anyhow::Result;

// Force the provider crates to be linked: `inventory::submit!` entries are
// dropped by the linker unless some symbol from each crate is referenced.
// Without this, the release binary starts with an empty provider registry.
#[allow(unused_imports)]
use provider_chatgpt as _chatgpt_link;
#[allow(unused_imports)]
use provider_deepseek as _deepseek_link;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse_args();
    dispatch::run(args).await
}
