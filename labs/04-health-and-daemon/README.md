# Lab 4: health and the daemon

[`src/main.rs`](src/main.rs) declares an executable check, a TCP probe, and an HTTP
probe in the same graph as the managed files. The Rust lab server supplies the live
endpoint; no external runtime is involved.

```console
$ target/debug/ifx-labs run 4
```

Checks are resources with dependencies, required/optional policy, timeout, and durable
observations. `ifx check` runs them immediately. `ifx status` separates desired-state
drift from health, while scheduled checks continue against the deployed revision even
if a new source edit fails to compile.

Continue with [Lab 5](../05-components/README.md).
