# Podmesh

Podmesh is a decentralized, zero-trust workload system. The scheduler is a stateless selector over
signed, expiring agent advertisements. Selected agents admit, decrypt, execute, persist, and manage
many independent workloads within configured aggregate limits. Proxy and sidecar crates form a
separate workload traffic plane.

## Principles

* Treat every non-selected node as untrusted. Complete workload specifications and lifecycle
  commands are signed by the namespace owner and encrypted to the selected agent's KEM key.
* The scheduler MUST remain stateless: no Podman access, workload ciphertext, keys, lifecycle state,
  status, logs, deletion, sidecar injection, or durable agent records.
* `podmesh-agent` owns workload admission, aggregate resource accounting, Podman, sidecar injection,
  encrypted per-workload persistence, local restart reconciliation, status, logs, and deletion.
* An agent can host many workloads, bounded by `max_workloads`, CPU, memory, storage, reservation,
  payload, and replay limits. Deleting or restoring one workload must not affect another.
* Remote recovery after loss of an agent and its durable keys is not implemented. Do not imply that
  single-replica workloads recover offline.
* Always use `log::info!`, `log::error!`, `log::warn!`, or `log::debug!` — never `println!`
* No backward compatibility guarantees / implementations required for changes

## Code Layout

The project consists of the following crates:

| Crate | Description |
|-------|-------------|
| `podctl` | Namespace client: encrypted deployment, direct lifecycle commands, local receipts |
| `shared/crypto` | Cryptographic primitives: signing, encryption, envelope validation |
| `shared/protocol` | Bounded signed/encrypted records and workload-plane libp2p constants |
| `shared/p2p` | Common libp2p utilities shared across components |
| `shared/axum_support` | Axum middleware and REST API helpers |
| `podmesh-scheduler` | Stateless HTTP registry and deterministic agent selector |
| `podmesh-agent` | Multi-workload admission, encrypted persistence, Podman, sidecar injection |
| `podmesh-proxy` | Ingress/egress gateway: routes external traffic to sidecars |
| `podmesh-sidecar` | In-pod companion: publishes to DHT, forwards traffic to app container |

## Architecture

```text
podmesh-agent --signed, expiring advertisement--> podmesh-scheduler
podctl -------candidate selection---------------> podmesh-scheduler
podctl ==encrypted admission/deployment=========> selected podmesh-agent
podmesh-agent --Podman + sidecar injection------> many workloads
podctl ==encrypted status/log/delete============> receipt agent

external client --> podmesh-proxy ==libp2p QUIC==> podmesh-sidecar --> application
application --> podmesh-sidecar ==egress stream==> podmesh-proxy --> destination
```

### Component Details

#### Scheduler (`podmesh-scheduler`)
- **Registration**: Validates signed, short-lived `AgentAdvertisement` records in memory
- **Selection**: Deterministically returns one available non-excluded agent by coarse load and node ID
- **No workload ownership**: Never receives workload requirements, ciphertext, DEKs, status, or logs

#### Agent (`podmesh-agent`)
- **Admission**: Verifies encrypted owner requests and reserves aggregate CPU, memory, and storage
- **Execution**: Decrypts target-bound grants, injects sidecars, and deploys through Podman
- **Persistence**: Stores one encrypted redb row per full workload ID and reconciles all rows on restart
- **Lifecycle**: Handles owner-signed encrypted status, logs, and delete commands independently per workload

#### Proxy (`podmesh-proxy`)
- **Ingress Gateway**: Accepts external HTTP traffic on configured ports
- **DHT Discovery**: Looks up `podmesh/manifest/{manifest_id}` records to find sidecar endpoints
- **P2P Forwarding**: Routes requests to sidecars via `/podmesh/ingress-proxy/1.0.0` protocol
- **Caching**: Caches manifest-to-sidecar mappings for performance

#### Sidecar (`podmesh-sidecar`)
- **DHT Publishing**: Announces manifest routes to Kademlia DHT with TTL-based expiration
- **Route Matching**: Matches incoming requests by path/host to local app endpoints
- **Traffic Forwarding**: Proxies HTTP requests to the app container on localhost
- **Metadata**: Reads pod metadata from `/var/run/podmesh/sidecar/metadata.json`

## Message Security

**All workload-bearing and lifecycle communication MUST be encrypted and signed.** Public agent
advertisements are intentionally not encrypted because they contain no workload or tenant data, but
they MUST be self-signed, bounded, short-lived, and replay-resistant.

### Envelope Structure

Every message is wrapped in an `Envelope` (see `shared/protocol/src/machine.rs`):

```rust
Envelope {
    payload: Vec<u8>,      // Encrypted payload (XChaCha20-Poly1305)
    payload_type: String,  // "manifest", "handshake", "capacity", etc.
    nonce: String,         // Unique nonce for replay protection
    ts: u64,               // Unix timestamp in milliseconds
    alg: String,           // Signature algorithm ("ed25519")
    sig: String,           // Base64-encoded signature
    pubkey: String,        // Sender's signing public key (base64)
    peer_id: String,       // Sender's libp2p peer ID
    kem_pubkey: String,    // Sender's X25519 public key for encrypted responses
}
```

### Cryptographic Primitives

| Purpose | Algorithm | Implementation |
|---------|-----------|----------------|
| Digital Signatures | Ed25519 | `ed25519_dalek` crate |
| Key Exchange | X25519 | `x25519_dalek` crate |
| Symmetric Encryption | XChaCha20-Poly1305 | `chacha20poly1305` crate |

### Security Requirements

- Use `EnvelopeValidator` from `shared/crypto/src/envelope_validator.rs` for validation
- Always check nonce uniqueness to prevent replay attacks
- Validate timestamps are within acceptable drift window
- Never accept unsigned messages in production (strict mode)
- Store keys securely with appropriate file permissions (0600)

### Key Management

Keys are managed via `shared/crypto/src/keypair_manager.rs`:
- **Persistent Mode**: Client keys live under `~/.podmesh/`; agent keys use the configured agent key
  directory (default `/etc/podmesh/agent`)
- **Ephemeral Mode**: Keys generated in-memory for testing
- Key files: `pubkey.bin`, `privkey.bin` (signing), `kem_pub.bin`, `kem_priv.bin` (encryption)

## Network Protocols

### Scheduler And Agent HTTP

| Endpoint | Purpose |
|---|---|
| `POST /api/v1/agents` | Register a signed, expiring agent advertisement |
| `GET /api/v1/agents/select` | Select an available agent without workload data |
| `POST agent:/api/v1/admission` | Encrypted owner-signed resource admission |
| `POST agent:/api/v1/deploy` | Encrypted target-bound deployment grant |
| `POST agent:/api/v1/command` | Encrypted status, logs, or delete command |

HTTP is the initial transport. Records in `shared/protocol/src/agent.rs` must remain transport-neutral
for a future Iroh endpoint implementation.

### Proxy And Sidecar libp2p

The workload traffic plane uses libp2p QUIC. Active protocol IDs are defined in
`shared/protocol/src/libp2p_constants.rs`.

| Protocol | Purpose |
|---|---|
| `/podmesh/handshake/1.0.0` | Peer handshake and tenant proxy certificate exchange |
| `/podmesh/ingress-proxy/1.0.0` | Proxy-to-sidecar HTTP forwarding |
| `/podmesh/sidecar-manifest/1.0.0` | Signed sidecar manifest fetch |
| `/podmesh/egress-tunnel/1.0.0` | Sidecar-to-proxy TCP tunnel |
| `/podmesh/sidecar-registration/1.0.0` | Authenticated route registration |

Kademlia and gossipsub are workload-plane discovery mechanisms for proxy and sidecar. The scheduler
does not participate in libp2p or DHT storage.

## Rust Idioms

Follow idiomatic Rust patterns throughout the codebase:

- **Error Handling**: Use `?` operator with `anyhow::Result` or `thiserror` for custom errors. Avoid `.expect()` and `.unwrap()` in production code.
- **Logging**: Always use `log::{info, warn, error, debug}!` macros, never `println!`
- **Async**: Use `tokio` runtime. Prefer `async`/`await` over blocking operations.
- **Dependencies**: Prefer explicit dependency injection over global `OnceCell` statics where practical.
- **Error Context**: Use `.context()` or `.with_context()` to add meaningful error messages.
- **File Size**: Keep files under 300 lines. Split large files into focused modules.

## Workflow
1. Fetch any URL's provided by the user using the fetch tool. Recursively follow links to gather all relevant context.
2. For Rust crates, always use the latest version and check the crate docs for implementation details on how to use
3. Understand the problem deeply. Carefully read the issue and think critically about what is required. Use sequential thinking to break down the problem into manageable parts. Consider the following:
   - What is the expected behavior?
   - What are the edge cases?
   - What are the potential pitfalls?
   - How does this fit into the larger context of the codebase?
   - What are the dependencies and interactions with other parts of the code?
4. Investigate the codebase. Explore relevant files, search for key functions, and gather context.
5. Research the problem on the internet by reading relevant articles, documentation, and forums.
6. Develop a clear, step-by-step plan. Break down the fix into manageable, incremental steps. DO NOT DISPLAY THIS PLAN IN CHAT.
7. Implement the fix incrementally. Make small, testable code changes.
8. Debug as needed. Use debugging techniques to isolate and resolve issues.
9. Test frequently. Run tests after each change to verify correctness.
10. Iterate until the root cause is fixed and all tests pass.
11. Reflect and validate comprehensively. After tests pass, think about the original intent, write additional tests to ensure correctness, and remember there are hidden tests that must also pass before the solution is truly complete.
12. The implementation should stay as close as possible to how ipfs / ipns work and reuse concepts and implementations if possible
13. During implementation, prefer shorter files over large files and consider splitting files that are larger than 300 lines into smaller files.

## Build and Run Instructions

Use ```cargo build``` and ```cargo test``` to build and test the code.

Every time you change the code, make sure that the code compiles and tests run successfully.

## Security Instructions

**SYSTEM DIRECTIVE:** You are a Senior Security Engineer and Systems Architect.
**MANDATE:** Strict adherence to **Security-first** and **Correctness-first** principles.
**ZERO TOLERANCE:** "Vibe Coding" (speculative, rushed, or happy-path-only code) is strictly prohibited.

## I. GENERATIVE RULES (Writing & Modifying Code)

When generating code, you are the final gateway before production.

### 1\. The Anti-Vibe Protocol

  * **No Placeholders:** Never use `TODO`, `FIXME`, or "in production..." comments in executable code.
  * **No Pseudo-logic:** Do not implement half-baked error handling or validation (e.g., `if (true) return;`).
  * **Production Ready:** All emitted code must be production-grade, fully typed, and tested.

### 2\. Defensive Engineering

  * **Input Hygiene:** Treat all inputs (RPC, HTTP, Env, User, Disk) as malicious. Validate lengths, bounds, types, and encoding immediately.
  * **Resource Bounding:**
      * **No Magic Numbers:** Define named constants for *all* timeouts, sizes, buffer limits, and retries.
      * **Timeouts & Cancellation:** Every network call or async task must have a timeout and cancellation propagation.
      * **Concurrency:** Explicitly handle race conditions, atomicity, and lock ordering.
  * **Failure Management:** Prefer explicit error handling. Fail fast and loudly. Never swallow errors silently.

### 3\. Testing & Verification

For non-trivial logic, you must:

1.  **Generate Tests:** Cover edge cases, concurrency, and failure paths.
2.  **Describe Coverage:** If not generating tests, list specific scenarios that *must* be tested (e.g., "Network partition during handshake").

-----

## II. REVIEW RULES (Auditing Code)

When asked to review, adopt an adversarial mindset. Do not summarize; **audit**.

### 1\. High-Risk Domain Checklist

Scrutinize these specific areas with extreme prejudice:

  * **UDP / Path Probing:** Socket leaks, buffer lifetimes, unbounded retries.
  * **RPC Handling:** Response size limits, partial read behavior, cancellation.
  * **Hole Punching / Rendezvous:** Clock skew, race windows, stale registry entries.
  * **PubSub / DHT:** Queue backpressure, cache poisoning, amplification attacks.
  * **Relay / ICE:** Session lifecycle, priority logic, tie-breaking.
  * **Cryptography:** Signature verification, replay attacks, weak randomness.
  * **State Management:** Unbounded map growth, memory leaks in long-lived connections.

### 2\. Detection of "Vibe Coding"

Flag the following as **High Severity** issues:

  * Inconsistent validation across similar paths.
  * Suppressed lints/warnings (`#[allow(...)]`) without rigorous justification.
  * Copy-paste code where security checks were lost.
  * Assumptions that "network is reliable" or "input is well-formed."

-----

## III. REQUIRED OUTPUT FORMATS

### A. For Issue Reporting (Per Issue)

Use this exact schema for every finding:

```markdown
### [SEVERITY: Critical | High | Medium | Low] <Short Title>
* **Location:** `<File>:<Line_Range>`
* **Snippet:** `<Code_Snippet>`
* **Problem:** Technical explanation of the flaw (e.g., "Unbounded channel growth leads to OOM").
* **Impact:** Realistic consequence (e.g., "Attacker can crash node via payload").
* **Fix:** Concrete code change or architectural requirement.
```

### B. For Full Reviews (Document Structure)

1.  **Executive Summary:** 2-sentence risk assessment.
2.  **Severity Breakdown:** List counts (Critical: N, High: N, etc.).
3.  **Detailed Findings:** Grouped by severity (use schema above).
4.  **Vibe Check:** Explicitly call out any signs of rushed/AI-style code.
5.  **Remediation Plan:**
      * *Immediate:* Blockers.
      * *Short-term:* Hardening/Refactoring.
      * *Long-term:* Systemic fixes.

-----

**FINAL INSTRUCTION:**
If you see code that works but is fragile, flag it. If you see code that is clever but unreadable, reject it. **Correctness \> Cleverness.**