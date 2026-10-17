# task-manager
A Windows 7/8/10-like task manager for Linux written in Rust

## Status

Step 1 (data core): `tm-core` is a stateful `/proc` sampler (system and per-core CPU, memory, per-process CPU/RSS) and `tm-cli` prints a top-style table to check it.

```
cargo run --release -p tm-cli -- [interval_secs] [rows]
```
