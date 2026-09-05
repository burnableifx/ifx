# CLI and stack configuration

`ifx` is a client for daemon-owned stacks. Except for schema/reference generation, every
command uses `ifxd`; the client does not compile source or invoke providers.

## Global flags

| Flag | Purpose |
|---|---|
| `-C, --dir DIR` | Stack directory containing `ifx.toml`; default `.` |
| `-f, --file FILE` | Rust manifest override; default `Cargo.toml` or `ifx.toml` `file` |
| `-s, --stack NAME` | Daemon stack name; default `default`, env `IFX_STACK` |
| `--daemon URL` | `ifxd` URL; default `http://127.0.0.1:7433`, env `IFXD_URL` |
| `-c, --config KEY=VALUE` | Override an emitter-time value; JSON literals are decoded |
| `-t, --target URN` | Restrict execution to a resource and its dependencies; repeatable |
| `-j, --parallel N` | Maximum provider operations; default 8 |
| `--attempts N` | Maximum execution attempts; default 3 |
| `--retry-backoff-secs N` | Initial exponential delay; default 2 |
| `--retry-max-backoff-secs N` | Delay cap; default 60 |
| `-v` | Include unchanged resources and debug events |

Set `IFXD_TOKEN` when the daemon requires bearer authentication.

## Build and intent

```console
$ ifx build
$ ifx emit-program
$ ifx schema qemu.instance
$ ifx schema --json
$ ifx stubs markdown -o docs/resources.md
```

`build` marks the source dirty and immediately retries compilation. Normal edits build
automatically. It returns 0 only for a ready generation.

`emit-program` asks the daemon to execute its current compiled artifact with merged
configuration and prints the validated Program. It does not invoke Cargo in the client.

`schema` and `stubs` are the only local commands because schemas are compiled into the
client and do not require stack intent or state.

## Reconciliation

```console
$ ifx plan
$ ifx plan --no-refresh
$ ifx apply -y
$ ifx apply -y --lease 30m
$ ifx refresh
$ ifx check
$ ifx destroy -y
```

Plan, apply, and refresh first resolve the current source generation through `ifxd`.
They fail if the build is dirty, building, or failed. `--no-refresh` trusts stored
observations for the preview. Destroy is state-only and remains available when source
does not compile.

Risks are approved by exact selector:

```console
$ ifx apply --approve qemu.instance:node/restart
$ ifx run approve run-123 qemu.instance:node/restart
```

Grants bind revision, URN, risk, and operation fingerprint. Changed inputs invalidate
the grant.

## Runs and retries

```console
$ ifx run list
$ ifx run show run-123
$ ifx run events run-123
$ ifx run retry run-123
$ ifx run cancel run-123
```

`run retry` wakes a durable `retry_wait` immediately while retaining exponential
backoff for later failures. A replacement interrupted by daemon restart enters
`recovery_wait`; retry resumes recovery and cancellation attempts rollback.

## Leases

```console
$ ifx lease set --for 2h
$ ifx lease set --until 2026-09-04T18:00:00Z
$ ifx lease show
$ ifx lease extend --by 30m
$ ifx lease clear
```

A lease is the stack's destruction deadline. `ifxd` checks it every second and queues a
state-only destroy once it passes, so the deadline holds while source does not compile
and after a daemon restart. `apply --lease` sets the lease first, then applies.

Extension requires an unexpired lease; an expired one blocks apply until `lease set`
replaces it. `lease clear` is refused when the daemon requires leases. `ifx status`
shows the deadline and time remaining. `apply --lease` sets the lease before the plan
is resolved, so the lease stands even if the apply itself fails.

## State and outputs

```console
$ ifx outputs
$ ifx state list
$ ifx state show qemu.instance:web
$ ifx state export -o state.json
$ ifx state import state.json
$ ifx state rm host.file:legacy
```

State operations go through `ifxd`. Removing state forgets identity without deleting the
provider object. Import and removal are disabled while replacement recovery is pending.

## Status and graph

```console
$ ifx status
$ ifx status --json
$ ifx graph
$ ifx graph --format json --no-refresh
$ ifx graph --format html --out deployment.html
```

DOT contains the desired dependency graph. JSON and HTML include desired/observed
properties, actions, health, history, current build generation, duration, diagnostics,
and revision. Open the live Explorer at the daemon root URL.

## `ifx.toml`

```toml
name = "production"
file = "Cargo.toml"

[config]
region = "us-east"
nodes = 3
```

- `name` is the project/database name unless `ifxd` overrides it.
- `file` must name a Rust `Cargo.toml` within the administrator-configured stack root.
- `[config]` becomes `ifx_program::Context`; CLI `-c` values override it.
- Secrets should arrive from an external secret mechanism or CLI environment and be
  wrapped with `ifx_program::secret` before declaration. Do not commit credentials.

The daemon configuration owns its trusted roots and database:

```toml
db = "surrealkv:///var/lib/ifx/state"
listen = "127.0.0.1:7433"

[[stacks]]
dir = "/srv/ifx/stacks/production"
name = "default"
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, empty plan, or healthy checks |
| 1 | Error, failed run, refused operation, or unhealthy check |
| 2 | Plan has changes, or status is degraded/drifted/unknown |
| 3 | Apply is durably waiting for approval |
