use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "beemesh", about = "beemesh CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Apply a configuration file to the cluster
    Apply {
        /// Filename, e.g. -f ./pod.yaml
        #[arg(short = 'f', long = "file", value_name = "FILE")] 
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Apply { file } => apply_file(file).await?,
    }

    Ok(())
}

async fn apply_file(path: PathBuf) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }

    // Read file contents
    let contents = tokio::fs::read_to_string(&path).await?;

    // Minimal inspection: find kind/name fields for YAML-like files
    let mut kind = None;
    let mut name = None;

    for line in contents.lines() {
        let l = line.trim();
        if l.starts_with("kind:") && kind.is_none() {
            kind = Some(l[5..].trim().to_string());
        }
        if l.starts_with("name:") && name.is_none() {
            name = Some(l[5..].trim().to_string());
        }
        if kind.is_some() && name.is_some() {
            break;
        }
    }

    println!("Applying file: {}", path.display());

    match (kind, name) {
        (Some(k), Some(n)) => println!("Detected resource: kind='{}' name='{}'", k, n),
        (Some(k), None) => println!("Detected resource: kind='{}' (no name found)", k),
        _ => println!("No top-level kind/name detected; sending raw payload..."),
    }

    // TODO: send to beemesh API endpoint or socket; for now simulate success
    println!("Apply succeeded (simulation)");

    Ok(())
}
