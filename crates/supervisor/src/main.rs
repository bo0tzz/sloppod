use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Args {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let Args {} = Args::parse();
    Ok(())
}
