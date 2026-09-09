# Proposed IFX function/struct examples

**Design examples only: `ifx/0.2-draft`, 9 September 2026.** The current compiler and
LSP do not accept these sources or the proposed `[language]` manifest selector.
See the [draft specification](../../../docs/language-next.md) for their rules and
[executable examples](../README.md) for what runs today. No cloud resources are
created by reading these examples. No syntax, provider or host implementation is
included in this proposal.

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
| [01-application.ifx](01-application.ifx) | Root parameters, named result, explicit return, nested constructor | Existing Linode instance kind through proposed generated syntax |
| [application.ifx](application.ifx) | Public struct, `impl`, `new`, scoped composition | Wraps shared web component |
| [shared/src/web.ifx](shared/src/web.ifx) | Reusable package, resource handle fields, read-only method, configuration | Existing Linode fields and in-memory host methods |
| [02-environments.ifx](02-environments.ifx) | Struct defaults, map keys, conditional deployment, graph function returning Unit | Same shared component |
| [03-lifecycle.ifx](03-lifecycle.ifx) | Convergence, once/always/on-change, source-order observations | Existing `memory.value` simulator target and host operations |
| [04-values.ifx](04-values.ifx) | Pure function, ordinary struct literal, pure fluent constructor, method | Data only; no graph declarations |

The Linode example installs/starts nginx in the proposed configuration body but
does not claim application deployment, firewall configuration, DNS, TLS or actual
provider availability. The returned URL forwards a provider IP; it is not a health
check. Provider fields and observed facts still come from IFX's authoritative Rust
schemas. These samples do not invent new AWS/network/database resource support.
The [earlier infrastructure sketches](../README.md) cover those separate API ideas.

## Package and runtime inputs

[Ifx.toml](Ifx.toml) declares the entry and visible source modules. Its `shared`
dependency points to [shared/Ifx.toml](shared/Ifx.toml), a directory that could become
its own Git repository. Public items are then imported with paths such as
`use shared::web::WebServer;`. The new item-import semantics are part of this draft;
the current implementation imports complete source modules.

[production.toml](production.toml) supplies the first example's root `name` and
`region` parameters. The future command would be:

```sh
# PROPOSED command; --inputs and --set are not implemented yet.
ifx-lang compile Ifx.toml --inputs production.toml --set region=us-central
```

The other numbered files define their own `main` for standalone review. To make one
the entry under the proposed rules, select it explicitly with `compile 02-environments.ifx`
(or another numbered file), or change `package.entry`. These commands are design
examples until the new frontend is implemented. Loading
those definitions through `[modules]` does not call their entry functions. A source
file's `main` is entry-only, cannot be imported as a reusable function, and need not
be public. Library files define no entry or top-level executable statements.

Git transport, cache/lock behavior and local-path confinement already exist for
legacy packages, as described in the [current guide](../../../docs/language.md).
The new edition selector and public-item semantics must be implemented before
these packages can be fetched/checked as edition 0.2. Private Git authentication,
SSH fetching, transitive dependencies and registry resolution remain separate work.

## Reading constructors consistently

Every `Type::new()` starts a named-parameter builder in this draft. Positional
`Type::new("frontend", "us-east")` is not another supported spelling. A graph
constructor/function needs `.key(...)`; a pure data constructor rejects it.
Ordinary pure functions and read-only methods keep positional call syntax:

```rust
let name = label("production", "web");
let data = Settings::new().name(name);
let description = data.description();
```

A semicolon finalizes a builder used as a complete `let` initializer or expression
statement. It registers graph declarations locally or produces a data value. It
never provisions a machine. Missing keys/parameters fail before a graph is emitted.
Pending builders cannot be returned or stored inside other values. Return completed
values with an explicit `return`; omitting a result does not remove infrastructure.

Effect inference and the special `new()` convention are proposed refinements to
make the conversational examples consistent. They need authoring feedback before
implementation; the full spec records their exact behavior and acceptance cases.
