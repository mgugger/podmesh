use std::ffi::{CString, c_char};
use std::fs::{self, File};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use libloading::Library;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;
const HOST_MEMORY_RESERVE_MIB: u64 = 1024;

fn guest_memory_mib(name: &str, two_tier: bool) -> u32 {
    if two_tier && name.starts_with("agent") {
        1024
    } else if name.starts_with("agent") || name == "control" {
        512
    } else {
        256
    }
}

fn proc_memory_kib(text: &str, field: &str) -> Result<u64> {
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .with_context(|| format!("missing {field} in Linux memory diagnostics"))?;
    let mut parts = value.split_whitespace();
    let amount: u64 = parts.next().context("missing memory amount")?.parse()?;
    ensure!(
        parts.next() == Some("kB"),
        "unexpected memory unit for {field}"
    );
    Ok(amount)
}

fn check_memory_budget(available_bytes: u64, guest_mib: u64) -> Result<()> {
    let required = (guest_mib + HOST_MEMORY_RESERVE_MIB) * MIB;
    ensure!(
        available_bytes >= required,
        "insufficient memory for KVM: available {} MiB, need {guest_mib} MiB guest RAM plus {HOST_MEMORY_RESERVE_MIB} MiB host reserve",
        available_bytes / MIB
    );
    Ok(())
}

fn memory_preflight(names: &[&str], two_tier: bool) -> Result<()> {
    let guest_mib: u64 = names
        .iter()
        .map(|name| u64::from(guest_memory_mib(name, two_tier)))
        .sum();
    let mut available =
        proc_memory_kib(&fs::read_to_string("/proc/meminfo")?, "MemAvailable:")? * 1024;
    let cgroups = fs::read_to_string("/proc/self/cgroup")?;
    let relative = cgroups
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("KVM memory preflight requires cgroup v2")?;
    let root = Path::new("/sys/fs/cgroup");
    let mut group = root.join(relative.trim_start_matches('/'));
    loop {
        let limit_path = group.join("memory.max");
        match fs::read_to_string(&limit_path) {
            Ok(limit) if limit.trim() != "max" => {
                let limit: u64 = limit.trim().parse().context("parse cgroup memory limit")?;
                let current: u64 = fs::read_to_string(group.join("memory.current"))?
                    .trim()
                    .parse()
                    .context("parse cgroup memory usage")?;
                available = available.min(limit.saturating_sub(current));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("read cgroup memory limit"),
        }
        if group == root {
            break;
        }
        ensure!(
            group.pop() && group.starts_with(root),
            "invalid cgroup path"
        );
    }
    check_memory_budget(available, guest_mib)?;
    println!(
        "KVM {} profile: {} guests, {} vCPUs, {guest_mib} MiB guest RAM; available {} MiB, host reserve {HOST_MEMORY_RESERVE_MIB} MiB",
        if two_tier { "two-tier" } else { "lean" },
        names.len(),
        names
            .iter()
            .map(|name| if guest_memory_mib(name, two_tier) == 1024 {
                2
            } else {
                1
            })
            .sum::<usize>(),
        available / MIB
    );
    Ok(())
}
pub const MESH_GUESTS: [&str; 8] = [
    "scheduler-1",
    "scheduler-2",
    "scheduler-3",
    "agent-1",
    "agent-2",
    "agent-3",
    "proxy-1",
    "proxy-2",
];
pub const MESH_ATTACHMENTS: [(&str, &[&str]); 3] = [
    ("scheduler-1", &[]),
    ("scheduler-2", &["agent-1", "agent-2"]),
    ("scheduler-3", &["agent-3"]),
];

pub struct Guest {
    pub name: String,
    pub address: Ipv4Addr,
    pub console: PathBuf,
    child: Child,
}

pub struct KvmRun {
    directory: TempDir,
    namespace: String,
    host_link: String,
    subnet: Ipv4Addr,
    forced_relay_table: Option<String>,
    namespace_created: bool,
    host_link_created: bool,
    pub guests: Vec<Guest>,
}

pub async fn command(program: &str, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    let started = Instant::now();
    let mut process = tokio::process::Command::new(program);
    process
        .args(args)
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = process
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let output = tokio::time::timeout(COMMAND_TIMEOUT, async {
        let mut child = child;
        if let Some(input) = input {
            child
                .stdin
                .take()
                .context("command stdin")?
                .write_all(input)
                .await?;
        }
        child.wait_with_output().await.context("wait for command")
    })
    .await
    .with_context(|| format!("{program} exceeded {COMMAND_TIMEOUT:?}"))??;
    log::debug!(
        "{program} completed in {:.3}s; input bytes={}",
        started.elapsed().as_secs_f64(),
        input.map_or(0, <[u8]>::len)
    );
    ensure!(
        output.stdout.len() + output.stderr.len() <= MAX_OUTPUT,
        "{program} output exceeded {MAX_OUTPUT} bytes"
    );
    ensure!(
        output.status.success(),
        "{program} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).with_context(|| format!("{program} returned non-UTF8 output"))
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str().context("VM artifact path must be UTF-8")
}

fn route_overlaps(destination: &str, subnet: Ipv4Addr) -> Result<bool> {
    if destination == "default" || destination.contains(':') {
        return Ok(false);
    }
    let (address, prefix) = destination.split_once('/').unwrap_or((destination, "32"));
    let address: Ipv4Addr = address.parse().context("parse host route address")?;
    let prefix: u32 = prefix.parse().context("parse host route prefix")?;
    ensure!(prefix <= 32, "invalid host route prefix");
    if prefix == 0 {
        return Ok(false);
    }
    let mask = u32::MAX << (32 - prefix.min(24));
    Ok(u32::from(address) & mask == u32::from(subnet) & mask)
}

impl KvmRun {
    pub async fn launch(image: &Path, worker: &Path) -> Result<Self> {
        Self::launch_named(image, worker, &["control", "agent"], false).await
    }

    pub async fn launch_mesh(image: &Path, worker: &Path) -> Result<Self> {
        Self::launch_named(image, worker, &MESH_GUESTS, false).await
    }

    pub async fn launch_two_tier_mesh(image: &Path, worker: &Path) -> Result<Self> {
        Self::launch_named(image, worker, &MESH_GUESTS, true).await
    }

    async fn launch_named(
        image: &Path,
        worker: &Path,
        names: &[&str],
        two_tier: bool,
    ) -> Result<Self> {
        ensure!(!names.is_empty(), "a KVM run requires at least one guest");
        ensure!(
            names.len() <= u8::MAX as usize - 10,
            "too many guests for the private IPv4 address range"
        );
        for (index, name) in names.iter().enumerate() {
            ensure!(
                !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
                "invalid guest name at index {index}"
            );
            ensure!(
                !names[..index].contains(name),
                "duplicate guest name: {name}"
            );
        }
        ensure!(
            image.is_file(),
            "missing guest image {}; run sh tests/kvm/build-image.sh",
            image.display()
        );
        File::options()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .context("kvm-tests requires usable /dev/kvm")?;
        memory_preflight(names, two_tier)?;
        command("ssh", &["-V"], None).await?;
        let directory = tempfile::Builder::new().prefix("podmesh-kvm-").tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let suffix = directory
            .path()
            .file_name()
            .context("run directory name")?
            .to_string_lossy();
        let suffix = suffix
            .strip_prefix("podmesh-kvm-")
            .context("run directory prefix")?;
        let namespace = format!("pmk-{suffix}");
        let host_link = format!("pmh-{suffix}");
        let mut run = Self {
            directory,
            namespace,
            host_link,
            subnet: Ipv4Addr::UNSPECIFIED,
            forced_relay_table: None,
            namespace_created: false,
            host_link_created: false,
            guests: Vec::new(),
        };
        let routes: Vec<Value> = serde_json::from_str(
            &command("ip", &["-j", "-4", "route", "show", "table", "all"], None).await?,
        )?;
        let mut subnet = None;
        for octet in 8..248 {
            let candidate = Ipv4Addr::new(172, 30, octet, 0);
            let mut collision = false;
            for route in &routes {
                if let Some(destination) = route["dst"].as_str() {
                    collision |= route_overlaps(destination, candidate)?;
                }
            }
            if !collision {
                subnet = Some(candidate);
                break;
            }
        }
        let subnet = subnet.context("no unused 172.30.x.0/24 subnet for private VM network")?;
        run.subnet = subnet;
        let octet = subnet.octets()[2];
        let gateway = format!("172.30.{octet}.1");
        run.ip(&["netns", "add", &run.namespace]).await?;
        run.namespace_created = true;
        run.ip(&[
            "link",
            "add",
            &run.host_link,
            "type",
            "veth",
            "peer",
            "name",
            "uplink",
            "netns",
            &run.namespace,
        ])
        .await?;
        run.host_link_created = true;
        run.ip(&[
            "addr",
            "add",
            &format!("{gateway}/24"),
            "dev",
            &run.host_link,
        ])
        .await?;
        run.ip(&["link", "set", &run.host_link, "up"]).await?;
        run.ip(&[
            "-n",
            &run.namespace,
            "link",
            "add",
            "mesh",
            "type",
            "bridge",
        ])
        .await?;
        run.ip(&["-n", &run.namespace, "link", "set", "mesh", "up"])
            .await?;
        run.ip(&["-n", &run.namespace, "link", "set", "lo", "up"])
            .await?;
        run.ip(&[
            "-n",
            &run.namespace,
            "link",
            "set",
            "uplink",
            "master",
            "mesh",
        ])
        .await?;
        run.ip(&["-n", &run.namespace, "link", "set", "uplink", "up"])
            .await?;
        let identity = run.directory.path().join("identity");
        command(
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                path_string(&identity)?,
            ],
            None,
        )
        .await?;
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600))?;
        for (index, name) in names.iter().copied().enumerate() {
            let guest_dir = run.directory.path().join(name);
            fs::create_dir(&guest_dir)?;
            let disk = guest_dir.join("root.raw");
            command(
                "cp",
                &[
                    "--reflink=auto",
                    "--sparse=always",
                    path_string(image)?,
                    path_string(&disk)?,
                ],
                None,
            )
            .await?;
            let config_dir = guest_dir.join("config");
            fs::create_dir(&config_dir)?;
            let address = Ipv4Addr::new(172, 30, octet, 10 + index as u8);
            fs::write(
                config_dir.join("guest.env"),
                format!("VM_NAME={name}\nVM_ADDRESS={address}\nVM_GATEWAY={gateway}\n"),
            )?;
            fs::copy(
                identity.with_extension("pub"),
                config_dir.join("authorized_keys"),
            )?;
            let config = guest_dir.join("config.raw");
            File::create(&config)?.set_len(16 * 1024 * 1024)?;
            command(
                "mkfs.ext4",
                &[
                    "-q",
                    "-F",
                    "-d",
                    path_string(&config_dir)?,
                    path_string(&config)?,
                ],
                None,
            )
            .await?;
            let tap = format!("tap{index}");
            run.ip(&[
                "netns",
                "exec",
                &run.namespace,
                "ip",
                "tuntap",
                "add",
                &tap,
                "mode",
                "tap",
            ])
            .await?;
            run.ip(&["-n", &run.namespace, "link", "set", &tap, "master", "mesh"])
                .await?;
            run.ip(&["-n", &run.namespace, "link", "set", &tap, "up"])
                .await?;
            let console = guest_dir.join("console.log");
            let output = File::create(&console)?;
            let child = Command::new("ip")
                .args(["netns", "exec", &run.namespace])
                .arg(worker)
                .arg("worker")
                .arg(&disk)
                .arg(&config)
                .arg(&tap)
                .arg(index.to_string())
                .arg(guest_memory_mib(name, two_tier).to_string())
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()
                .context("spawn isolated libkrun worker")?;
            run.guests.push(Guest {
                name: name.to_string(),
                address,
                console,
                child,
            });
        }
        for index in 0..run.guests.len() {
            run.wait_ready(index).await?;
        }
        Ok(run)
    }

    async fn ip(&self, args: &[&str]) -> Result<()> {
        command("ip", args, None)
            .await
            .context("configure run-owned VM network")?;
        Ok(())
    }

    pub async fn force_relay(&mut self) -> Result<()> {
        ensure!(
            self.forced_relay_table.is_none(),
            "forced-relay filter is already installed"
        );
        let table = format!("pmf_{}", self.namespace.trim_start_matches("pmk-"));
        let subnet = format!("{}/24", self.subnet);
        self.ip(&[
            "netns",
            "exec",
            &self.namespace,
            "nft",
            "add",
            "table",
            "bridge",
            &table,
        ])
        .await?;
        self.ip(&[
            "netns",
            "exec",
            &self.namespace,
            "nft",
            "add",
            "chain",
            "bridge",
            &table,
            "forward",
            "{ type filter hook forward priority -200; policy accept; }",
        ])
        .await?;
        for expression in [
            format!(
                "ip saddr {subnet} ip daddr {subnet} udp dport {{ 7840, 7850 }} counter accept"
            ),
            format!(
                "ip saddr {subnet} ip daddr {subnet} udp sport {{ 7840, 7850 }} counter accept"
            ),
            format!(
                "ip saddr {subnet} ip daddr {subnet} tcp dport {{ 7440, 7450 }} counter accept"
            ),
            format!(
                "ip saddr {subnet} ip daddr {subnet} tcp sport {{ 7440, 7450 }} counter accept"
            ),
        ] {
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "add",
                "rule",
                "bridge",
                &table,
                "forward",
                &expression,
            ])
            .await?;
        }
        let expression =
            format!("ip saddr {subnet} ip daddr {subnet} ip protocol udp counter drop");
        self.ip(&[
            "netns",
            "exec",
            &self.namespace,
            "nft",
            "add",
            "rule",
            "bridge",
            &table,
            "forward",
            &expression,
        ])
        .await?;
        self.forced_relay_table = Some(table);
        Ok(())
    }

    pub async fn allow_agent_bootstrap(&self, enabled: bool) -> Result<()> {
        let table = self
            .forced_relay_table
            .as_deref()
            .context("missing relay filter")?;
        if enabled {
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "add",
                "chain",
                "bridge",
                table,
                "bootstrap",
            ])
            .await?;
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "insert",
                "rule",
                "bridge",
                table,
                "forward",
                "jump bootstrap",
            ])
            .await?;
            for guest in self
                .guests
                .iter()
                .filter(|guest| guest.name.starts_with("agent-"))
            {
                for expression in [
                    format!("ip saddr {} udp dport 5001 accept", guest.address),
                    format!("ip daddr {} udp sport 5001 accept", guest.address),
                ] {
                    self.ip(&[
                        "netns",
                        "exec",
                        &self.namespace,
                        "nft",
                        "add",
                        "rule",
                        "bridge",
                        table,
                        "bootstrap",
                        &expression,
                    ])
                    .await?;
                }
            }
        } else {
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "flush",
                "chain",
                "bridge",
                table,
                "bootstrap",
            ])
            .await?;
        }
        Ok(())
    }

    pub async fn forced_relay_dropped_packets(&self) -> Result<u64> {
        Ok(self.forced_relay_path_counters().await?.1)
    }

    pub async fn forced_relay_path_counters(&self) -> Result<(u64, u64)> {
        let table = self
            .forced_relay_table
            .as_deref()
            .context("forced-relay filter is not installed")?;
        let output = command(
            "ip",
            &[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "-nn",
                "list",
                "chain",
                "bridge",
                table,
                "forward",
            ],
            None,
        )
        .await?;
        let parse = |line: &str| {
            line.split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .find(|pair| pair[0] == "packets")
                .and_then(|pair| pair[1].parse::<u64>().ok())
        };
        let mut allowed = 0;
        let mut dropped = None;
        for line in output
            .lines()
            .filter(|line| line.contains("counter packets"))
        {
            let packets = parse(line).context("forced-relay packet counter is malformed")?;
            if line.trim_end().ends_with("drop") {
                dropped = Some(packets);
            } else if line.trim_end().ends_with("accept") {
                allowed += packets;
            }
        }
        Ok((
            allowed,
            dropped.context("forced-relay drop counter is missing")?,
        ))
    }

    pub async fn block_relays(&self, blocked: bool) -> Result<()> {
        let table = self
            .forced_relay_table
            .as_deref()
            .context("missing relay filter")?;
        if blocked {
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "add",
                "chain",
                "bridge",
                table,
                "relay_stop",
            ])
            .await?;
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "insert",
                "rule",
                "bridge",
                table,
                "forward",
                "jump relay_stop",
            ])
            .await?;
            for expression in [
                "tcp dport { 7440, 7450 } counter drop",
                "tcp sport { 7440, 7450 } counter drop",
            ] {
                self.ip(&[
                    "netns",
                    "exec",
                    &self.namespace,
                    "nft",
                    "add",
                    "rule",
                    "bridge",
                    table,
                    "relay_stop",
                    expression,
                ])
                .await?;
            }
        } else {
            self.ip(&[
                "netns",
                "exec",
                &self.namespace,
                "nft",
                "flush",
                "chain",
                "bridge",
                table,
                "relay_stop",
            ])
            .await?;
        }
        Ok(())
    }

    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    async fn wait_ready(&mut self, index: usize) -> Result<()> {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            let guest = &mut self.guests[index];
            if let Some(status) = guest.child.try_wait()? {
                bail!(
                    "{} worker exited {status}; console: {}",
                    guest.name,
                    console_tail(&guest.console)?
                );
            }
            ensure!(
                Instant::now() < deadline,
                "{} boot deadline exceeded; console: {}",
                guest.name,
                console_tail(&guest.console)?
            );
            let console = fs::read_to_string(&guest.console)?;
            ensure!(
                console.len() <= MAX_OUTPUT,
                "{} console exceeded limit",
                guest.name
            );
            if let Some(key) = console
                .lines()
                .find_map(|line| line.strip_prefix("PODMESH_SSH_HOST_KEY="))
            {
                if !console.ends_with('\n')
                    && console
                        .lines()
                        .last()
                        .is_some_and(|line| line.starts_with("PODMESH_SSH_HOST_KEY="))
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let mut fields = key.split_whitespace();
                ensure!(
                    fields.next() == Some("ssh-ed25519"),
                    "{} emitted an unexpected guest SSH host key type",
                    guest.name
                );
                let encoded = fields.next().context("guest SSH key missing")?;
                let decoded =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)?;
                ensure!(decoded.len() == 51, "unexpected ed25519 host-key length");
                let known = self.directory.path().join(format!("known-hosts-{index}"));
                fs::write(known, format!("{} ssh-ed25519 {encoded}\n", guest.address))?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut last_error = None;
        while Instant::now() < deadline {
            match self.ssh(index, "true", None).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error.get_or_insert(error);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(last_error.unwrap_or_else(|| anyhow!("SSH readiness deadline exceeded"))).with_context(
            || {
                format!(
                    "{} SSH deadline; console: {}",
                    self.guests[index].name,
                    console_tail(&self.guests[index].console)
                        .unwrap_or_else(|error| format!("console read failed: {error}"))
                )
            },
        )
    }

    pub async fn ssh(&self, guest: usize, script: &str, input: Option<&[u8]>) -> Result<String> {
        let guest_ref = self.guests.get(guest).context("unknown guest index")?;
        let identity = self.directory.path().join("identity");
        let known = self.directory.path().join(format!("known-hosts-{guest}"));
        command(
            "ssh",
            &[
                "-F",
                "/dev/null",
                "-i",
                path_string(&identity)?,
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=3",
                "-o",
                &format!("UserKnownHostsFile={}", known.display()),
                &format!("root@{}", guest_ref.address),
                &format!("set -eu; {script}"),
            ],
            input,
        )
        .await
        .with_context(|| format!("guest {} command", guest_ref.name))
    }

    pub async fn put(&self, guest: usize, destination: &str, content: &[u8]) -> Result<()> {
        ensure!(
            destination.starts_with("/opt/podmesh/")
                && destination
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
            "invalid component destination"
        );
        self.ssh(guest, &format!("cat > {destination}.new && chmod 755 {destination}.new && mv {destination}.new {destination}"), Some(content)).await?;
        Ok(())
    }

    pub async fn restart_role(&self, guest: usize, role: &str) -> Result<()> {
        ensure!(
            ["scheduler", "agent", "proxy"].contains(&role),
            "unknown role"
        );
        self.ssh(guest, &format!("pid=$(cat /run/podmesh-{role}.pid); exe=$(readlink /proc/$pid/exe); case \"$exe\" in /opt/podmesh/podmesh-{role}|\"/opt/podmesh/podmesh-{role} (deleted)\") ;; *) exit 1 ;; esac; kill -TERM \"$pid\""), None).await?;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        let mut running = Vec::new();
        for index in 0..self.guests.len() {
            if self.guests[index].child.try_wait()?.is_none() {
                let guest = &self.guests[index];
                match fs::read_to_string(format!("/proc/{}/status", guest.child.id())).and_then(
                    |status| proc_memory_kib(&status, "VmHWM:").map_err(std::io::Error::other),
                ) {
                    Ok(peak) => println!(
                        "KVM memory: {} worker peak RSS {} MiB (not simultaneous total)",
                        guest.name,
                        peak / 1024
                    ),
                    Err(error) => log::warn!("{} worker memory diagnostics: {error}", guest.name),
                }
                running.push(index);
            }
        }
        let run = &*self;
        futures::future::join_all(running.into_iter().map(|index| async move {
            if let Err(error) = run.ssh(index, "sync", None).await {
                log::warn!("{} shutdown sync: {error:#}", run.guests[index].name);
            }
            // Poweroff stops the guest before SSH can send its exit status.
            match tokio::time::timeout(SHUTDOWN_ACK_TIMEOUT, run.ssh(index, "poweroff -f", None))
                .await
            {
                Ok(Ok(_)) => (),
                Ok(Err(error)) => log::debug!(
                    "{} shutdown acknowledgement: {error:#}",
                    run.guests[index].name
                ),
                Err(_) => log::debug!(
                    "{} poweroff SSH acknowledgement deadline; checking worker exit",
                    run.guests[index].name
                ),
            }
        }))
        .await;
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        for guest in &mut self.guests {
            while guest.child.try_wait()?.is_none() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if guest.child.try_wait()?.is_none() {
                log::warn!(
                    "{} worker did not exit after guest poweroff; terminating owned PID {}",
                    guest.name,
                    guest.child.id()
                );
                guest.child.kill()?;
            }
            guest.child.wait()?;
        }
        self.remove_network()?;
        Ok(())
    }

    fn remove_network(&mut self) -> Result<()> {
        if self.host_link_created {
            cleanup_command(&["link", "delete", &self.host_link])?;
            self.host_link_created = false;
        }
        if self.namespace_created {
            cleanup_command(&["netns", "delete", &self.namespace])?;
            self.namespace_created = false;
        }
        Ok(())
    }

    pub fn allocated_disk_bytes(&self) -> Result<u64> {
        self.guests.iter().try_fold(0, |sum, guest| {
            let disk = self.directory.path().join(&guest.name).join("root.raw");
            Ok(sum + fs::metadata(disk)?.blocks() * 512)
        })
    }
}

fn cleanup_command(args: &[&str]) -> Result<()> {
    let status = Command::new("timeout")
        .args(["10", "ip"])
        .args(args)
        .status()?;
    ensure!(
        status.success(),
        "run-owned network cleanup failed: {args:?}"
    );
    Ok(())
}

impl Drop for KvmRun {
    fn drop(&mut self) {
        for guest in &mut self.guests {
            match guest.child.try_wait() {
                Ok(Some(_)) => (),
                Ok(None) => {
                    if let Err(error) = guest.child.kill() {
                        log::error!("kill {} worker: {error}", guest.name);
                    }
                    if let Err(error) = guest.child.wait() {
                        log::error!("reap {} worker: {error}", guest.name);
                    }
                }
                Err(error) => log::error!("check {} worker: {error}", guest.name),
            }
        }
        if let Err(error) = self.remove_network() {
            log::error!("KVM run cleanup: {error:#}");
        }
    }
}

fn console_tail(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn worker(disk: &Path, config: &Path, tap: &str, index: u8, memory_mib: u32) -> Result<()> {
    ensure!(
        matches!(memory_mib, 256 | 512 | 1024),
        "unsupported lean guest memory budget"
    );
    let disk = CString::new(path_string(disk)?)?;
    let config = CString::new(path_string(config)?)?;
    let tap = CString::new(tap)?;
    let mac = [0x02, 0x50, 0x4d, 0x00, 0x00, index];
    let env = [
        CString::new("KRUN_INIT_PID1=1")?,
        CString::new("PATH=/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")?,
    ];
    let envp = [env[0].as_ptr(), env[1].as_ptr(), std::ptr::null()];
    let argv = [std::ptr::null::<c_char>()];
    // Signatures match the pinned 1.19.6 header. Strings, arrays and the library
    // remain alive until the blocking worker exits; never call this in the supervisor.
    unsafe {
        let library = Library::new("libkrun.so.1").context(
            "kvm-tests requires libkrun 1.19.6 with NET/BLK/init-blob and libkrunfw 5.6.2",
        )?;
        let create = library.get::<unsafe extern "C" fn() -> i32>(b"krun_create_ctx\0")?;
        let ctx = create();
        ensure!(ctx >= 0, "krun_create_ctx failed: {ctx}");
        let ctx = u32::try_from(ctx)?;
        let configure =
            library.get::<unsafe extern "C" fn(u32, u8, u32) -> i32>(b"krun_set_vm_config\0")?;
        check_krun(
            "VM configuration",
            configure(ctx, if memory_mib == 1024 { 2 } else { 1 }, memory_mib),
        )?;
        let add_disk =
            library.get::<unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32>(
                b"krun_add_disk\0",
            )?;
        check_krun(
            "root disk",
            add_disk(ctx, c"root".as_ptr(), disk.as_ptr(), false),
        )?;
        check_krun(
            "config disk",
            add_disk(ctx, c"config".as_ptr(), config.as_ptr(), true),
        )?;
        let remount = library.get::<unsafe extern "C" fn(
            u32,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> i32>(b"krun_set_root_disk_remount\0")?;
        check_krun(
            "guest-local root",
            remount(
                ctx,
                c"/dev/vda".as_ptr(),
                c"ext4".as_ptr(),
                std::ptr::null(),
            ),
        )?;
        let net = library
            .get::<unsafe extern "C" fn(u32, *const c_char, *const u8, u32, u32) -> i32>(
                b"krun_add_net_tap\0",
            )?;
        check_krun("TAP networking", net(ctx, tap.as_ptr(), mac.as_ptr(), 0, 0))?;
        let exec = library.get::<unsafe extern "C" fn(
            u32,
            *const c_char,
            *const *const c_char,
            *const *const c_char,
        ) -> i32>(b"krun_set_exec\0")?;
        check_krun(
            "guest init",
            exec(ctx, c"/sbin/init".as_ptr(), argv.as_ptr(), envp.as_ptr()),
        )?;
        let enter = library.get::<unsafe extern "C" fn(u32) -> i32>(b"krun_start_enter\0")?;
        check_krun("VM execution", enter(ctx))?;
        bail!("libkrun execution returned unexpectedly");
    }
}

fn check_krun(operation: &str, result: i32) -> Result<()> {
    ensure!(
        result == 0,
        "libkrun {operation}: {}",
        std::io::Error::from_raw_os_error(-result)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lean_profiles_and_memory_preflight_bounds() -> Result<()> {
        assert_eq!(guest_memory_mib("control", false), 512);
        assert_eq!(guest_memory_mib("agent", false), 512);
        let total: u64 = MESH_GUESTS
            .iter()
            .map(|name| u64::from(guest_memory_mib(name, false)))
            .sum();
        assert_eq!(total, 2816);
        assert_eq!(guest_memory_mib("agent-1", true), 1024);
        assert_eq!(guest_memory_mib("scheduler-1", true), 256);
        assert_eq!(
            MESH_GUESTS
                .iter()
                .map(|name| u64::from(guest_memory_mib(name, true)))
                .sum::<u64>(),
            4352
        );
        assert!(check_memory_budget((total + 1024) * MIB, total).is_ok());
        assert!(check_memory_budget((total + 1024) * MIB - 1, total).is_err());
        assert_eq!(
            proc_memory_kib("MemAvailable: 1234 kB\n", "MemAvailable:")?,
            1234
        );
        assert!(proc_memory_kib("MemAvailable: 1234 MB\n", "MemAvailable:").is_err());
        assert!(proc_memory_kib("", "MemAvailable:").is_err());
        Ok(())
    }

    #[test]
    fn route_collision_checks_include_broader_and_narrower_routes() -> Result<()> {
        let subnet = Ipv4Addr::new(172, 30, 8, 0);
        for route in [
            "172.30.0.0/16",
            "172.30.8.0/24",
            "172.30.8.128/25",
            "172.30.8.10",
        ] {
            assert!(route_overlaps(route, subnet)?, "{route}");
        }
        for route in [
            "default",
            "0.0.0.0/0",
            "172.30.9.0/24",
            "10.0.0.0/8",
            "::/0",
        ] {
            assert!(!route_overlaps(route, subnet)?, "{route}");
        }
        assert!(route_overlaps("172.30.8.0/33", subnet).is_err());
        Ok(())
    }

    #[test]
    fn mesh_topology_has_eight_guests_and_exact_uneven_attachments() {
        assert_eq!(MESH_GUESTS.len(), 8);
        assert_eq!(
            MESH_ATTACHMENTS,
            [
                ("scheduler-1", &[][..]),
                ("scheduler-2", &["agent-1", "agent-2"][..]),
                ("scheduler-3", &["agent-3"][..]),
            ]
        );
        for (index, guest) in MESH_GUESTS.iter().enumerate() {
            assert!(!MESH_GUESTS[..index].contains(guest));
        }
        for (scheduler, agents) in MESH_ATTACHMENTS {
            assert!(MESH_GUESTS.contains(&scheduler));
            for agent in agents {
                assert!(MESH_GUESTS.contains(agent));
                assert!(agent.starts_with("agent-"));
            }
        }
    }

    #[test]
    fn libkrun_errors_remain_failures() {
        assert!(check_krun("test", 0).is_ok());
        assert!(
            check_krun("test", -22)
                .unwrap_err()
                .to_string()
                .contains("Invalid argument")
        );
    }
}
