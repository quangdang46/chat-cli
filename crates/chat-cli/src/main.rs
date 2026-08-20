mod cli;
mod dispatch;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse_args();
    dispatch::run(args).await
}
