# IFX language proposal: functions, structs and scoped construction

Status: **testable authoring MVP, 9 September 2026 — `ifx/0.2-draft`**.
The Rust compiler and LSP now accept the [application, fleet, lifecycle and data
examples](../examples/language/proposal/README.md). Select this edition explicitly
in `Ifx.toml`; omitted editions retain the [legacy language](language.md).

Implemented: nominal structs, immutable bindings, pure positional functions and
methods, fluent `new()` constructors, explicit returns, scoped graph keys,
public item imports, TOML root inputs, schema-based provider lowering, constructor
completion, cross-file navigation, and syntax/semantic highlighting. The same
in-memory host engine evaluates attached configurations.

This document specifies the language direction as well as the implemented core.
The following remain **explicitly deferred**:

- Calls to user helpers, struct literals and `return` inside configuration lambdas;
  compute helper results before `.configure` and capture them. Host lambdas retain
  the existing host statement subset. Collections of structs/handles cannot yet
  be captured; scalar/primitive collection captures preserve typed references.
- TOML construction of nominal structs, `--set`, a serialized public result-type
  schema, and equality for nominal structs/collections containing them. TOML
  currently supplies scalar and primitive collection parameters; outputs retain
  encoded handles and deferred projections.
- General provider type generation beyond the two schema-backed aliases
  `ifx::linode::Instance` and `ifx::memory::Value`, signature-help, diagnostic
  related locations, and availability/effect semantic-token modifiers. Current
  completion gives fields/setters and signatures; highlighting classifies syntax.
- Real hosts, durable journals, Burn CLI integration and managed admission.

Calling conventions are fixed: ordinary functions are positional and pure;
constructors are fluent and may compose infrastructure. Effect checking validates
identity requirements without changing function call syntax.

## 1. The authoring model

An IFX source file contains imports, structs, functions and `impl` blocks. Importing
a file loads definitions; it never instantiates infrastructure. A reusable infrastructure module
is an exported type whose constructor composes resources. Ordinary functions compute values. A project
has one entry file with a `main` function. There are no executable file-level
statements in this edition.

```rust
use crate::application::Application;

pub struct Deployment {
    pub application: Application,
    pub website_url: String,
}

fn main(name: String = "customer-portal", region: String = "us-east") -> Deployment {
    let production = Application::new()
        .key("production")
        .name(name)
        .region(region);

    return Deployment {
        application: production,
        website_url: production.website_url,
    };
}
```

The four former declaration forms have ordinary language equivalents:

| Current form | Draft replacement | Meaning retained |
|---|---|---|
| `input region: String = ...;` | A typed function/constructor parameter | Caller-supplied data and defaults |
| `resource web = ...;` | `let web = Instance::new()...;` | Declare a provider resource and bind its handle |
| `module app = ...;` | `let app = Application::new()...;` | Expand a reusable component in a stable namespace |
| `output url: String = ...;` | Explicit `return` of a typed value | Expose selected values to callers |

The entry can return `Unit`, and a graph constructor can return an empty struct while still declaring resources. Returning a value never
causes provisioning, and not returning a resource never removes its declaration.
The compiler must not eliminate declarations because their handles are unused.

## 2. Values, bindings and structs

`let` creates an immutable lexical binding. Its type is inferred unless annotated:

```rust
let region = "us-east";
let ports: List[Int] = [80, 443];
```

Assignment, mutable bindings, shadowing within an overlapping lexical scope and
implicit conversions are excluded from the first implementation of this edition.
Repeated names in separate function bodies or separate loop iterations are valid.
Values have value semantics; copying a resource handle preserves its identity and
does not duplicate the resource. There is no borrowing or ownership syntax.

Types are `String`, signed 64-bit `Int`, `Bool`, `Unit`, `List[T]`, `Map[T]`, named
structs and generated provider handle types. `Map[T]` has String keys. These
collection spellings retain the current language convention; generic user types,
traits, inheritance, enums, optional values and floating-point arithmetic are
outside this draft's first implementation. `Self` means the enclosing impl type.

Structs are nominal, with typed fields and optional defaults:

```rust
pub struct Settings {
    pub name: String,
    pub region: String = "us-east",
}

let settings = Settings { name: "frontend" };
```

A struct literal creates data, with no graph effect of its own. Field expressions
must already be ordinary values or finalized handles. Unknown/duplicate fields,
missing nondefaulted fields and mismatched types are errors. Field shorthand and
struct-update syntax are deferred: write `name: name` explicitly.

Items are private to their source file unless `pub`. A public struct's fields
remain private unless individually `pub`. Its own `impl` can initialize/read all
fields; other code can read public fields. External struct literals require all
fields to be public. A public function's interface cannot expose a private named
type. Re-export syntax is deferred.

Defaults are closed, known expressions: literals, collections, struct literals
and pure function calls over known values. They cannot reference other parameters,
resource outputs, host observations, environment variables or files. Defaults are
type-checked even when a caller overrides them. Omitted parameters/fields receive
their defaults exactly once; required ones must be supplied.

## 3. Functions, constructors and effects

Functions have typed parameters and an explicit result type when returning a
value. Omitting `-> ...` means `Unit`. Every reachable exit from a non-Unit function
must use `return expression;`. Unit functions may fall through or use `return;`.
Bare trailing expressions and anonymous objects are not implicit returns.

```rust
fn label(prefix: String, suffix: String) -> String {
    return prefix + "-" + suffix;
}

let name = label("production", "web");
```

Named function calls use lexical/static resolution. Forward references are allowed;
recursive call cycles, including defaults and constructors, are rejected. Function
values, arbitrary closures and overloading are deferred. There is one signature
per name; only one `new` constructor per struct. An inherent `impl` must be in the
same file as its own user-defined struct. Foreign/imported types and generated
provider handles are sealed: user code cannot add impls or replace their constructors.
All impl blocks for a type share one method namespace and cannot redeclare a method.

### Function effect classification

The analyzer infers an effect from each body and its statically resolved callees:

| Effect | Allowed work | Call shape |
|---|---|---|
| Pure | Compute values, construct data, read existing handles as typed references | `label("production", "web")` |
| Graph constructor | Declare provider resources or invoke another graph constructor | `Application::new().key("production").name("frontend")` |
| Host | Observe/change a host through its supplied capability | Restricted to configuration/policy lambdas in this increment |

Ordinary functions and read-only methods are always pure and use positional calls.
They may read finalized resource handles but cannot construct infrastructure,
directly or through a constructor. Only constructors and the selected `main` may
build a graph. The checker infers whether a constructor builds infrastructure to
validate `.key(...)`; inference never changes ordinary function call syntax.
A constructor stays graph-building even when a known branch produces no resources.
Constructor completion includes `.key` only when identity is required.

Pure functions accept positional arguments in declaration order; only trailing
defaulted parameters may be omitted. Constructor calls accept no positional
arguments: the empty call creates a builder with named parameter setters. `.key`
is explicit metadata, never a hidden first function parameter.

`main` is the entry exception: the CLI supplies its parameters directly and invokes
its body once inside the selected stack root. It needs no `.key(...)` and cannot
be imported/called as a reusable component. It can be Pure or Graph. An ordinary
function cannot use `self`; a static constructor cannot use a receiver.

### Struct constructors and methods

```rust
pub struct Application {
    pub website_url: String,
}

impl Application {
    pub fn new(name: String, region: String = "us-east") -> Self {
        let web = WebServer::new()
            .key("web")
            .name(name)
            .region(region);

        return Self { website_url: web.website_url };
    }
}
```

`new` is an associated constructor returning exactly `Self`. At a call site,
`Type::new()` always starts a typed builder, for both pure and graph constructors.
Parameters become named setters; constructor arguments inside `new(...)` are
rejected. This is an intentional IFX convention, not Rust constructor semantics.
Constructors are explicitly defined, not synthesized from struct fields; struct
literals remain available for simple data.

```rust
let settings = Settings::new().name("frontend"); // Pure constructor: no key.
let app = Application::new().key("production").name("frontend");
```

A graph constructor requires `.key(...)`; a pure constructor rejects it. Ordinary
read-only instance methods such as `address(self) -> String` are pure, use normal
positional calls, and may read fields. `self` is the first parameter and is supplied
by the receiver. Static associated helpers other than `new` are pure and use positional calls. Mutating or graph-declaring instance
methods and user-defined Host functions are deferred.

A parameter cannot take a name reserved by its builder: `key`, or `configure` on
generated resource builders. Provider fields that collide need an explicit alias
in authoritative Rust schema metadata (for example an object-storage `key` field
could expose `object_key`). Schema generation must fail on unresolved collisions;
it must not silently hide a provider field. `new` is reserved for constructors.

## 4. Builder finalization and stable identity

A builder is temporary authoring state. Setters return an updated builder; they do
not perform provider calls. A builder must occupy the complete initializer of a
`let` statement, or a complete expression statement. Reaching that statement's
semicolon finalizes it once. The bound value is the completed constructor result
or provider handle, never a reusable builder.

Finalization validates required fields, duplicate setters, types and identity;
then evaluates a user constructor or registers a provider resource
in the in-memory graph. Failed expansion produces no executable compilation.
Check and compile evaluate this graph-building logic but perform no host/provider
operations. The entire graph must validate before an apply may start.

```rust
let web = Instance::new()
    .key("web")
    .label("frontend")
    .region("us-east")
    .type("g6-nanode-1")
    .image("linode/debian12");
```

Unfinished builders cannot be returned, passed as arguments, placed in collections
or structs, or reused after binding. Bind a completed result, then return/pass that
value. Calling `.region(...)` on a completed resource handle is an error; make a
configuration struct or a reusable constructor when several declarations share
settings. Duplicate setters are errors even when their values are equal.

A graph constructor key opens a namespace for declarations in its body.
A provider resource key names a leaf within the current namespace. Constructor
names, local variable names, source paths, import aliases and return-field names
are not identity components.

For the application example:

| Declaration | Logical key path |
|---|---|
| `Application::new().key("production")` | `["production"]` |
| Its `WebServer::new().key("web")` | `["production", "web"]` |
| That constructor's `Instance::new().key("vm")` | `["production", "web", "vm"]` |

The stack/account identity scopes this path externally. Resources also carry their
canonical provider kind, such as `linode.instance`. Edition 0.2 encodes every full key path as a JSON String array in the existing
IFX URN key slot, including a one-element root path. Thus a root key containing
literal JSON cannot alias a nested path. For example `Value::new().key("worker")`
lowers to `memory.value:["worker"]`. Legacy edition identity encoding is unchanged;
there is no implicit state migration between editions. Sibling graph invocations cannot reuse a namespace key. Resources
cannot duplicate the same provider kind and complete key path. Reusing a handle
or returning it twice is not another invocation and does not reserve a new key.

Keys must be known, nonempty Strings, at most 128 UTF-8 bytes, without control
characters. Variables and map keys are valid. A loop index must never be generated
implicitly as identity. Renaming a key or moving a resource to another namespace
changes identity; a future planner must surface the resulting create/destroy work.
There is no automatic state migration in this draft.

## 5. Provider resources and deferred values

Provider types are generated from trusted Rust schemas. `use ifx::linode::Instance;`
and `use ifx::memory::Value;` name schema-backed aliases corresponding to existing
`linode.instance` and `memory.value` kinds. The `ifx` namespace is reserved and has
no network resolution. Edition 0.2 uses these aliases; the legacy edition retains
`linode.instance(...)` and `memory.value(...)`.

Resource configuration and observed state should be struct-shaped in the catalog:

```rust
// Schema illustration, not a new user-defined provider implementation.
pub struct InstanceConfig {
    pub label: String,
    pub region: String,
    pub type: String,
    pub image: String,
}

pub struct InstanceState {
    pub id: Int,
    pub ipv4: String,
}
```

Schema enums remain field constraints with completion/validation; user-defined enums
are deferred. These illustrative subsets do not override current schema requirements, defaults,
field types or validators. The canonical Rust provider contract binds configuration,
observed state and lifecycle operations to the generated handle type. A user-defined
struct with the same fields grants no provider capability. User code cannot forge
an `Instance` with a struct literal. New providers/lifecycle implementations remain
Rust work; this DSL revision does not introduce provider plugins or FFI.

Ordinary type and availability are distinct. `web.ipv4` has type String but is a
deferred typed reference until provider execution supplies it. A returned struct
can contain such references or handles; no await, polling or fabricated value is
inserted by the authoring tools. Reading an observed field introduces dependency
information where that value is consumed, not a blocking network request.

Known values may choose graph branches, iteration, keys and type/field selection.
Deferred values may flow through fields, parameters and returns into provider
inputs and configuration captures. They cannot choose graph shape. Handle inputs
must retain typed dependency edges when supported by the receiving schema.

For the first implementation, retain the existing lowerable deferred operations:
reference forwarding, supported field/index projections and String concatenation.
A pure helper is usable with deferred arguments only if its evaluated operations
are representable that way. Reject graph-time deferred comparisons/arithmetic;
do not erase operands or replace unknown booleans with defaults. Inside configuration,
required outputs are resolved before host execution and ordinary operations can
use those concrete values. Compile referenced pure helper bodies into the closed
local configuration program; execution uses only that validated program and its
captures, never runtime source discovery or Git fetching. General future/expression
evaluation is deferred.

## 6. Collections and control flow

List literals use `[a, b]`; map literals use `{ "name": value }`. Record literals
always name a struct type, such as `Environment { name: "staging" }`. Empty
collections require enough expected type information; lists and map values must
have one compatible type. Struct/map literals in `if` or `for` headers require
parentheses to avoid ambiguity with the following block.

`for value in list` and `for key, value in map` create a fresh lexical scope per
iteration. Maps iterate by lexical String-key order. `if condition { ... } else
{ ... }` requires Bool. Conditions and iteration inputs must be known while
building a graph; they may use observed values inside a configuration lambda.
Loops are statements, not collection comprehensions, in the first implementation.
There is no mutation-based accumulator, `while`, `break` or `continue` yet.

The initial operators remain `!`, checked Int addition, String concatenation,
`==` and `!=`, with the current precedence. Equality is defined for primitive
values and recursively compatible data collections/structs, not resource handles.
A map/list can contain finalized handles; bounds and dependency typing still apply.

## 7. Ordered configuration and action policies

`.configure("bootstrap", |host| { ... })` remains a generated provider-resource
builder operation. It attaches a deferred configuration program to that resource.
It is not a general method on arbitrary structs or completed resource handles.
Multiple configuration attachments require distinct stable keys and run in source
order in the MVP. Captured values are immutable and retain deferred dependencies.

Lambdas occur only at `.configure`, `once`, `always`, and `on_change`. Their host
capability cannot escape, be returned, stored in a struct, or passed to an arbitrary
function. Named pure helpers may compute arguments; user-defined host executors are
outside this increment. Configuration cannot instantiate graph builders, including
indirectly through constructors or functions. Struct data construction remains valid.

| Operation | Behavior on an explicit apply |
|---|---|
| `host.file(...).content(...).ensure()` and other desired-state operations | Observe and converge only differences when reached |
| `host.once(key, || { ... })` | Run until confirmed success for that action and target incarnation |
| `host.always(key, || { ... })` | Run each apply when reached |
| `host.on_change(key, watched, || { ... })` | Run initially and when the resolved watched value differs from last success |

Bodies run in source order; later observations see earlier successful mutations.
A failed operation stops the body/apply path. Nested policy blocks and duplicate
reached action keys are rejected. Ordinary ensures inside a successful `once` body
will not repair subsequent drift, because the body is skipped. `on_change` is an
explicit-value trigger, not implicit file watching or service reload propagation.

Action identity includes stack/module/resource identity, configuration key, action
key and target incarnation. Record uncertain intent before effects; record success
only after the complete body succeeds. Uncertain outcomes require reconciliation,
not blind automatic retry. Replacement changes incarnation and permits `once` to
run again. This preserves the existing simulator contract; it does not claim
exactly-once external execution or introduce production persistence.

Host observations, commands, files and SSH remain on the machine running IFX/burn.
Managed IFXD brokers typed provider API operations only. No source file, lambda,
package constructor, shell command or SSH transport becomes a managed execution
endpoint. The current validation target remains the in-memory host. This draft
adds no real command runner, payment behavior, provider calls or credential loading.

## 8. Files, imports and packages

Keep the existing explicit package scope and pinned Git distribution model. Within
a declared source module, `pub` selects importable types/functions. Imports target
items rather than treating an entire file as an implicit callable lambda:

```rust
use crate::application::Application;
use shared::web::WebServer as Server;
use ifx::linode::Instance;
```

`crate::` refers to the current package's `[modules]`; another first segment must
be a declared direct dependency alias or reserved builtin namespace. After resolving
a manifest module path, the final segment must name a public item in that source.
Qualified manifest names remain supported; ambiguous resolutions are errors.
Grouped imports, glob imports, re-exports, implicit directory scanning and `mod`
declarations are deferred. File imports are acyclic in this first implementation.

```toml
[language]
edition = "0.2-draft"

[package]
name = "production"
version = "0.1.0"
entry = "main.ifx"

[modules]
application = "application.ifx"

[dependencies]
shared = { path = "shared" }
```

The `[language]` table selects syntax and is separate from package version.
The compiler rejects unsupported editions before graph evaluation. Omitting it retains legacy behavior
for existing projects; edition 0.2 does not accept the old `input`, `output`,
`resource`, `module` binding forms or `import ... from` syntax. Mixing source editions
within a package or depending on another edition is rejected in the first increment;
a later typed interoperability layer requires its own compatibility contract.

A library omits `package.entry`; a root entry defines one `fn main(...)`.
`compile Ifx.toml` selects `package.entry`. An explicit `compile path/to/file.ifx`
selects that file's `main` for this invocation, using its containing project's
scope and parameters; the file must be the manifest entry or a declared module.
Other loaded files may define their own `main`, but those definitions stay inert
and cannot be imported/called. `check` can validate a library without a `main`;
only a selected entry with supplied parameters is expanded to validate its graph.
Check all loaded definitions without executing imports. Library exports can be checked without
requiring a root `main`. Instantiation occurs only from the explicit entry path.

Retain current fetch boundaries: explicit `ifx-lang fetch`, full commit SHA pins,
`Ifx.lock`, verified source bytes in `.ifx/deps/`, confined local paths, no automatic
editor fetch, no dependency build scripts, and no transitive packages in the MVP.
The consumer alias is independent of the dependency package's declared name. Cache
integrity checks do not authenticate a modified lockfile or publisher. Private Git
authentication, SSH fetching and a registry remain separate increments.

## 9. Root parameters and externally visible results

The root `main` parameters are the runtime-input interface. Implemented CLI syntax:

```sh
ifx-lang compile Ifx.toml --inputs production.toml
```

The parameter file is a TOML table whose keys exactly match root parameter names.
The MVP accepts primitive lists/maps recursively; nominal struct inputs are deferred. Planned scalar `--set
name=value` supports String/Int/Bool according to the declared parameter type;
String values are literal after shell processing. Complex values use the TOML file,
not an ad hoc expression parser. No parameter input is executable DSL.

Current precedence is parameter default < one explicit input file. A future `--set` layer would override the file. Duplicate CLI
keys, unknown keys, missing required parameters and invalid supplied values are
errors even if a later layer would override them. Defaults are independently
checked. The same resolved inputs feed check, compile and simulate; no ambient
environment/file discovery supplies missing values. Resource handles and deferred
values cannot be fabricated as external inputs.

The root result is an explicit API value. The target result contract will carry its type as well as encoded
reference projections; the current envelope carries only values/projections alongside the complete graph and configurations. Root results
may include handles/structs. A handle serializes as its canonical kind/URN reference,
never as its provider credentials or local transport capability. Host/connection
capabilities are not legal public result values in this edition. An apply consumer
can render resolved results after successful execution; Unit yields no result payload.
No new production apply/result renderer is claimed by this draft.

## 10. Core grammar

The following EBNF defines the proposed syntax, with semantic constraints in the
preceding sections. `path` includes single names; `::` and `->` are tokens. `Self`
is allowed where a type path/record constructor is expected.

```ebnf
file        = { use | struct | function | impl } ;
use         = "use", path, ["as", identifier], ";" ;
struct      = ["pub"], "struct", identifier, "{", [fields], "}" ;
fields      = field, {",", field}, [","] ;
field       = ["pub"], identifier, ":", type, ["=", expression] ;
function    = ["pub"], "fn", identifier, "(", [parameters], ")",
              ["->", type], block ;
parameters  = parameter, {",", parameter}, [","] ;
parameter   = "self" | identifier, ":", type, ["=", expression] ;
impl        = "impl", path, "{", {function}, "}" ;
type        = path | "List", "[", type, "]" | "Map", "[", type, "]" ;
path        = identifier, {"::", identifier} ;
block       = "{", {statement}, "}" ;
statement   = "let", identifier, [":", type], "=", expression, ";"
            | "return", [expression], ";"
            | "if", expression, block, ["else", block]
            | "for", identifier, [",", identifier], "in", expression, block
            | expression, ";" ;
expression  = equality ;
equality    = sum, {("==" | "!="), sum} ;
sum         = unary, {"+", unary} ;
unary       = "!", unary | postfix ;
postfix     = primary, {".", identifier | "(", [arguments], ")"
                      | "[", expression, "]"} ;
arguments   = expression, {",", expression}, [","] ;
primary     = string | integer | "true" | "false" | path
            | path, "{", [members], "}"
            | "[", [arguments], "]" | "{", [entries], "}"
            | "(", expression, ")" | lambda ;
members     = identifier, ":", expression, {",", identifier, ":", expression}, [","] ;
entries     = string, ":", expression, {",", string, ":", expression}, [","] ;
lambda      = "|", [identifier], "|", block ;
```

Identifiers follow the current ASCII letter/underscore then letter/digit/underscore
rule. Strings retain JSON escapes and Unicode; `//` comments extend to line end.
Int literals include negative decimal values. The grammar deliberately has no Rust
macros, arbitrary attributes, lifetimes, unsafe blocks, FFI or dynamic loading.
`self` is valid only as the first parameter of an instance method; imports need at
least a namespace/module and item. `configure` accepts one lambda parameter; policy
lambdas accept none. A lambda has Unit result. The current host subset rejects `return` in lambdas;
a future extension may permit only a final outer-body return, so early returns
cannot skip action work and incorrectly mark a policy successful.

Retain bounded parsing/evaluation: 256 KiB/source, 32 sources/1 MiB aggregate,
24,000 tokens/source, nesting 48, 256 collection entries, 128 graph resources and
128 configurations, and 10,000 expansion steps. Also retain the 64 KiB expanded-value,
256 KiB environment-data and 128-binding limits. Check allocation budgets incrementally
before concatenation, collection growth, copying arguments or capturing values; reject
overflow before allocating the expanded result. Add call-stack limit 32 and graph
namespace depth 8. Charge constructors, helpers, defaults and loops to the same
work budget; never reset it for nested calls. The checker examines all branches
for types/effects without host execution; the graph evaluator expands only known
selected branches. Definition/call caches cannot bypass budgets.

## 11. LSP and implementation acceptance

The CLI and LSP must share parsing, symbol resolution, effects, parameter/default
checking, resource schemas and availability tracking. Opening the editor must never
finalize a deployment against a provider or execute a host body.

| Authoring action | Required feedback |
|---|---|
| Type `use shared::web::` | Complete public items from declared packages |
| Type `Application::` | Show constructor/static helpers and their signatures |
| Type `.new().` | Complete unassigned constructor parameters and `.key` when required |
| Type `deployment.` | Complete public return fields/methods, not builder setters |
| Omit `.key` or a required parameter | Diagnostic at the finalization statement with declaration context |
| Return the wrong struct/field type | Diagnostic at return/field and expected signature |
| Use an observed IP in a key or graph branch | Explain which operation needs a known value |
| Navigate an imported constructor or public field | Correct cross-file source location |
| Edit an imported struct/function unsaved | Recheck affected consumers using the same workspace snapshot |

The VS Code adapter includes an IFX TextMate grammar; the server provides full
semantic tokens for keywords, types, functions, variables, properties, strings
and numbers. Comments are handled by TextMate. Known/deferred/effect modifiers
and parameter-specific semantic classification remain future work. Cross-file
navigation and incomplete constructor-chain completion are covered by protocol
tests and a real Neovim session. A VS Code UI session remains unverified.

The implementation follows these vertical slices, retaining executable legacy examples:

1. Edition gate, structs, immutable bindings, pure functions/methods, explicit return
   checking and source navigation. Prove the values example without graph effects.
2. Generated resource constructors and constructor effect checking; builder
   finalization, `.key` validation and unchanged canonical resource graph lowering.
3. User constructors/scoped composition, public cross-file items and manifest resolution.
   Prove the nested application and environments examples plus CLI/LSP agreement.
4. Typed root parameter files/overrides and explicit result envelopes; preserve
   deferred references and resource handles without exposing runtime capabilities.
5. Carry configuration captures through the new frontend; prove policy behavior
   with the in-memory host and complete the authoring walkthrough in a real editor.

Each slice includes its LSP surface, diagnostics and tests. The explicit deferrals above remain outside this MVP. Real hosts, durable journals, Burn CLI integration and managed admission
remain separate product work.

### Required behavioral vectors

- Renaming a local binding, constructor symbol, import alias or return field keeps
  provider resource identities; changing `.key` changes identity intentionally.
- Reordering a String map preserves resource identity and deterministic output;
  duplicate scoped calls/resources fail even when bound results are unused.
- Missing required/default-invalid parameters, duplicate setters, wrong/ private
  fields and unresolved public return types fail before graph emission.
- Pure helpers/data constructors produce no resources; graph constructors returning
  empty structs retain theirs. Storing/returning a pending builder and positional constructor calls fail.
- Imports do not instantiate resources. Cyclic imports/calls and work-budget overflow
  produce bounded diagnostics. Adding a deferred comparison never fabricates a value.
- Nested constructor returns preserve reference type, projection and canonical URN;
  handing a result to another provider input retains its dependency edge.
- Runtime parameter files cannot supply unknown keys, expressions, fake handles or
  invalid values masked by higher-precedence overrides.
- Two unchanged lifecycle applies yield initialized=1, visited=2, revision-changed=1,
  directory-observed=2. Changing revision triggers that action again; deleting the
  once-created directory does not rerun initialization; replacing its target does.
- A failing policy leaves uncertainty and blocks replay until reconciled. Host
  capabilities cannot escape lambdas or enter managed IFXD requests.
- CLI/LSP agree on each error; incomplete `use`, `impl`, field and builder chains
  remain navigable and highlighted. Parser fuzzing includes the new syntax.

## 12. Review points and explicit deferrals

The September 9 refinement fixes calling conventions: ordinary functions are
positional and pure; `new()` constructors are fluent and may compose infrastructure.
Constructor effect checking controls whether `.key` is required without changing
function call syntax. The `Fleet` example replaces the earlier free graph function.
This narrows the first implementation while retaining the user-approved authoring path.

Keep the first implementation focused on these constructs. Do not add a general
Rust compiler, ownership system, macros, arbitrary closures, user generics, mutable
state, shell evaluation, provider plugins or a registry as incidental parts of it.
Existing resource metadata remains the authority for provider behavior. Existing
source fingerprints, simulator policy semantics and the local/managed execution
boundary remain the compatibility baseline.
