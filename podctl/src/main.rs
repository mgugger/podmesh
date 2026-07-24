use clap::{Parser, Subcommand};
use podctl::{apply_file, cert, delete_file, get_logs, get_pod, get_pods};
use std::io::Write;
use std::path::PathBuf;

mod convert;

#[derive(Parser, Debug)]
#[command(
    name = "podctl",
    about = "podmesh CLI - manage workloads on podmesh cluster"
)]
struct Cli {
    /// REST API base URL (can also be set via PODMESH_API)
    #[arg(long = "api-url", env = "PODMESH_API", value_name = "URL")]
    api_url: Option<String>,
    /// Output format (table or json)
    #[arg(
        long = "output",
        short = 'o',
        value_name = "FORMAT",
        default_value = "table"
    )]
    output: String,
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
    /// Delete a configuration from the cluster
    Delete {
        /// Filename, e.g. -f ./pod.yaml
        #[arg(short = 'f', long = "file", value_name = "FILE")]
        file: PathBuf,
        /// Force deletion without confirmation
        #[arg(long = "force")]
        force: bool,
    },
    /// Get information about resources
    Get {
        #[command(subcommand)]
        resource: GetResource,
    },
    /// Get logs from a workload
    Logs {
        /// Workload ID or name
        workload_id: String,
        /// Number of lines to show from the end (tail)
        #[arg(long = "tail", short = 'n')]
        tail: Option<usize>,
    },
    /// Convert a Kubernetes manifest to podmesh format
    Convert {
        #[arg(short, long)]
        file: String,
    },
    /// NodeCert management (issue, show, verify, grant-proxy)
    Cert {
        #[command(subcommand)]
        cmd: cert::CertCommands,
    },
}

#[derive(Subcommand, Debug)]
enum GetResource {
    /// List all pods/workloads
    #[command(alias = "pod")]
    Pods {
        /// Specific workload ID to get details for
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let Cli {
        api_url,
        output: _,
        command,
    } = cli;

    match command {
        Commands::Apply { file } => {
            let workload_id = apply_file(file, api_url.as_deref()).await?;
            writeln!(std::io::stdout(), "Applied workload {workload_id}")?;
        }
        Commands::Delete { file, force } => {
            delete_file(file, force, api_url.as_deref()).await?;
            writeln!(std::io::stdout(), "Deleted successfully")?;
        }
        Commands::Get { resource } => match resource {
            GetResource::Pods {
                name: Some(workload_id),
            } => {
                let response = get_pod(&workload_id, api_url.as_deref()).await?;
                writeln!(std::io::stdout(), "{response}")?;
            }
            GetResource::Pods { name: None } => {
                let response = get_pods(api_url.as_deref()).await?;
                writeln!(std::io::stdout(), "{response}")?;
            }
        },
        Commands::Logs { workload_id, tail } => {
            let logs = get_logs(&workload_id, tail, api_url.as_deref()).await?;
            write!(std::io::stdout(), "{logs}")?;
        }
        Commands::Convert { file } => {
            let yaml = std::fs::read_to_string(&file)?;
            let (output, warnings) = convert::convert_manifest(&yaml)?;
            for w in &warnings {
                writeln!(std::io::stderr(), "{w}")?;
            }
            write!(std::io::stdout(), "{output}")?;
        }
        Commands::Cert { cmd } => {
            cert::handle_cert_command(cmd).await?;
        }
    }

    Ok(())
}
