# Podmesh

Podmesh is a decentralized, lock-free orchestration system that turns any device into an interchangeable compute resource through a decentralized scheduler. The workload plane is strictly focusing on scheduling and running workloads, it does not contain any service mesh like behavior.

## Principles

* The solution prioritizes decentralization through libp2p and zero-trust by encrypting and signing all communication and encrypting with the receivers kem public key through functions in the crypto crate.
* The scheduler listen should check for node failure and restart crashed containers and reschedule workloads on failed nodes, but does not handle any workload communication as this is handled by the scheduler plane (strict segregation and zero trust, the workload plane does not trust the scheduler plane).
* Always use `log::info!`, `log::error!`, `log::warn!`, or `log::debug!` — never `println!`
* No backward compatibility guarantees / implementations required for changes

## Code Layout

The project consists of the following crates:

| Crate | Description |
|-------|-------------|
| `podctl` | CLI tool (similar to kubectl) for interacting with podmesh |
| `shared/crypto` | Cryptographic primitives: signing, encryption, envelope validation |
| `shared/protocol` | Postcard-serialized message types and libp2p constants |
| `shared/p2p` | Common libp2p utilities shared across components |
| `shared/axum_support` | Axum middleware and REST API helpers |
| `podmesh-scheduler` | Core scheduler node: workload lifecycle, P2P networking, REST API |
| `podmesh-proxy` | Ingress/egress gateway: routes external traffic to sidecars |
| `podmesh-sidecar` | In-pod companion: publishes to DHT, forwards traffic to app container |

## Architecture

```
┌─────────────────┐     ┌─────────────────────────────────────────────┐
│   podctl CLI    │────▶│  Bootstrap Scheduler (podmesh-scheduler)    │
│   (apply/delete)│     │  - REST API on port 3000                    │
└─────────────────┘     │  - Gossipsub capacity queries               │
                        │  - Coordinates workload placement           │
                        └───────────────┬─────────────────────────────┘
                                        │ libp2p (QUIC)
                        ┌───────────────▼─────────────────────────────┐
                        │  Worker Scheduler (podmesh-scheduler)       │
                        │  - Deploys pods via Podman                  │
                        │  - Announces as provider in Kademlia DHT    │
                        │  - Injects sidecar into each pod            │
                        └───────────────┬─────────────────────────────┘
                                        │
              ┌─────────────────────────┼─────────────────────────────┐
              │                         │                             │
              ▼                         ▼                             ▼
┌─────────────────────┐   ┌─────────────────────┐   ┌─────────────────────┐
│  Proxy              │   │  Pod                │   │  Pod                │
│  (podmesh-proxy)    │   │  ┌───────────────┐  │   │  ┌───────────────┐  │
│  - Ingress gateway  │◀──│  │ Sidecar       │  │   │  │ Sidecar       │  │
│  - DHT lookup for   │   │  │ - DHT publish │  │   │  │ - DHT publish │  │
│    manifest routes  │   │  │ - Route match │  │   │  │ - Route match │  │
│  - P2P to sidecars  │   │  └───────┬───────┘  │   │  └───────┬───────┘  │
└─────────────────────┘   │          │          │   │          │          │
                          │  ┌───────▼───────┐  │   │  ┌───────▼───────┐  │
                          │  │ App Container │  │   │  │ App Container │  │
                          │  └───────────────┘  │   │  └───────────────┘  │
                          └─────────────────────┘   └─────────────────────┘
```

### Component Details

#### Scheduler (`podmesh-scheduler`)
- **Bootstrap Mode**: Receives apply/delete requests from CLI, broadcasts capacity queries via gossipsub, selects worker nodes, forwards encrypted manifests
- **Worker Mode**: Responds to capacity queries, deploys pods via Podman runtime, injects sidecars, announces as provider in Kademlia DHT
- **REST API**: Exposes endpoints for CLI interaction (apply, delete, status)
- **Failure Recovery**: Monitors pod health, restarts crashed containers, reschedules on node failure

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

**All inter-node communication MUST be encrypted and signed.** The system uses proven cryptographic primitives.

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
- **Persistent Mode**: Keys stored in `~/.podmesh/` or `/etc/podmesh/machine/`
- **Ephemeral Mode**: Keys generated in-memory for testing
- Key files: `pubkey.bin`, `privkey.bin` (signing), `kem_pub.bin`, `kem_priv.bin` (encryption)

## P2P Protocols

The system uses libp2p with QUIC transport. All protocols are defined in `shared/protocol/src/libp2p_constants.rs`.

### Request-Response Protocols

| Protocol | Purpose | Used By |
|----------|---------|---------|
| `/podmesh/apply/1.0.0` | Deploy workload manifests | Scheduler ⟷ Scheduler |
| `/podmesh/delete/1.0.0` | Delete workloads | Scheduler ⟷ Scheduler |
| `/podmesh/handshake/1.0.0` | Peer authentication, key exchange | All nodes |
| `/podmesh/scheduler-tasks/1.0.0` | Scheduler coordination tasks | Scheduler ⟷ Scheduler |
| `/podmesh/ingress-proxy/1.0.0` | HTTP request forwarding | Proxy ⟷ Sidecar |
| `/podmesh/sidecar-manifest/1.0.0` | Manifest fetch RPCs | Proxy ⟷ Sidecar |

### Gossipsub Topics

| Topic | Purpose |
|-------|---------|
| `podmesh-machine` | Scheduler plane: capacity queries, proposals |
| `podmesh-workload` | Workload plane: proxy/sidecar coordination |

### Gossipsub Message Prefixes

| Prefix | Purpose |
|--------|---------|
| `podmesh-handshake` | Peer handshake messages |
| `podmesh-free-capacity` | Capacity request broadcasts |
| `podmesh-free-capacity-reply` | Capacity response messages |

### Kademlia DHT

- **Provider Records**: Schedulers announce as providers for deployed manifests
- **Manifest Records**: Key format `podmesh/manifest/{manifest_id}` maps to sidecar endpoints
- **Mode**: All nodes run in Server mode to store/serve records

### Behaviour Composition

The scheduler's libp2p behaviour (`podmesh-scheduler/src/podmesh_p2p/behaviour/mod.rs`):

```rust
pub struct MyBehaviour {
    pub gossipsub: gossipsub::Behaviour,      // PubSub messaging
    pub apply_rr: request_response::Behaviour, // Apply protocol
    pub handshake_rr: request_response::Behaviour, // Handshake protocol
    pub scheduler_rr: request_response::Behaviour, // Scheduler tasks
    pub delete_rr: request_response::Behaviour,    // Delete protocol
    pub kademlia: kad::Behaviour,              // DHT for discovery
    pub relay: relay::Behaviour,               // NAT traversal
    pub autonat: autonat::Behaviour,           // NAT detection
    pub identify: identify::Behaviour,         // Peer identification
}
```

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