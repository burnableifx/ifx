# Reading the IFX language examples

Read the numbered examples in order. They illustrate common infrastructure-as-code
patterns using IFX's own proposed authoring surface, not translations of a specific
Terraform or Pulumi provider API. Resource names in sketches are design proposals.

| File | What to look at | Status |
|---|---|---|
| [01-web-server.ifx](01-web-server.ifx) | One VM, inputs, ordered nginx setup and exported URL | Compiles today; configuration can be simulated |
| [02-environments.ifx](02-environments.ifx) | Reusable module instances generated from a map; optional monitoring | Compiles today; imports [machine.ifx](machine.ifx) |
| [03-linode-web-stack.sketch.ifx](03-linode-web-stack.sketch.ifx) | VPC, database, VM pool, shared load balancer, firewall and DNS | Proposed resource/field APIs |
| [04-aws-network.sketch.ifx](04-aws-network.sketch.ifx) | Public/private subnets, route tables, NAT and an S3 endpoint | Proposed AWS APIs and explicit `depends_on` builder |
| [05-application-lifecycle.sketch.ifx](05-application-lifecycle.sketch.ifx) | Convergence, one-time setup, migrations, reloads and runtime observations | Proposed command/reload host APIs; existing policy syntax |

All five use the current parser's grammar. A `.sketch.ifx` file is intentionally
not accepted by the current semantic checker: it will report unsupported resources
or methods. These files explore the language's feel; they are not runnable deployment
recipes or approved Burnable resource packages. Existing IFX provider resources may
also have different field names from these sketches. No live resources are created.

The first two can be checked from the repository root:

```sh
cargo run -p ifx-lang -- check examples/language/01-web-server.ifx
cargo run -p ifx-lang -- check examples/language/02-environments.ifx
cargo run -p ifx-lang -- simulate examples/language/01-web-server.ifx --applies 2
```

That simulation executes configuration against in-memory records; it does not boot
a VM or install nginx. Provider output references in the compiled graph stay deferred.

For the compact original fixtures, see [configure.ifx](configure.ifx) for observable
policy counters, [linode.ifx](linode.ifx) for keyed resource generation, and
[modules.ifx](modules.ifx) / [machine.ifx](machine.ifx) for typed inputs/outputs across
files. The [language guide](../../docs/language.md) specifies current behavior.

A few design points to inspect while reading:

- Stable names belong in constructor arguments; local symbols can be renamed.
- Resource outputs wire dependencies without an explicit await.
- Loops generate separate backend/association resources, avoiding mutable handle lists.
- Configuration lambdas execute in source order; ordinary `.ensure()` and action
  selection policies have different jobs.
- `on_change` watches explicit values. It is not yet a subscription to a file's
  observed drift or a service restart notification mechanism.
- Credentials/secret delivery are not designed by these sketches. The database
  example shows address wiring only; a complete application needs that contract too.
