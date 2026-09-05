# `ifxd`

`ifxd` is the trust and execution boundary. It compiles configured Rust stacks,
validates and persists Program revisions, serializes mutations, observes health and
drift, and exposes the CLI/Explorer API.

It also has a separate [broker mode](brokering.md): hold an upstream credential and
perform named-stack lifecycle calls for expiring scoped client tokens. Broker mode
does not execute local stack source or expose the executor admin API.

## Start one stack

```console
$ ifxd \
    --db surrealkv://$PWD/.ifx/db \
    --listen 127.0.0.1:7433 \
    --stack "$PWD=default"
```

The daemon refuses to start without at least one trusted stack. `DIR=NAME` is
repeatable. Stack roots are server-local and administrator-configured; clients cannot
register paths, upload source, or submit Programs.

## Configuration

```toml
db = "ws://127.0.0.1:8000"
listen = "127.0.0.1:7433"
check_interval = "30s"
drift_interval = "5m"
require_lease = false

[[stacks]]
dir = "/srv/ifx/stacks/web"
name = "production"
file = "Cargo.toml"
check_interval = "15s"
drift_interval = "2m"
no_drift = false
require_lease = false
```

`require_lease` refuses to apply a stack that has no lease. The global setting applies
to every stack; a stack can also opt in alone. `--require-lease` sets it from the
command line.

| Setting/environment | Meaning |
|---|---|
| `IFXD_CONFIG` | TOML configuration path |
| `IFX_DB` | SurrealDB endpoint or embedded `surrealkv://` path |
| `IFXD_LISTEN` | HTTP address |
| `IFXD_TOKEN` | Bearer token for every `/api` route |
| `IFX_CACHE_DIR` | Parent of the shared Rust compiler cache |

Use an embedded store only when one daemon process owns it. Use `ws://` for a SurrealDB
server shared across daemon restarts or hosts.

## Build lifecycle

At startup, each watcher queues an initial build while the HTTP server becomes
available. The build endpoint reports:

```json
{
  "generation": 4,
  "phase": "ready",
  "changed_at": "2026-08-30T14:00:00Z",
  "started_at": "2026-08-30T14:00:00Z",
  "finished_at": "2026-08-30T14:00:00Z",
  "duration_ms": 148,
  "revision": "…",
  "error": null
}
```

Stack sources, local path dependencies, workspace/lock files, Cargo configuration,
toolchain selectors, and `ifx.toml` are scanned every 250 ms. Scans, builds, and emitter
execution run off the async executor. Scan errors invalidate the current generation;
source recovery rebuilds automatically. Generation checks prevent an old build from
becoming current after a later edit. Configuration-only resolution executes the stable
emitter and does not invoke Cargo.

Program resolution rechecks the source fingerprint synchronously. The polling interval
therefore affects background readiness latency, not whether an operation can use stale
intent.

```console
$ curl -fsS http://127.0.0.1:7433/api/v1/stacks/default/build
$ curl -fsS -X POST http://127.0.0.1:7433/api/v1/stacks/default/build
```

The POST is manual retry. It does not affect execution retry counters.

## API

| Method and path | Purpose |
|---|---|
| `GET /healthz` | Process readiness; never requires bearer auth |
| `GET /api/stacks` | Watched/stored stack inventory including build state |
| `GET /api/stacks/{stack}` | Health, runs, and current build state |
| `GET /api/stacks/{stack}/topology` | Desired/observed graph and build status |
| `POST /api/stacks/{stack}/check` | Run checks immediately |
| `POST /api/stacks/{stack}/drift` | Run drift observation immediately |
| `GET/POST /api/v1/stacks/{stack}/build` | Build status/manual retry |
| `POST /api/v1/stacks/{stack}/program` | Emit current artifact with configuration |
| `GET /api/v1/stacks/{stack}/revisions/active` | Current immutable revision |
| `POST /api/v1/stacks/{stack}/runs` | Queue plan/apply/refresh/check/destroy |
| `GET/PUT/DELETE /api/v1/stacks/{stack}/lease` | Show, set or replace, clear the destruction deadline |
| `POST /api/v1/stacks/{stack}/lease/extend` | Push an unexpired deadline later |
| `GET /api/v1/runs/{id}` | Durable run status |
| `GET /api/v1/runs/{id}/events.json` | Persisted event list |
| `POST /api/v1/runs/{id}/retry` | Wake retry/recovery now |
| `POST /api/v1/runs/{id}/approve` | Grant one exact risk |
| `POST /api/v1/runs/{id}/cancel` | Cancel or roll back |

There is intentionally no Program-submission endpoint. The daemon alone produces the
revision it executes from configured source.

## Execution

Read-only plans may overlap. Apply, refresh, and destructive destroy runs acquire the
stack mutation lease. Every event and terminal status is persisted. Normal failures
retry with capped exponential backoff; operator retry wakes the wait immediately.

Approval waits remain resumable across daemon restart. A restart during replacement
cannot guess whether a provider mutation committed, so the run enters `recovery_wait`
with the affected URNs. Retry invokes the provider's recovery contract; cancel attempts
rollback.

Plan/apply/refresh reject dirty or failed source. Check and destroy are state-only and
remain available. Scheduled drift stops until the current Program is ready; scheduled
checks continue against deployed state.

The daemon reloads `ifx.toml` for each generation and retains that exact context with
the compiled artifact. Project/store selection is a startup boundary; restart `ifxd`
after changing the project name.

## Leases

A lease is a durable per-stack destruction deadline stored beside state, never in
source. The daemon polls it every second. Once the deadline passes it queues an ordinary
state-only destroy run with a single attempt, records that run on the lease, and leaves
it alone while it is in flight. A failed destroy is queued again after one minute; an
empty stack queues nothing.

Because the lease is in the store, a daemon restart re-arms it: an expired lease is
enforced on the first tick after startup. Apply is refused while a lease is expired,
and refused without any lease when `require_lease` is set. Extension only moves an
unexpired deadline; replacing an expired lease requires `PUT` with a new deadline.
Replacing a lease while its destroy is already queued keeps that destroy.

A deadline never waits for a human. Any run parked in `approval_wait`, including a
lease destroy whose deletions carry named risks, is cancelled by the expired lease and
the destroy is queued again after the retry interval. A lease destroy that enters
`recovery_wait` is the one exception: replacement recovery needs an operator, and the
lease waits for `retry` or `cancel` on that run.

The lease carries a reserved `grace` window for a future suspend-before-destroy mode.
It must be zero today; other values are rejected.

## Authentication and exposure

With `IFXD_TOKEN`, every `/api` route requires `Authorization: Bearer …`; `/healthz`
remains public. Bind to loopback unless a reverse proxy supplies TLS and access control.
The bearer token protects control APIs but does not sandbox stack compilation. A trusted
stack can execute Rust build scripts and its emitter as the daemon user, so configured
roots must be reviewed code.

## Explorer

Open `/` or `/explorer`. The live view polls topology and shows source phase,
generation, build duration, diagnostics, revision, resource actions, health, and durable
runs. It can trigger checks, drift, retries, cancellation, and approvals. A failed build
continues to render the previous deployed topology with an explicit stale error instead
of silently treating it as desired state.
