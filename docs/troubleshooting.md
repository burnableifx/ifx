# Troubleshooting

Start with the exact command under `-v`; if the failure is provider-specific, inspect
the relevant resource with `ifx state show TYPE:NAME` and `ifx schema TYPE`.

## Build and generated code

### The lab runner cannot find `ifx` or `ifxd`

```console
$ cargo build -p ifx-cli -p ifxd -p ifx-labs
$ target/debug/ifx-labs list
```

The `ifx` library package does not produce the CLI executable; `ifx-cli` does.

### Generated code is stale

```console
$ cargo run -p ifx-gen
$ cargo run -p ifx-gen -- --check
```

Generated modules under `crates/ifx-program/src/generated/` must be committed with
schema changes.

### Rust Program compilation consumes too much disk

Rust stacks compile into one shared `$XDG_CACHE_HOME/ifx/rust` cache, not `target/`
directories beside the stacks. Set `IFX_CACHE_DIR` to relocate the IFX cache root. All
trusted stacks share one locked Cargo target, so dependencies compile once. At most
daily, IFX clears that target after 30 idle days or when it exceeds 4 GiB. Removing the
cache manually is safe when no IFX compilation is running; the next build recreates it.
Stable emitter copies under each stack's `.ifx/build`, stack source, lockfiles, and
Program revisions are unaffected.

### A builder refuses `.add()`

Run `ifx schema TYPE` and check required inputs, conditional requirements, exclusive
groups, and input types. Generated typestate prevents many incomplete builders; ifxd
performs authoritative Program validation after emission.

### The stack build is failed or remains dirty

Inspect `ifx status --json` or run `ifx build` to retry immediately. Otherwise the
watcher rebuilds after the next source change. Plan, apply, and refresh are fail-closed
until the current generation is ready. Checks and destroy remain available against the
deployed state.

## Plans and state

### The client says “is ifxd running?”

Check the URL and liveness endpoint:

```console
$ echo "$IFXD_URL"
$ curl -v "${IFXD_URL:-http://127.0.0.1:7433}/healthz"
```

For a local stack, start the daemon with
`ifxd --db surrealkv://$PWD/.ifx/db --stack "$PWD=default"`. A stack name in the client
must match the configured watcher name.

### The API returns 401

The daemon has bearer authentication enabled. Export the same `IFXD_TOKEN` in the
client environment. For curl, add `-H "Authorization: Bearer $IFXD_TOKEN"`. The
Explorer prompts for the token and stores it only for the current browser tab.

### `ifx plan` exits 2

Exit 2 means the plan contains changes. It is expected in scripts that are checking for
drift. Exit 1 represents an actual failure.

### A resource exists but state is missing

Run a normal refreshed plan. Providers that can prove identity may show adoption.
Inspect the object and declaration before applying. `ifx state rm` deliberately forgets
a resource without deleting it; it is not a repair command.

### Embedded SurrealDB is locked

Embedded SurrealKV permits one owning process, and that process must be ifxd. Check for
a second daemon using the same path and stop the duplicate. The CLI does not open the
database. For multiple daemon hosts, use a remote SurrealDB endpoint—but still assign
only one owning watcher to each stack. See [ifxd](ifxd.md#configuration).

### A run is waiting before retry

Inspect the error and either let exponential backoff proceed or wake it immediately:

```console
$ ifx run show RUN_ID
$ ifx run events RUN_ID
$ ifx run retry RUN_ID
```

`retry` does not add attempts. It also resumes an interrupted replacement from
`recovery_wait`. Use a new apply with a larger `--attempts` value after a terminal
failure. Use `ifx run cancel RUN_ID` to stop a nonterminal run; replacement cancellation
is terminal only after provider rollback succeeds.

### A run became failed after ifxd restarted

This is explicit crash recovery. The old task no longer exists, so ifxd records an
`interrupted` event instead of leaving a false `running` status. Review its events and
start a new idempotent plan/apply. Runs already in `approval_wait` are the exception:
their pending exact operation is durable, so ifxd restores the waiter after restart. A
run with a durable replacement journal becomes `recovery_wait`; inspect its events, then
use `ifx run retry RUN_ID` to recover forward or `ifx run cancel RUN_ID` to roll back.

### Move or back up state

```console
$ ifx state export -o state.json
$ ifx state import state.json
```

Treat exports as sensitive: provider outputs may contain infrastructure metadata.

## SSH and host resources

- Run with `-v` to see the underlying system `ssh` command and transport failure.
- Confirm the connection output with `ifx outputs` or `ifx state show`.
- Test the same identity, port, proxy, and known-host arguments using system SSH.
- `.privileged(true)` requires non-interactive `sudo -n` through a sudo-enabled
  connection.
- File ownership and system paths usually require privileged execution.
- Package manager locks are external contention; wait for the owning process rather
  than deleting its lock file.

## QEMU

### A required executable is missing

```console
$ command -v qemu-system-x86_64 qemu-img ssh-keygen
$ command -v cloud-localds || command -v genisoimage || command -v xorriso
```

The error names the first missing prerequisite.

### KVM is unavailable

This is not fatal. Check permissions with `test -w /dev/kvm`; if unavailable, expect a
warning and slower TCG boot.

### A forwarded port is occupied

Choose another `ssh_port` or `port_forwards` key. ifx binds only `127.0.0.1`; inspect
listeners with `ss -ltn`.

### SSH never becomes ready

Inspect the instance's `console.log`, then check:

- cloud image architecture and checksum;
- available disk space;
- seed-builder errors;
- guest SSH user (`debian` or `root`);
- user data that may block cloud-init;
- TCG performance versus `connect_timeout_secs`.

### A stale pidfile remains

The provider validates the process command line before treating it as its QEMU process.
Do not signal the recorded PID manually unless you have independently verified it. A
normal destroy uses QMP first and cleans the instance directory.

## Linode

- Set `LINODE_TOKEN` in the environment; do not commit it to `ifx.toml`.
- API error responses are surfaced with endpoint context and response fields.
- A label collision may be an adoption candidate; confirm account, region, and stack
  naming before applying.
- Destroy paid labs when finished.

## Reporting a problem

Include the ifx version, command, stack type, provider, verbose error, and a minimal
redacted stack. Never attach secrets, private keys, state exports, or unredacted
connection objects.
