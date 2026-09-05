# Performance measurements

These are observed MVP measurements, not promises. They were captured on 2026-08-30
from a Threadripper 7970X (32 cores/64 threads, 125 GiB RAM), Fedora Linux 44 with
kernel 7.1.9, Rust 1.98, local NVMe storage, and a warm Cargo registry. Compilation was forced
offline. Each row is one run unless stated otherwise.

## Authoring loop

| Operation | Wall time | Internal build time |
|---|---:|---:|
| Fresh IFX cache: daemon start to ready | 5.168 s | 4.855 s |
| Manual no-op `ifx build` (3-run median) | 280 ms | 222 ms |
| Save to ready, 250 ms watcher (5-run median) | 396 ms | 225 ms |
| Second stack, same shared cache | 620 ms | 334 ms |

The clean build is the one-time dependency cost. The second stack proves that all
trusted manifests use one locked, garbage-collected Cargo target: it reused the first
stack's dependencies and added only its emitter. Each stack also keeps one stable
runtime copy under `.ifx/build`; it is protected from concurrent rebuild/GC by the same
cache lease.

Warm compilation meets the 250 ms target, but the complete authoring loop does not yet:
manual builds are 280 ms end to end and save-to-ready ranged from 390 to 475 ms across
five runs. The 250 ms poll phase is intentionally not synchronized to a save; the
compilation portion ranged from 222 to 226 ms. A previous 25 ms polling experiment
produced 234–245 ms save-to-ready but consumed 12.6% of one CPU while idle. That tradeoff
was rejected. Event-driven filesystem notification is the next correct optimization.

## Stack size and resolution

The component fixture declares two resources per requested node plus three shared
resources. “Direct emit” runs the cached 1.62 MiB artifact. “Daemon resolve” includes
CLI startup, HTTP, emission, validation, hashing, and durable SurrealKV revision storage.

| Resources | Program JSON | Direct emit | Daemon resolve | Emitter max RSS |
|---:|---:|---:|---:|---:|
| 23 | 5.8 KiB | <10 ms | 70 ms | 2.2 MiB |
| 2,003 | 527 KiB | 50 ms | 820 ms | 9.7 MiB |
| 20,003 | 5.22 MiB | 1.77 s | 28.24 s | 80.6 MiB |

The 20,003-resource result is not acceptable for an interactive path. The direct/API
gap indicates that validation and especially durable revision storage dominate at that
size; profiling and a compact/batched persistence representation belong ahead of
further emitter optimization. Normal small stacks stay below 100 ms for config-only
resolution and never invoke Cargo.

## Full QEMU lab

Lab 8 was remeasured on 2026-08-31 with the already verified Debian image, writable
KVM, three 512 MiB VMs, two rootless socket links, parallel SSH package/file/service
configuration, and five probes. The middle and leaf removed their direct bootstrap
forwards and used one-hop and two-hop SSH routes for the remainder of the run.

| Measurement | Result |
|---|---:|
| Complete plan/apply/check/idempotent-plan/destroy | 169.8 s |
| QEMU processes after destroy | 0 |
| Managed resources | 20 |
| Required checks healthy | 5/5 |
| Narrated cast duration | 180.70 s |

The verified 325 MiB base remained cached; all instance overlays, processes, pidfiles,
and QMP sockets were removed.

## Disk and binaries

| Artifact | Size |
|---|---:|
| Fresh shared compiler cache after two stacks | 109.19 MiB |
| Lab 1 emitter | 1.42 MiB |
| Stripped release `ifx` | 11.47 MiB |
| Stripped release `ifxd` | 51.44 MiB |
| Stripped release `ifx-labs` | 1.11 MiB |

The stack is the small emitter, not the CLI or daemon. Debug binaries are much larger
because they contain symbols for the complete provider and embedded-database graph.
The existing release profile uses ThinLTO and stripping. A clean release build of all
three binaries took 84.39 seconds and peaked at 4.70 GiB RSS. Rebuilding the final four
changed workspace crates incrementally took 47.61 seconds and peaked at 4.68 GiB RSS.
Both measurements used the 64-thread machine aggressively.

Reproduce the functional measurements with:

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ target/debug/ifx-labs run 1 2 3 4 5 9
$ target/debug/ifx-labs run 8
```

Cold-cache numbers require a new `IFX_CACHE_DIR`; warm and edit timings are reported by
`ifx build`/`ifx status --json`. Keep the machine, storage, toolchain, cache state,
resource count, and KVM/TCG mode alongside any comparison.
