use clap::{Parser, Subcommand};
use podctl::{apply_file, delete_file, get_pods, get_pod, get_logs, format_workloads_table, format_workload_details};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "podctl", about = "podmesh CLI - manage workloads on podmesh cluster")]
struct Cli {
    /// REST API base URL (can also be set via PODMESH_API)
    #[arg(long = "api-url", env = "PODMESH_API", value_name = "URL")]
    api_url: Option<String>,
    /// Output format (table or json)
    #[arg(long = "output", short = 'o', value_name = "FORMAT", default_value = "table")]
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
    let Cli { api_url, output, command } = cli;
    let use_json = output.to_lowercase() == "json";

    match command {
        Commands::Apply { file } => {
            let manifest_id = apply_file(file, api_url.as_deref()).await?;
            println!("Applied manifest: {}", manifest_id);
        }
        Commands::Delete { file, force } => {
            delete_file(file, force, api_url.as_deref()).await?;
            println!("Deleted successfully");
        }
        Commands::Get { resource } => {
            match resource {
                GetResource::Pods { name: Some(workload_id) } => {
                    let response = get_pod(&workload_id, api_url.as_deref()).await?;
                    if use_json {
                        println!("{}", response);
                    } else {
                        println!("{}", format_workload_details(&response));
                    }
                }
                GetResource::Pods { name: None } => {
                    let response = get_pods(api_url.as_deref()).await?;
                    if use_json {
                        println!("{}", response);
                    } else {
                        println!("{}", format_workloads_table(&response));
                    }
                }
            }
        }
        Commands::Logs { workload_id, tail } => {
            let logs = get_logs(&workload_id, tail, api_url.as_deref()).await?;
            print!("{}", logs);
        }
    }

    Ok(())
}
