# Proposed IFX function/struct examples

**Executable MVP examples: `ifx/0.2-draft`, 9 September 2026.** The directory
name is retained for existing links. Select the edition with `[language]` in
`Ifx.toml`. See the [specification and current limits](../../../docs/language-next.md).
Checks and compilation make no provider calls. Simulation uses an in-memory host.

From the repository root:

```sh
cargo run -p ifx-lang -- check examples/language/proposal/Ifx.toml
cargo run -p ifx-lang -- compile examples/language/proposal/Ifx.toml --inputs examples/language/proposal/production.toml
cargo run -p ifx-lang -- compile examples/language/proposal/02-environments.ifx
cargo run -p ifx-lang -- simulate examples/language/proposal/03-lifecycle.ifx --applies 2
cargo run -p ifx-lang -- check examples/language/proposal/shared/Ifx.toml
```

The lifecycle run reports `initialized=1`, `visited=2`, `revision-changed=1`,
`directory-observed=2`. Tests also exercise changed inputs, drift and replacement.

## Start here

Read [01-application.ifx](01-application.ifx), then follow its constructor into
[application.ifx](application.ifx) and [shared/src/web.ifx](shared/src/web.ifx).
The complete composition reads:

```rust
let production = Application::new()
    .key("production")
    .name(name)
    .region(region);

return Deployment {
    application: production,
    website_url: production.website_url,
};
```

`let` binds the completed constructor result. `.key` names this infrastructure
instance; `.name` and `.region` supply constructor parameters. `return` exposes an
explicit typed result to the caller. The actual VM is declared in the shared
constructor. It has logical path `["production", "web", "vm"]`, regardless of the
names of local variables or return fields.

| Example | What to inspect | Provider/host API scope |
|---|---|---|
| [01-application.ifx](01-application.ifx) | Root parameters, named result, explicit return, nested constructor | Existing Linode instance kind through schema-backed constructor syntax |
| [application.ifx](application.ifx) | Public struct, `impl`, `new`, scoped composition | Wraps shared web component |
| [shared/src/web.ifx](shared/src/web.ifx) | Reusable package, resource handle fields, read-only method, configuration | Existing Linode fields and in-memory host methods |
| [02-environments.ifx](02-environments.ifx) | Struct defaults, map keys, conditional deployment, fleet constructor returning an empty struct | Same shared component |
| [03-lifecycle.ifx](03-lifecycle.ifx) | Convergence, once/always/on-change, source-order observations | Existing `memory.value` simulator target and host operations |
| [04-values.ifx](04-values.ifx) | Pure function, ordinary struct literal, pure fluent constructor, method, named output struct | Data only; no graph declarations |

The Linode example installs/starts nginx in its deferred configuration body but
does not claim application deployment, firewall configuration, DNS, TLS or actual
provider availability. The returned URL forwards a provider IP; it is not a health
check. Provider fields and observed facts still come from IFX's authoritative Rust
schemas. These samples do not invent new AWS/network/database resource support.
The [earlier infrastructure sketches](../README.md) cover those separate API ideas.

## Package and runtime inputs

[Ifx.toml](Ifx.toml) declares the entry and visible source modules. Its `shared`
dependency points to [shared/Ifx.toml](shared/Ifx.toml), a directory that could become
its own Git repository. Public items are then imported with paths such as
`use shared::web::WebServer;`. Edition 0.2 imports public items; the legacy edition still imports complete source modules.

[production.toml](production.toml) supplies the first example's root `name` and
`region` parameters. From this directory:

```sh
ifx-lang compile Ifx.toml --inputs production.toml
```

The other numbered files define their own `main` for standalone execution. To make one
the entry, select it explicitly with `compile 02-environments.ifx`
(or another numbered file), or change `package.entry`. Loading
those definitions through `[modules]` does not call their entry functions. A source
file's `main` is entry-only, cannot be imported as a reusable function, and need not
be public. Library files define no entry or top-level executable statements.

Git transport, cache/lock behavior and local-path confinement already exist for
legacy packages, as described in the [current guide](../../../docs/language.md).
The same loader enforces edition consistency, declared exports and source confinement for edition 0.2. Private Git authentication,
SSH fetching, transitive dependencies and registry resolution remain separate work.

## Reading constructors consistently

Every `Type::new()` starts a named-parameter builder in this draft. Positional
`Type::new("frontend", "us-east")` is not another supported spelling. A graph
constructor needs `.key(...)`; a pure data constructor rejects it.
Ordinary pure functions and read-only methods keep positional call syntax:

```rust
let name = label("production", "web");
let data = Settings::new().name(name);
let description = data.description();
```

A semicolon finalizes a builder used as a complete `let` initializer or expression
statement. It registers graph declarations locally or produces a data value. It
never provisions a machine. Missing keys/parameters fail before a graph is emitted.
`main` returns either `Unit` or a named output struct. The application publishes
`Deployment`; the values example publishes `ValuesOutputs`. Fleet and lifecycle
publish no outputs. Ordinary helpers/methods may still return scalars or collections.
Pending builders cannot be returned or stored inside other values. Return completed
values with an explicit `return`; omitting a result does not remove infrastructure.

Calling conventions are fixed: ordinary functions are positional and pure; constructors
are fluent and may compose infrastructure. Constructor effect checking validates
identity without changing function syntax. The full spec records the exact rules.

## Current authoring limits

Configuration lambdas use the existing host statement subset. Compute pure helper
results before attaching a configuration and capture the resulting values; helper
calls, new struct syntax, and returns inside host lambdas are not supported yet.
Primitive collection captures retain deferred element types. Collections containing
nominal structs or handles are rejected at the configuration boundary.

Root TOML inputs support scalars and primitive collections. `--set`, nominal struct
inputs, production apply, real SSH/host execution and durable action journals are
separate work. Open these examples with the [VS Code adapter](../../../editors/vscode/README.md)
or the existing Neovim setup for highlighting and constructor completion.
