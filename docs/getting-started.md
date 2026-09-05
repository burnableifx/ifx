# Getting started

## Prerequisites

- Rust 1.85 or newer;
- a C compiler and system build dependencies required by SurrealDB;
- `curl` for manual API examples;
- QEMU prerequisites only for Lab 8.

Build the product and lab runner:

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ cargo run -p ifx-labs -- list
```

## Run the first lab

```console
$ cargo run -p ifx-labs -- run 1
```

The runner uses `/tmp/ifx-lab/01`, starts an isolated `ifxd`, forces one build, plans,
applies, proves the next plan is empty, and destroys the files. Narration explains each
command and its expected exit status.

For manual work, use two terminals:

```console
$ cargo run -p ifxd -- \
    --db surrealkv://$PWD/labs/01-first-stack/.ifx/db \
    --stack "$PWD/labs/01-first-stack=default"
```

```console
$ export IFXD_URL=http://127.0.0.1:7433
$ cd labs/01-first-stack
$ ../../../target/debug/ifx build
$ ../../../target/debug/ifx plan
$ ../../../target/debug/ifx apply -y
$ ../../../target/debug/ifx plan
$ ../../../target/debug/ifx destroy -y
```

`plan` exits 2 when changes are pending, 0 when empty, and 1 on failure.

## Stack layout

```text
my-stack/
├── Cargo.toml
├── Cargo.lock
├── ifx.toml
└── src/
    └── main.rs
```

`Cargo.toml` should make the stack standalone and keep the authoring dependency small:

```toml
[package]
name = "my-stack"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
ifx-program = { version = "0.1", default-features = false, features = ["host"] }
```

Commit `Cargo.lock`: `ifxd` builds with `cargo rustc --locked` so a deployment cannot
silently resolve different dependencies.

`ifx.toml` names the project, selects the manifest, and supplies emitter-time values:

```toml
name = "my-stack"
file = "Cargo.toml"

[config]
base = "/tmp/my-stack"
```

Read values through the emitter context:

```rust
use std::process::ExitCode;

use ifx_program::{Connection, emit_program};

fn main() -> ExitCode {
    emit_program(|stack, context| {
        let base: String = context.config("base")?;
        let directory = stack
            .host_file("directory")
            .on(Connection::local())
            .path(&base)
            .directory(true)
            .add()?;

        stack
            .host_file("message")
            .on(Connection::local())
            .path(format!("{base}/message.txt"))
            .content("hello from ifx\n")
            .depends_on(&directory)
            .add()?;
        Ok(())
    })
}
```

Required inputs and mutually exclusive schema groups are enforced by generated typestate
builders. Output accessors return typed deferred `Input<T>` references. Use
`ifx_program::concat!` to combine literals and unknown outputs into a string resolved by
the engine during apply.

## What happens after save

Each `ifxd` stack watcher scans the stack, local Cargo path dependencies, workspace
inputs, Cargo configuration, toolchain selectors, and `ifx.toml` every 250 ms. A change
or scan failure immediately marks the previous Program stale; source recovery queues a
shared-cache build. Successful compilation installs a stable emitter under `.ifx/build`,
runs it with the current stack name and config, validates its graph and schemas, and
activates a content-addressed revision.

```console
$ ifx status
stack default  3 resource(s), 0 check(s)  overall: no observations yet
build Ready  generation 4  148 ms
```

If compilation fails, `ifx status --json` and the Explorer show diagnostics. Plan and
apply refuse to use the previous Program. Fixing the source triggers recovery; `ifx
build` retries immediately without waiting for another scan.

## QEMU path

Install `qemu-system-x86_64`, `qemu-img`, OpenSSH client tools, and one NoCloud seed
builder. Then run:

```console
$ cargo run -p ifx-labs -- run 8
```

The image is verified and cached under `/tmp/ifx-lab/qemu-cache`. Per-run overlays,
pidfiles, QMP sockets, SSH forwards, and two socket-network endpoints live under
`/tmp/ifx-lab/08`. The router remains directly managed; the middle and leaf remove their
bootstrap forwards and use nested SSH via the preceding VM. The lab destroys instance
files but intentionally preserves the image cache.

## Next

- [Lab sequence](../labs/README.md)
- [Rust authoring](../labs/07-rust-program/README.md)
- [CLI](cli.md)
- [`ifxd`](ifxd.md)
- [QEMU provider](qemu.md)
- [Design](design.md)
