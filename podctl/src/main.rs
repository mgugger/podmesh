use clap::{Parser, Subcommand};
use podctl::{
    ClientOptions, OutputFormat, apply_file, cert, delete_deployment, delete_file, get_logs,
    get_pod, get_pods, list_workloads, namespace_id,
};
use std::io::Write;
use std::path::PathBuf;

mod convert;

#[derive(Parser, Debug)]
#[command(
    name = "podctl",
    version,
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
    output: OutputFormat,
    /// Deploy to whichever agent the mesh offers instead of consulting the
    /// trusted agent list. The selected agent can read the workload in full,
    /// so this hands that ability to whoever answers the selection request.
    #[arg(
        long = "trust-any-agent",
        env = "PODMESH_TRUST_ANY_AGENT",
        global = true
    )]
    trust_any_agent: bool,
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
    /// Delete a deployment, addressed by manifest file or by name/id
    Delete {
        /// Filename, e.g. -f ./pod.yaml
        #[arg(short = 'f', long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
        /// Deployment id or workload name, when the manifest is unavailable
        #[arg(value_name = "DEPLOYMENT")]
        deployment: Option<String>,
        /// Drop the local catalog entry even if some replicas could not be deleted
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
        /// Deployment id or workload name
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
    /// Ask the mesh which workloads it is running for this owner
    ///
    /// The local catalog only knows what this installation deployed. This asks
    /// the agents, so it also finds workloads whose catalog entry was lost,
    /// overwritten, or written on another machine.
    List,
    /// Print this installation's namespace (owner) identity
    Whoami,
    /// Proxy grant management
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
        /// Specific deployment id or workload name to get details for
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let Cli {
        api_url,
        output,
        trust_any_agent,
        command,
    } = Cli::parse();
    let options = ClientOptions {
        api_base: api_url,
        trust_any_agent,
    };

    match command {
        Commands::Apply { file } => {
            let workload_id = apply_file(file, &options).await?;
            writeln!(std::io::stdout(), "Applied workload {workload_id}")?;
        }
        Commands::Delete {
            file,
            deployment,
            force,
        } => {
            let deployment_id = match (file, deployment) {
                (Some(file), None) => delete_file(file, force, &options).await?,
                (None, Some(deployment)) => delete_deployment(&deployment, force, &options).await?,
                (Some(_), Some(_)) => {
                    anyhow::bail!("pass either --file or a deployment id, not both")
                }
                (None, None) => anyhow::bail!("pass --file or a deployment id to delete"),
            };
            writeln!(std::io::stdout(), "Deleted {deployment_id}")?;
        }
        Commands::Get { resource } => match resource {
            GetResource::Pods { name: Some(name) } => {
                let response = get_pod(&name, &options).await?;
                writeln!(std::io::stdout(), "{response}")?;
            }
            GetResource::Pods { name: None } => {
                writeln!(std::io::stdout(), "{}", get_pods(output)?)?;
            }
        },
        Commands::Logs { workload_id, tail } => {
            let logs = get_logs(&workload_id, tail, &options).await?;
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
        Commands::List => {
            writeln!(
                std::io::stdout(),
                "{}",
                list_workloads(&options, output).await?
            )?;
        }
        Commands::Whoami => {
            writeln!(std::io::stdout(), "{}", namespace_id()?)?;
        }
        Commands::Cert { cmd } => {
            cert::handle_cert_command(cmd).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_proxy_trust_commands_parse() {
        let trust = Cli::try_parse_from([
            "podctl",
            "cert",
            "trust-proxy",
            "--proxy-url",
            "http://proxy.example:7100",
            "--replace",
        ])
        .unwrap();
        assert!(matches!(
            trust.command,
            Commands::Cert {
                cmd: cert::CertCommands::TrustProxy { replace: true, .. }
            }
        ));

        let list = Cli::try_parse_from(["podctl", "cert", "list-proxies"]).unwrap();
        assert!(matches!(
            list.command,
            Commands::Cert {
                cmd: cert::CertCommands::ListProxies
            }
        ));

        let remove = Cli::try_parse_from([
            "podctl",
            "cert",
            "remove-proxy",
            "--proxy-url",
            "http://proxy.example:7100",
        ])
        .unwrap();
        assert!(matches!(
            remove.command,
            Commands::Cert {
                cmd: cert::CertCommands::RemoveProxy { .. }
            }
        ));
    }
}
