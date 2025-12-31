# JITO-Downloader

A high-performance, asynchronous concurrent download utility written in Rust. The project implements a Just-In-Time Optimizer (JITO) to dynamically assess server capabilities and network conditions, selecting the most efficient transfer strategy in real-time.

---

## Architecture and Components

The system is modularized to separate network orchestration, optimization logic, and file I/O operations:

| Component | PC Directory | Functional Description |
| :--- | :--- | :--- |
| `main.rs` | `/src/main.rs` | Application entry point, CLI argument parsing, and workflow orchestration. |
| `jito.rs` | `/src/jito.rs` | Just-In-Time Optimizer: Network probing, server profiling, and strategy selection. |
| `net.rs` | `/src/net.rs` | Network primitives: Multi-stream management, range requests, and buffered I/O. |
| `logic.rs` | `/src/logic.rs` | Validation engine: Security constraints, path sanitization, and integrity checks. |

---

## Core Optimization Principles

### 1. The New Fix: Adaptive Scaling Analysis
Unlike static downloaders, JITO performs **Adaptive Probe Rounds**. By executing staggered range requests during the initiation phase, the system calculates the "Scaling Efficiency" of the target server. If the server exhibits performance degradation as concurrency increases, the optimizer dynamically recalibrates the thread count to the detected threshold, preventing bandwidth caps or temporary IP blocks.

### 2. The Vibe Check (Logic): Synergy of Multi-Range and Keep-Alive
The system prioritizes the intersection of `RangeSupport` and `KeepAlive` capabilities. When a server supports `multipart/byteranges`, it indicates an architecture optimized for parallel delivery. By leveraging a persistent connection pool alongside these features, the downloader eliminates the cumulative overhead of TCP and TLS handshakes for individual segments, effectively saturating the network pipe.

---

## Technical Specifications

### Server Capability Detection
* **Protocol Support:** Automatic detection and optimization for HTTP/2 and HTTP/3.
* **Throttling Identification:** Detection of Hard Limits, Bandwidth Caps, and Time-Based Throttling.
* **Network Quality:** Real-time measurement of latency, packet loss, and jitter to adjust chunk sizes.

### Security and Data Integrity
* **SSRF Mitigation:** Restrictions against loopback and private network address ranges.
* **Path Sanitization:** Protection against path traversal and restricted character sets.
* **Integrity Verification:** Post-download byte-count validation against server-reported `Content-Length`.

---

## Usage

### Basic Command Syntax 
cargo run -- --jito --link "URL" --threads 16

or Simply Use Command --help command to Learn More.