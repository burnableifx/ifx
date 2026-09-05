# Direction

This page records planned changes to IFX that are not yet implemented. [Design](design.md)
describes the system as it is; nothing here changes current behaviour. The notes come from
architecture discovery for burnable.dev (2026-09-03), the first external consumer of IFX,
and are kept in IFX terms so they stay useful as general infrastructure primitives.

## Driving requirements

burnable.dev provisions infrastructure that carries an explicit destruction condition and
is expected to be gone when that condition trips, whether or not any daemon is running.
The requirements it places on IFX:

- a stack can declare a lifetime, and the executor enforces it;
- a control plane can execute stacks it did not build, on behalf of many callers;
- one deployment may span several provider credential sets;
- destruction must be provable independently of executor state;
- new providers must be addable without changing the executor.

None of these are specific to one product. They are the missing lifecycle half of a
reconciliation engine that already knows how to create and destroy.

## Leases

Implemented; see [Design](design.md#leases). Source-declared `lease.initial` and
`lease.max` are not: the executor's configuration is the only ceiling, and any source
value is advisory. Money, session, and job fuses stay outside `ifxd`; callers convert
them to deadlines.

## Deadlines travel with the resource

The strongest destruction guarantee comes from a reaper that needs nothing from `ifxd`.
Each provider handler records the stack id and lease deadline on the resource itself where
the provider allows it (Linode tags; a per-guest metadata file for QEMU). A reaper is then a
loop over provider credentials: list everything carrying a lease marker, delete anything
past its deadline. Extension rewrites the marker as part of the lease update.

Two-writer safety is a delete-idempotency rule: every handler treats "already gone" on
delete as success. The reaper also catches resources the executor lost track of, which is
orphan detection for free. The trade-off is that the deadline is legible to anyone holding
the provider credential; that is acceptable.

## Execution split

`ifxd` currently owns compilation, execution, and persistence. It will stop compiling.
Program JSON becomes the boundary between developer tooling and execution.

```text
ifx             CLI. Talks to ifx-build for builds and diagnostics, to ifxd for
                plan, apply, state, and leases.
ifx-build       Local build daemon. Watches sources, owns the warm Cargo cache,
                emits Program JSON, serves diagnostics over a local socket.
                Never holds a provider credential. Never runs hosted.
ifxd            Executor. Accepts a Program plus a revision fingerprint, owns
                state, leases, and schedulers, spawns provider programs.
                The same binary locally and hosted.
ifx-provider-*  Provider programs spawned by ifxd (see below).
```

What moves out of `ifxd`: source polling, Cargo invocation, cache leases and GC, build
generations. What stays: run queue, retries, approvals, drift, health, recovery, and the
new lease scheduler. The fail-closed stale-generation check becomes a revision fingerprint
recorded with every run, which approvals already require.

`ifx watch` keeps its shape: `ifx-build` emits on change, the CLI submits to the local
`ifxd` for a plan, the diff is printed. Two processes instead of one.

Compilation is arbitrary code execution; that is why it must never happen on a host that
holds credentials for more than one caller. Hosted executors accept only emitted Programs.

Editor integration is deferred. Rust stacks already get diagnostics from rust-analyzer and
the generated typed builders; anything IFX-specific worth showing in an editor is plan-aware
(current state, replace-versus-update, output values) and would be a client of `ifxd`, not a
language server over source.

## Providers as programs

Providers move out of process. `ifxd` spawns a provider binary per type family and speaks
JSON-RPC over stdio. Goals: independent release cadence, third-party providers, and an
executor that does not change when a provider is added.

The `Handler` and `Checker` traits become the wire protocol. Two provider classes fall out:

- **API providers** (Linode, other clouds): pure HTTP, no transports, straightforward to
  externalize first.
- **Execution providers** (`host.*`, `qemu.*`): need the SSH and local transport pool in
  `Ctx`. Either they stay in-tree behind the same protocol, or the protocol grows transport
  operations. Undecided; start with API providers.

Alongside the protocol, a conformance kit drives any provider through
absent-to-create, matching no-op, update, drift, delete, delete-when-already-gone, and
interrupted recovery, with recorded HTTP fixtures. Without it, "easy to add" means
"easy to add broken".

Considered and not chosen: stable-ABI dynamic libraries (fragile, toolchain-locked, no
isolation) and declarative REST provider packs (attractive for simple clouds, may return
once the protocol exists). WASM components are a possible later transport for the same
protocol; not a current plan.

## Tenancy

`ifxd` stays a single trust domain with one caller. Tenancy is a namespace on stacks, not an
authorization model inside the executor. The caller in front of it enforces ownership; the
executor enforces that stack ids are unique within a namespace. Deploy one `ifxd` per
provider credential set so process boundaries match credential boundaries. The "modes"
discussed for the daemon reduce to configuration: `require_lease`, loopback or socket API,
namespace enforcement on or off.

## Program portability

An emitted Program is fully resolved; configuration is baked in at emit time. Sharing a stack
therefore means sharing source, and the consumer builds it with `ifx-build`. Hosted
executors can ship curated, pre-emitted Programs. Parameterized templates would need an
IR-level notion of unresolved inputs, which does not exist. Deferred until a registry asks
for it.

## Open questions

- Transport operations in the provider protocol, or execution providers in-tree.
- When the standalone reaper takes over from a repeatedly failing lease destroy.
- Suspend-before-destroy as a configurable grace mode; immediate destroy is the default.
- Store schema migration; state is at version 1 with no migration path, and leases add a
  table.

## Sequencing

1. Lease record, scheduler task, startup re-arm, `require_lease`. Done.
2. Provider-side lease markers and the standalone reaper.
3. `ifx-build` split; `ifxd` accepts Programs and stops compiling.
4. Provider protocol for API providers, with the conformance kit.
5. Execution providers over the protocol, or an explicit decision to keep them in-tree.
