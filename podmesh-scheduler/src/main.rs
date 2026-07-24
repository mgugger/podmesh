use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = env_logger::try_init();
    podmesh_scheduler::run(podmesh_scheduler::Config::parse()).await
}
