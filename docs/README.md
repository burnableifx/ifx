# IFX documentation

The installed CLI is authoritative for commands (`ifx --help`) and schemas
(`ifx schema`). These guides explain the system around it.

## Learn

1. [Getting started](getting-started.md) — compile and operate a first Rust stack.
2. [Labs](../labs/README.md) — executable, narrated tutorials.
3. [QEMU lab](../labs/08-qemu/README.md) — the primary free full-system environment.
4. [Deployment Explorer](../labs/09-deployment-explorer/README.md) — topology, health,
   builds, runs, drift, and intervention.
5. [Examples](../examples/README.md) — copyable Rust stacks.

## Build and operate

- [CLI and configuration](cli.md) — commands, `ifx.toml`, configuration, and exit codes.
- [`ifxd`](ifxd.md) — trusted roots, continuous builds, immutable revisions, durable
  runs, retries, approval, authentication, and the HTTP API.
- [Resource reference](resources.md) — generated input/output and lifecycle schema.
- [QEMU](qemu.md) and [Linode](linode.md) — provider surfaces and boundaries.
- [Troubleshooting](troubleshooting.md) — build, state, database, SSH, and QEMU failures.
- [Performance](performance.md) — cold/warm/edit, stack-size, QEMU, cache, and binary measurements.

## Understand and extend

- [Design](design.md) — Program IR, execution boundary, graph and reconciliation model.
- [Provider authoring](provider-authoring.md) — handlers, schemas, generation, and tests.
- [Direction](direction.md) — planned leases, execution split, and provider protocol.
- [Brand](BRAND.md) — identity, voice, screenshots, and terminal demonstrations.

Regenerate schema-derived code and reference material after provider schema changes:

```console
$ cargo run -p ifx-gen
$ cargo run -p ifx-gen -- --check
$ cargo run -p ifx-cli -- stubs markdown -o docs/resources.md
```
