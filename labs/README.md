# IFX labs

These labs teach the Rust authoring model and the daemon-owned build and execution
loop. Labs 1–5 and 8–9 are executable; Lab 8 is the primary full-system target because
it creates real machines without a cloud account.

| Lab | Subject | Extra requirements | Recording |
|---|---|---|---|
| [0 — Setup](00-setup.md) | toolchain, daemon, shared cache | Rust | — |
| [1 — First stack](01-first-stack/README.md) | plan, apply, idempotency, destroy | — | [GIF](media/lab-1.gif) · [cast](casts/lab-1.cast) |
| [2 — References](02-references-and-loops/README.md) | outputs, loops, components, config | — | [GIF](media/lab-2.gif) · [cast](casts/lab-2.cast) |
| [3 — Lifecycle](03-lifecycle/README.md) | triggers, guards, protection | — | [GIF](media/lab-3.gif) · [cast](casts/lab-3.cast) |
| [4 — Health](04-health-and-daemon/README.md) | required checks and daemon state | — | [GIF](media/lab-4.gif) · [cast](casts/lab-4.cast) |
| [5 — Components](05-components/README.md) | reusable Rust functions and typed outputs | — | [GIF](media/lab-5.gif) · [cast](casts/lab-5.cast) |
| [6 — Linode](06-linode/README.md) | paid cloud instances configured over SSH | `LINODE_TOKEN` | — |
| [7 — Program model](07-rust-program/README.md) | the emitter and immutable Program IR | — | — |
| [8 — QEMU](08-qemu/README.md) | three VMs, two links, nested SSH management | QEMU, seed builder | [GIF](media/lab-8.gif) · [cast](casts/lab-8.cast) |
| [9 — Explorer](09-deployment-explorer/README.md) | interactive topology, health, drift | browser | [GIF](media/lab-9.gif) · [cast](casts/lab-9.cast) |

Build the binaries once, then run any lab. The runner starts an isolated daemon and
SurrealKV store, checks the first plan, applies, verifies an empty second plan, and
destroys the resources.

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ target/debug/ifx-labs list
$ target/debug/ifx-labs run 1
$ target/debug/ifx-labs run 8
```

`demo` adds deliberate reading pauses and `#` explanations. `record` captures that
same narration with asciinema; `gif` renders a paced preview with `agg`.

```console
$ target/debug/ifx-labs demo 2
$ target/debug/ifx-labs record 2
$ target/debug/ifx-labs gif 2
$ cargo run -p ifx-labs -- docs
```

Each stack is an ordinary Cargo binary with `ifx-program` as a dependency. `ifxd`
compiles trusted stack roots into one garbage-collected cache, executes the cached
emitter for configuration changes, validates the resulting graph, and owns all
provider work. A source build must be current and successful before plan or apply.
