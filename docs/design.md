# Design

ifx models a deployment as one directed graph of resource declarations. Provider
resources, guest configuration, and health checks use the same lifecycle and reference
model. Rust is the only authoring language; `ifxd` is the only compilation, execution,
and persistence owner.

Planned changes to these boundaries, an execution split and out-of-process providers,
are recorded in [Direction](direction.md).

## Boundaries

```text
trusted Rust stack roots
        │ source/path-dependency scan
        ▼
ifxd compiler ─── shared garbage-collected Cargo target
        │ compiled emitter
        ▼
validated Program revision ─── SurrealDB history
        │
        ▼
engine plan/apply ─── provider APIs and SSH
        │
        └── health, drift, events, approvals, recovery
```

`ifx-program` contains only the IR, stack collector, generated typed builders, and the
small emitter runtime. It has no provider implementation, transport, store, HTTP client,
or async runtime. This keeps stack compilation small and lets unrelated stacks share
the same compiled dependencies.

The `ifx` CLI sends configuration and control requests. It does not compile source or
submit serialized Programs. `ifxd` accepts only stack roots configured by its
administrator; there is no endpoint for arbitrary source paths or uploads.

## Continuous compilation

Each configured stack has one daemon watcher and one build state:

- `dirty` — a source generation changed; previous intent is stale;
- `building` — Cargo is producing an isolated emitter in the shared target;
- `ready` — the current emitter produced and validated a Program revision;
- `failed` — compilation, emission, graph validation, or schema validation failed.

The watcher scans the stack, every local Cargo path dependency, the workspace manifest
and lock, applicable Cargo configuration, Rust toolchain selectors, and `ifx.toml` at
250 ms intervals. It excludes `.git`, `.ifx`, `target`, and `node_modules`. Scans run on
blocking workers. A missing/unreadable required source root invalidates the generation;
recovery queues a new build. A result can become current only if its generation still
matches, so edits during a build coalesce into the next build.

The polling loop minimizes authoring latency; it is not the mutation safety boundary.
Program resolution performs its own source fingerprint check, so a plan/apply cannot
use a change merely because the next 250 ms tick has not fired yet.

`ifx build` increments the generation and retries immediately. Source builds have no
execution retry policy; daemon execution retries retain their capped exponential
backoff and `ifx run retry` can wake them early.

Build and emit are separate. Cargo produces a configuration-independent executable.
The daemon copies the emitter and dynamic runtime libraries into `.ifx/build`, then
executes that stable artifact with `IFX_STACK` and serialized `IFX_CONFIG` to produce
one Program. `ifx.toml` is reloaded for each source generation. Changing only a CLI
configuration override emits another content-addressed revision without compiling.

All stacks use one cache rooted at `IFX_CACHE_DIR/rust`,
`$XDG_CACHE_HOME/ifx/rust`, or `~/.cache/ifx/rust`. Builds/GC take an exclusive cache
lease; emitter processes hold a shared lease through exit. Daily GC removes a target
that is over 4 GiB or has been idle for 30 days. Stable per-stack runtime artifacts are
replaced only while the exclusive lease is held and are not part of the Cargo cache.
`rustc` runtime discovery is cached per stack/toolchain-selector fingerprint; editing
`rust-toolchain` or `rust-toolchain.toml` selects a new entry and rebuilds.

## Fail-closed source semantics

Plan, apply, refresh, drift, and topology resolution require a `ready` current
generation. They never silently execute the last successful Program after source has
changed or failed. The Explorer may render the previous deployed topology, clearly
marked with the current failed/dirty build and diagnostics. Health checks may continue
against durable deployed state because they do not infer desired state.

## Program IR

A Program is data:

```json
{
  "resources": [
    {
      "urn": "host.file:page",
      "inputs": {
        "on": {"$ref": "qemu.instance:web", "$path": "connection"},
        "path": "/var/www/html/index.html",
        "content": "hello\n"
      },
      "depends_on": [],
      "triggers": [],
      "protect": false
    }
  ]
}
```

The IR remains serde-encoded because it is hashed, persisted, returned by APIs, and
stored with execution history. Rust-only authoring removes a second runtime; it does
not remove the durable data boundary.

References are both values and graph edges. `$concat` defers strings containing unknown
outputs. `$secret` marks values that must be stripped before provider calls and redacted
from plans, snapshots, logs, and approval fingerprints.

## Planning and reconciliation

For every declaration the engine:

1. validates the graph and provider schema;
2. resolves known references and marks unresolved values unknown;
3. calls the handler's `read` against the real provider or host;
4. compares observed properties with desired inputs through `diff`;
5. schedules create, adopt, update, replace, trigger, delete, or no-op actions;
6. executes ready nodes concurrently, bounded by configured parallelism;
7. persists identities, outputs, protection, and events.

State preserves facts that cannot always be rediscovered cheaply: provider ids, last
inputs, outputs, protection, replacement journals, execution events, approvals, and
history. It is not treated as proof that a resource still matches; normal plans observe
the provider.

Destroy uses durable state and an empty Program, so it remains available when source
does not compile. Protection stored on a resource still blocks deletion.

## Replacement safety and approval

Destructive and availability-affecting operations publish named risks with exact
fingerprints. Approval binds the Program revision, URN, risk, and resolved operation.
Changed input invalidates the grant.

Replacement writes a durable intent before provider mutation. If `ifxd` stops between
prepare and commit, startup enters `recovery_wait`; an operator can retry recovery or
cancel and roll back. Runs, approval waits, retry waits, and recovery events are durable.

QEMU attachment changes illustrate the rule: an instance declared `restartable = true`
may be restarted automatically. Otherwise the restart requires an exact approval or an
explicit force path.

QEMU management is an execution capability, not just an address. Direct, via, bootstrap-
then-via, execution-scoped direct, immutable, and absent lifecycles are typed instance
inputs. Management and egress remain independent. Execution-scoped access is opened only
by a mutating run that needs it and closed before the provider owning its QMP endpoint is
removed. Read-only plans, drift scans, and check-only schedules never mutate networking.

## Observation and Explorer

Checks are resources. Required checks can fail apply; all checks can run on demand or
on the daemon schedule. Drift reuses the planner, so “drifted” means a current plan
would change the resource.

The topology API combines Program declarations, provider observation, state, health,
build status, and execution history. The web document supports dependency, provider,
health, and action layouts over the same graph. Offline HTML embeds a secret-safe
snapshot; the live Explorer polls `ifxd` and exposes manual check, drift, retry,
cancellation, and approval controls.

## Schema and generation

Every handler owns a `ResourceSchema`. `ifx-gen` generates Rust builders into
`ifx-program`; the same schemas drive daemon validation, `ifx schema`, and
`docs/resources.md`. Generated sources are checked in so stack compilation and editor
completion do not require code generation.

Adding a resource means implementing its handler and schema, registering it, running
`cargo run -p ifx-gen`, and adding tests at the cheapest layer that proves its
lifecycle. There are no handwritten alternate-language builders.

## Leases

A stack may carry a lease: a durable destruction deadline with an extension history.
It lives in the store next to resource state, not in `ifx.toml` or the Program, because
anything in source is reasserted on every apply and would silently revert an extension
granted through the API.

`ifxd` polls each lease every second and queues the existing state-only destroy when the
deadline passes, so the deadline holds without compilable source and across daemon
restarts. The queued run is recorded on the lease; a failed run is retried after a fixed
interval. An expired lease blocks apply until it is replaced. `require_lease` makes a
lease a precondition for apply at all.

The daemon knows deadlines and nothing else. Converting a budget, session, or job into a
deadline is the caller's responsibility. `protect` is the inverse concept and is
unchanged: a protected resource in a leased stack fails the destroy at plan time, which
is visible on the run and in the lease's recorded fire.

## Deferred direction: broader Linux convergence

IFX may eventually use Ansible's module catalog as a coverage map for Linux host
management. This is not current scope, an Ansible compatibility promise, or a plan to
port its implementation. Ansible is useful here as a mature inventory of user needs;
IFX keeps its own typed, graph-based lifecycle and stronger state, recovery, approval,
and observation semantics.

The conceptual mapping is direct:

| Ansible concept | IFX convention |
|---|---|
| module arguments, defaults, choices, and secrets | schema inputs, defaults, enums, and sensitive fields |
| module return values and facts | typed resource outputs |
| current-state inspection and `changed` | `read` plus `diff` |
| module mutation | `create`, `update`, and `delete` |
| role | versioned Rust component crate or function |
| inventory connection | `Connection` output passed to `host.*.on(...)` |
| notification and handler | graph edge plus `triggered_by` |
| check and diff modes | normal IFX plan |
| destructive option | named risk with exact approval |

Most file, package, service, user, group, cron, sysctl, repository, hostname, and
authorized-key resources should be routine once common Linux observation and command
helpers exist. Networking, firewall, SELinux/AppArmor, storage, filesystems, LVM, RAID,
and database lifecycles require more deliberate platform, destructive-action, and
recovery design. Arbitrary commands are easy to execute but cannot become declarative
without an observable guard or postcondition.

Do not translate action-shaped Ansible inputs literally. `restarted`, `reloaded`,
`update_cache`, `force`, `purge`, and `backup` must be classified as desired state,
policy, trigger, risk, or output. Restarts and reloads are normally triggered actions,
not durable states. Avoid arbitrary `changed_when`/`failed_when` expressions and global
variable-precedence behavior when typed resources and explicit graph edges suffice.

When this work is scheduled, build it as a resource factory rather than a module-porting
campaign:

1. inventory Ansible's declarative Linux surface and record the intended IFX semantic;
2. establish reusable inspection, normalization, atomic-write, and validation helpers;
3. generate schema/builders first, then implement provider-owned observation and change;
4. test each resource against cached QEMU images for the distributions it claims;
5. require absent-to-create, matching no-op, drift repair, desired update, deletion,
   interrupted recovery, actionable unsupported-platform failure, and an empty second
   plan;
6. add higher-risk networking, security, and storage resources only after their approval
   and recovery contracts are explicit.

AI can shorten schema, parser, fixture, implementation, documentation, and repair loops.
It does not replace the cross-distribution integration matrix or destructive failure
testing. The expected bottleneck is proof of behavior, not Rust code generation.
