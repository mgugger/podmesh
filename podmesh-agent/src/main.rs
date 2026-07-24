use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = env_logger::try_init();
    podmesh_agent::run(podmesh_agent::Config::parse()).await
}
