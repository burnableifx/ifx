# Lab 9: Deployment Explorer

[`src/main.rs`](src/main.rs) creates a small local deployment whose data dependencies,
health checks, and live state are visible in the interactive graph.

```console
$ target/debug/ifx-labs run 9
```

The runner also writes `/tmp/ifx-lab/09/deployment.html`, a self-contained offline
snapshot. The live Explorer is served by ifxd at `/explorer`; it adds build phase,
generation, diagnostics, current revision, resource actions, health, durable runs,
manual check/drift controls, retry, cancellation, and approval actions.

Use the view selector to reshape the same graph as:

- build dependencies;
- resource utilization;
- logical versus physical layout;
- human-facing versus computer-facing resources.

A failed edit is fail-closed for plan/apply/refresh, but the Explorer retains the last
deployed topology with an explicit stale-build banner. Fix the source or run
`ifx build` to trigger a manual retry.
