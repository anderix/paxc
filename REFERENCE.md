# pax Reference

pax is a small domain-specific language for Power Automate cloud flows. The `paxc` compiler emits the JSON that Power Automate expects, and the `paxr` interpreter runs the same source locally so you can see what a flow does before deploying it.

A pax source file is a sequence of statements in the order they should execute. The compiler infers the `runAfter` dependency graph from source order, which means the source you write looks like imperative code even though what gets emitted is a graph of named actions.

pax owns the programmable parts of a flow (variables, control flow, expressions). PA-specific parts (connectors, ParseJson, non-default triggers, connection references) live in JSON files next to the source under a `pa/` folder. paxc reads those files at compile time and drops their contents verbatim into the emitted flow.

## A minimal example

Save this as `hello.pax`:

```
let greeting = "Hello, world!"
debug(greeting)
```

Run it through the interpreter:

```
$ paxr hello.pax
debug: greeting="Hello, world!" at line 4

end state:
greeting (let) = "Hello, world!"
```

The first line is the debug statement firing. The section after `end state:` is paxr's dump of every binding and the value it settled on at the end of the run.

Compile the same source for Power Automate:

```
$ paxc --target pa-legacy --name hello --out hello.zip hello.pax
note: dropped 1 debug() statement
```

`hello.zip` is a legacy-format package you can import through Power Automate's **My flows > Import > Import Package (Legacy)** path. The imported flow contains a manual trigger and a single Compose action named `Compose_greeting`. When you run the flow, "Hello, world!" appears in its run history as the output of that Compose.

Two observations matter here. The `let greeting = ...` line is a Compose in both worlds: paxr captures it in its state dump, and paxc compiles it to an actual PA Compose action. The `debug(greeting)` line is paxr-only: paxc strips every debug call from the emitted flow and writes a note to stderr saying how many it dropped. One source file, two observable executions, both showing the same string.

## Triggers

Triggers are file-based. paxc scans the source directory's `pa/` folder for a single `*.trigger.json` file at compile time. The filename minus the `.trigger.json` suffix becomes the PA trigger key, and the file's contents are dropped verbatim into the emitted flow's `definition.triggers`.

If no trigger file is present, paxc generates a default manual ("Button") trigger so a fresh pax source compiles without any setup. Two or more `*.trigger.json` files under `pa/` is a compile error -- a flow can only have one trigger.

### Default manual

With no `pa/*.trigger.json` next to the source, paxc emits:

```json
"triggers": {
  "manual": {
    "type": "Request",
    "kind": "Button",
    "inputs": {}
  }
}
```

That covers the common case: write a pax program, compile it, get a Button-triggered flow.

### File-based triggers

For anything else (Recurrence, HTTP request, connector webhook), put a JSON file under `pa/` named after the trigger. Example: `pa/Recurrence.trigger.json` for a 5-minute schedule:

```json
{
    "type": "Recurrence",
    "recurrence": {
        "frequency": "Minute",
        "interval": 5
    }
}
```

The filename's stem (`Recurrence`) becomes the trigger's key in `definition.triggers`. The file's content matches PA's "Code View" / "Peek code" output exactly, which makes round-trip from a real PA flow byte-identical.

paxr does not execute triggers, so a flow with a non-default trigger runs through the interpreter exactly like a manual one.

## Variables and types

```
var counter: int = 0
var rate: float = 1.5
var greeting: string = "hello"
var active: bool = true
var tags: array = ["urgent", "review"]
var config: object = {
  "region": "us-east",
  "retries": 3,
}
```

A `var` declaration compiles to a Power Automate `InitializeVariable` action named `Initialize_<name>`. The six v1 types are `int`, `float`, `string`, `bool`, `array`, and `object`. Float literals require at least one digit after the decimal point (`1.5`, `0.25`), so identifier access like `obj.field` is never ambiguous. Int and float mix freely in arithmetic and comparisons: any float operand promotes the result to float, while int operated on int stays int (including `/`, which matches Power Automate's integer-division behavior). An int literal assigned to a float-typed variable is coerced at initialization, so `var budget: float = 5` followed by `budget += 0.5` gives `5.5`, not an int. Array and object literals use JSON-like syntax, and trailing commas are permitted.

The initializer is optional. `var todo: string` (no `= ...`) emits an `InitializeVariable` whose `value` field is omitted; Power Automate then runs the variable up at the type's zero value (0 for int, 0.0 for float, empty string for string, false for bool, empty array for array, empty object for object). The paxr interpreter mirrors that behavior locally. The no-initializer form is the default shape produced by the Power Automate designer, so it's what you typically see when round-tripping an existing flow into pax.

One paxr-specific note on equality: the interpreter treats `5 == 5.0` as true so numeric comparisons across the two types behave sanely during local simulation. Power Automate's expression language uses strict JToken equality and would consider those unequal, so do not rely on cross-type `==` for business logic. The stub-and-fix workflow tests the real flow in Power Automate anyway.

## let and Compose

```
let remaining = total - completed
let region = config.region
```

A `let` binding compiles to a Power Automate `Compose` action named `Compose_<name>`. Compose actions are immutable: you can reference `<name>` in later expressions, but you can't reassign it. Use `let` when you want to capture a derived value without making it a mutable variable.

## Assignment and compound assignment

```
counter = 10
counter += 1
counter -= 1
message &= ", world"
tags += "urgent"
```

Variables declared with `var` support four assignment forms. Plain `=` compiles to `SetVariable`. The compound forms each map to a dedicated Power Automate action: `+=` on an int or float becomes `IncrementVariable`, `-=` on an int or float becomes `DecrementVariable`, `&=` on a string becomes `AppendToStringVariable`, and `+=` on an array becomes `AppendToArrayVariable`. Each assignment is a separate action in the emitted flow. Compose bindings (`let`) cannot be reassigned.

## String literals and escapes

```
var multiline: string = "line one\nline two"
var tabbed: string = "col1\tcol2"
var quoted: string = "she said \"hi\""
var windows_path: string = "C:\\Users\\david"
```

String literals use double quotes. The recognized escape sequences are `\n` (newline), `\t` (tab), `\"` (embedded quote), and `\\` (backslash). Power Automate's expression language uses single-quoted strings internally, so paxc rewrites the quote convention and escapes for you when emitting JSON.

## Expressions

pax expressions cover arithmetic on ints, comparison, boolean logic, string concatenation, and function calls. Operators and their precedence:

| Category | Operators |
|----------|-----------|
| Unary | `!` (boolean not), `-` (numeric negation) |
| Multiplicative | `*`, `/` |
| Additive | `+`, `-` |
| Comparison | `==`, `!=`, `>`, `<`, `>=`, `<=` |
| String concat | `&` |
| Logical AND | `&&` |
| Logical OR | `\|\|` |

Precedence runs from tightest (top of table) to loosest (bottom). Three examples that exercise it:

```
let mixed = 2 + 3 * 4
// 14: multiplicative binds tighter than additive

let pass = completed > 0 || total == 0 && flag
// && binds tighter than ||

var report: string = "Remaining: " & total - completed
// arithmetic binds tighter than &, so the subtraction runs first
```

In the emitted JSON, pax expressions become Power Automate expression strings. Interpolated contexts get `@{...}` wrapping, like `@{concat('count: ', variables('total'))}`. Standalone boolean expressions in an `if` condition get the bare function form, like `@greater(variables('completed'), 0)`. paxc picks the right wrapping based on context.

Parenthesized expressions (`(expr)`) override precedence the same way they do in any C-family language: `!(approved == true)` flips the boolean of the comparison rather than negating `approved` first. This is the form the round-trip decoder reaches for when emitting binary or unary operators that would otherwise re-bind awkwardly inside a larger expression context.

## Member access

```
let region = config.region
let primary = config.endpoints.primary
var announcement: string = config.endpoints.fallback
```

Dot notation reads fields from object variables, object literals, and the iterator variable of a `foreach`. Chains are allowed. The emitted expression uses Power Automate's safe-navigation bracket form: `variables('config')?['endpoints']?['primary']`.

### Subscript form for non-identifier keys

```
let raw = triggerBody()?["body/email"]
let first = items?[0]
var label: string = config?["display name"]
```

When a path segment isn't a valid pax identifier (slashes, spaces, hyphens, leading digits, numeric indexes), use the subscript form `?[<literal>]`. The key must be a string literal or a non-negative integer literal. The dot form `obj.field` is sugar for `obj?["field"]` whenever the field name happens to be an identifier; both compile to PA's `?['field']`. Subscript chains and dot chains mix freely (`triggerBody()?["body/value"].name`).

paxr evaluates subscripts with null-safe semantics: a missing key, an out-of-range index, or a non-matching target type yields `null` instead of erroring. This matches PA's runtime behavior for the `?[...]` form and lets chains over unknown trigger / connector data degrade gracefully when running locally.

### PA accessor calls

paxc's function library recognizes the standard PA expression accessors so they round-trip from real flows: `triggerBody()`, `triggerOutputs()`, `trigger()`, `parameters('<name>')`, `body('<actionKey>')`, `outputs('<actionKey>')`, `actions('<actionKey>')`, `iterationIndexes('<loopKey>')`, and `item()`. They appear in pax source as ordinary call expressions and emit unchanged. paxr can simulate `iterationIndexes('<loopKey>')` (it returns the active foreach iteration counter) but for the others returns `null` with a `<skipping unknown "...">` notice — Power Automate provides the runtime data that paxr can't.

## Control flow

### if / else if / else

```
if all_done {
  label = "done"
} else if remaining > 0 {
  label = "in progress"
} else {
  label = "empty"
}
```

Each `if` compiles to a Power Automate `Condition` action. When the condition is already a comparison or boolean expression, paxc emits it directly. When it isn't (a function call that returns a value, for example), paxc wraps it with `equals(..., true)` so PA gets the boolean shape it requires. `else if` chains compile to nested Conditions inside the `else` branch.

### switch

```
switch status {
  case "active" {
    label = "ACTIVE"
  }
  case "pending" {
    label = "PENDING"
  }
  default {
    label = "UNKNOWN"
  }
}
```

`switch` dispatches on a subject expression and runs the matching case body, falling through to an optional `default` arm if nothing matches. Compiles to Power Automate's `Switch` action. Case values must be scalar literals (string, int, or bool), which matches the same constraint PA enforces on Switch. Arbitrary expressions are not allowed in the case clause.

The `default` arm is optional. When absent, a switch with no matching case is a no-op and the next statement runs normally.

Each case body is scoped independently: a `let` declared inside one case does not leak to other cases or to the outer scope. Nested `var` declarations are rejected the same way they are inside `if` branches.

### foreach

```
foreach task in tasks {
  total += 1
  if task.done {
    completed += 1
  } else {
    pending_titles += task.title
  }
}
```

`foreach` compiles to a Power Automate `Apply_to_each` action. The iterator name (`task` above) is available by dot-access inside the body. Mutations inside the body operate on enclosing-scope variables, which PA runs serially by default.

### until

```
var n: int = 0
until n >= 5 {
  n += 1
}
```

`until` is Power Automate's do-while loop. The condition is the exit condition: the body runs at least once, then the condition evaluates, and the loop exits when the condition becomes true. Compiles to a PA `Until` action.

Two optional trailing clauses tune the loop's safety limits, matching PA's `limit` block:

```
until ok max 5 timeout "PT10M" {
  ...
}
```

`max N` sets the maximum iteration count; `N` must be a positive integer literal that fits in 32 bits. `timeout "..."` sets the wall-clock timeout as an ISO 8601 duration string literal. Both clauses are independent and optional. When either is omitted, paxc falls back to PA's own defaults (60 iterations and `PT1H`, one hour). The two clauses must appear in the order `max` first, `timeout` second, when both are present.

paxr uses the user-set `max` (when present) to cap its local iteration; without one, it falls back to 60. When the cap is hit, paxr exits the loop cleanly and prints `<until "Until" hit iteration cap of N>` so you can tell a capped exit from a natural one. The `timeout` clause is ignored by paxr because the interpreter cannot simulate wall-clock time; the real flow in Power Automate still enforces it.

As with `foreach`, a `terminate` inside the body halts both the loop and the enclosing program, and a `let` declared inside the body is scoped to the body.

### scope

```
scope try_work {
  pa HTTP_Get_Data
}
```

`scope [<name>] { ... }` wraps a block of actions in a single Power Automate `Scope` action. A named scope compiles to action key `Scope_<name>`; an anonymous scope (no label) compiles to `Scope`, auto-suffixed if repeated.

A scope on its own is a no-op container, which matters for two reasons. First, the `runAfter` graph sees the scope as one unit: a statement following the scope chains back to the scope itself rather than to its internal actions, so the graph stays clean regardless of how many steps are inside. Second, scopes are the attachment point for error-path handlers described next.

Scope bodies follow the same nesting rules as `if` and `foreach` bodies: nested `var` is rejected, and `let` bindings are scoped to the body.

### on handlers

```
scope fetch_data {
  pa HTTP_Get_Data
}

on failed fetch_data {
  debug("the fetch failed")
}

on succeeded fetch_data {
  debug("fetched ok")
}
```

`on <status> [or <status>]* <target> { ... }` attaches a handler to a named scope or pa action. The handler runs when the target reports any of the listed statuses. Supported statuses are `succeeded`, `failed`, `skipped`, and `timedout`, which mirrors the set Power Automate's own `runAfter` accepts. Each handler compiles to a Power Automate `Scope` action whose `runAfter` points at the target with every listed status in the array.

pa actions are always valid handler targets without opt-in syntax -- their user-written name is also their PA action key. This is the most natural place to attach retry / recovery logic in the stub-and-fix workflow, because the action most likely to fail is usually a connector call.

```
pa HTTP_Get_Order

on failed or timedout HTTP_Get_Order {
  debug("recoverable call failure")
}
```

Multi-status form uses `or` to join statuses:

```
on failed or timedout fetch_data {
  debug("the fetch failed or timed out")
}
```

That compiles to a single `Scope` action with `runAfter: { "Scope_fetch_data": ["Failed", "TimedOut"] }`. One handler, one body, covers both cases without duplication. Listing the same status twice (`on failed or failed ...`) is a resolve error, since it is almost always a typo.

Handlers sit off the main sibling chain. A statement written after any handlers does not chain its `runAfter` through the handlers; it chains back to whatever real action came before them. In the example above, a statement after `on succeeded fetch_data` would chain to `Scope_fetch_data`, not to the handler. The "normal path" of the flow runs through the scope; handlers are side-attached safety nets.

Multiple handlers on the same scope are independent parallel actions in the emitted graph. Handler action names follow the pattern `On_<status>_<target>` for a single-status handler, or `On_<status1>_<status2>_..._<target>` for a multi-status handler (for example, `On_failed_fetch_data` or `On_failed_timedout_fetch_data`), auto-suffixed if two handlers would otherwise collide.

The target of an `on` handler must be a named scope or pa action declared somewhere earlier in the source. An unknown target raises a resolve error with a "not a named scope or pa action" diagnostic.

Handler targets share one namespace: named scopes and pa actions compete for the same names across the whole program. Declaring two blocks with the same name (two `scope work`, or a `scope foo` and a later `pa foo`, or two `pa HTTP_Call`) is a resolve error. The rule is strict: names are globally unique regardless of nesting, so two scopes with the same name in mutually-exclusive branches of an `if` / `else` / `switch` / `foreach` / `until` / `scope` are also rejected. The strictness keeps the mental model simple and prevents PA from compiling two identically-labeled actions. Anonymous `scope { }` blocks do not register a name, so you can have any number of them.

paxr walks the happy path, so any handler whose status list contains `succeeded` fires locally and its side effects appear in the end-of-run state dump. Handlers without `succeeded` in their list cannot be triggered from the interpreter, so paxr prints `<skipping on-<labels> handler "...">` and moves on without executing them. The compiled flow in Power Automate still dispatches them correctly at runtime.

### terminate

```
terminate succeeded
terminate failed
terminate failed "queue is empty"
terminate failed "validation failed at step " & step_name
terminate failed "No title" code "Invalid item"
terminate failed code "Invalid item"
terminate cancelled
```

`terminate` early-exits the flow. The syntax is `terminate <status> [<message>] [code <code-expr>]`. Status is one of `succeeded`, `failed`, or `cancelled`. The message and `code` clauses are only accepted after `failed`, because Power Automate's `runError` field is ignored on the other statuses. Both are expressions that evaluate to strings; `&` concat and variable references both work. The `code` clause is independent of the message — you can supply either, both, or neither.

`code` is a contextual keyword: it introduces the code clause only when followed by an expression. A bare trailing `code` (no expression after) parses as an identifier reference, so `terminate failed code` (with `code` a string variable) still works as the message-only form.

Compiles to a Power Automate `Terminate` action with `runStatus` set and, for the failed form, a `runError` block containing `code` and/or `message` fields when present. paxr halts execution on reaching a `terminate`: subsequent statements do not run, including those enclosed by `foreach` or `if` at higher scopes. The end-of-run state dump still prints and reflects what was set up to the point of termination.

## Function calls

```
let label_length = length("pending")
let summary = concat("Completed ", completed, " of ", total, " tasks")
let upper = toUpper(concat("hello ", "world"))
var items: array = []
if empty(items) {
  total = 0
}
```

Function calls pass through to Power Automate's expression language. paxc doesn't distinguish between "built-in" and "unknown" function names: `concat`, `length`, `toUpper`, `empty`, `substring`, `first`, `last`, and anything else Power Automate supports all work the same way. The call is emitted as-is inside the expression string.

This means the catalog of available functions is Power Automate's own expression function reference. Any function name valid there is valid in pax. Nested calls are fine. When a function call appears as an `if` condition, paxc can't prove its return type is boolean, so it auto-wraps with `equals(..., true)`.

### Functions paxr can evaluate locally

paxc emits every function call unchanged, but paxr implements a subset of Power Automate's expression functions so the interpreter can produce real values for local testing instead of returning null. A function not in the list below still compiles and runs correctly in Power Automate; paxr just returns null and prints a `<skipping unknown "name">` notice so you know the value didn't come from a real evaluation.

| Category | Functions |
|---|---|
| Arithmetic | `add`, `sub`, `mul`, `div`, `mod`, `min`, `max`, `range` |
| Comparison and logic | `equals`, `less`, `lessOrEquals`, `greater`, `greaterOrEquals`, `and`, `or`, `not`, `coalesce` |
| Text | `concat`, `toUpper`, `toLower`, `trim`, `substring`, `indexOf`, `lastIndexOf`, `startsWith`, `endsWith`, `replace`, `split`, `uriComponent`, `uriComponentToString` |
| Polymorphic | `length`, `empty`, `contains` |
| Array | `first`, `last`, `skip`, `take`, `join`, `createArray` |
| Conversion and utility | `string`, `int`, `bool`, `guid` |

A few semantics worth knowing:

- `length`, `empty`, and `contains` accept strings, arrays, and objects.
- Power Automate's `startsWith`, `endsWith`, `indexOf`, and `lastIndexOf` are case-insensitive; paxr follows the same convention.
- String `contains` is case-sensitive. Array `contains` is a membership test. Object `contains` checks for a key.
- `min` and `max` accept either variadic integer arguments or a single array.
- `coalesce` returns the first argument that is not `null`. An empty string or zero is not null and will be returned.
- `int("  42  ")` trims whitespace before parsing. Unparseable input returns `null` rather than erroring.
- `bool` accepts `"true"` / `"false"` (case-insensitive), `"1"` / `"0"`, and the integer `0` / `1`.
- `guid()` produces a fresh random UUID each call, so paxr runs that use it are not bit-for-bit reproducible. This matches Power Automate's runtime behavior.

Date and time functions (`utcNow`, `formatDateTime`, `addMinutes`, and the rest) are not yet implemented in paxr and currently render as null under the interpreter. They continue to work correctly in Power Automate.

## The pa primitive

```
pa Post_to_webhook
```

with `pa/Post_to_webhook.json` next to the source:

```json
{
  "type": "Http",
  "inputs": {
    "method": "POST",
    "uri": "https://example.com/hooks/daily",
    "headers": {
      "Content-Type": "application/json"
    },
    "body": {
      "summary": "@{variables('summary')}",
      "retries": "@{variables('retry_count')}"
    },
    "retryPolicy": {
      "type": "exponential",
      "count": 3,
      "interval": "PT10S"
    }
  }
}
```

When pax doesn't model an action natively (which is the case for every connector, including SharePoint, Outlook, Teams, and HTTP, and for ParseJson and other PA-designer-shaped primitives), `pa <Name>` references an opaque action whose body lives in `pa/<Name>.json` next to the source. paxc reads the file at compile time and drops it verbatim into the emitted flow's `definition.actions`. The name after `pa` becomes the action key, and it must be a valid pax identifier (matches `[A-Za-z_][A-Za-z0-9_]*`) so it doubles as a filesystem-safe filename.

Inside the JSON file you write Power Automate expression strings directly. Variables are referenced as `@{variables('name')}`, Compose outputs as `@{outputs('Compose_name')}`, and trigger data as `@{triggerBody()}`. This is the one place where you write PA expression syntax rather than pax syntax. The convention is for the file to match PA's "Code View" / "Peek code" output exactly, which makes round-trip from a real PA flow byte-identical.

pa actions participate in the runAfter chain like any other statement: they run after the preceding statement, and the next statement runs after them. The file's own `runAfter` (if it has one from PA's Peek code) is informational only -- paxc's structural sequence wins on emit. Paxr can't invoke real connectors, so when it encounters a `pa` action during interpretation it prints `<skipping pa action "Name">` and moves on.

## Connection references

PA flows that use connectors carry a top-level `connectionReferences` map in the flow envelope. paxc reads it from `pa/connectionReferences.json` if present:

```
pa/
├── Get_items.json
├── Send_email.json
└── connectionReferences.json
```

The JSON file's contents are dropped verbatim at the top level alongside `definition` in the emitter's output, and the packager places them in the legacy import package at the location PA expects. If no `pa/connectionReferences.json` is present, the field is simply omitted -- which is fine for connector-free flows.

## debug() and paxr

```
debug()
debug(total)
debug(total, completed)
debug(remaining, total - completed)
```

`debug()` is a paxr-only diagnostic. It accepts any number of expressions. Zero arguments is a breadcrumb, one argument is auto-labeled with its source slice, and multiple arguments are printed comma-separated on one line. Output looks like:

```
debug: total=10 at line 7
debug: total=10, completed=7 at line 10
debug: remaining=3, total - completed=3 at line 13
```

`debug()` can appear anywhere a statement is legal: at the top level, inside an `if` branch, or inside a `foreach` body.

paxc strips every debug call from the emitted flow and writes a note to stderr at the end of compilation:

```
note: dropped 4 debug() statement(s)
```

Because debug statements are stripped at compile time, they don't participate in the runAfter chain. The statement before a debug and the statement after it see each other as adjacent in the emitted graph.

paxr has four output modes. Default prints debug output plus an end-of-run state dump. `--verbose`/`-v` adds a trace event for every action the interpreter touches (`init`, `set`, `increment`, `compose`, `condition?`, `iter[N]`, and others). `--quiet`/`-q` suppresses all output, which is useful when a script only cares about the exit code. `--debug`/`-d` prints only debug lines. The four modes are mutually exclusive.

A verbose run looks like this:

```
$ paxr -v demo.pax
init counter = 0
increment counter = 1
increment counter = 2
debug: counter=2 at line 6
compose label = "done"

end state:
counter (var int) = 2
label (let) = "done"
```

The section after `end state:` is the end-of-run state dump. The markers next to each name show what kind of binding it is: `var int` for an int variable, `let` for a Compose binding.

## The runAfter rule

Power Automate's flow definition is a map of actions keyed by name, and each action declares a `runAfter` dict listing its predecessors. Writing this by hand means wiring every dependency edge manually and keeping the graph consistent as the flow changes. pax infers the graph from source order: each statement's `runAfter` is the name of the immediately preceding statement, and the first statement has an empty `runAfter` (meaning "run after the trigger fires").

This rule applies recursively inside control flow. Inside an `if` body, statements chain to each other starting fresh at the first branch statement. Inside a `foreach`, `switch` case, `until`, or `scope` body, same rule. `debug()` statements are stripped at compile time and don't participate at all. `on` handlers are the one intentional break from the source-order rule: their `runAfter` points at their target scope with the chosen status, and statements following a handler chain back to the last real action before any handlers, not to the handler itself.

The practical consequence: you write pax the way you'd write any imperative code, and the dependency graph comes out correct. You never touch `runAfter` directly. The runAfter inside a `pa/<Name>.json` body is informational only; paxc's structural sequence wins on emit, which preserves the property that pasting Peek-code output into the file Just Works.

## Running paxc and paxr

paxc has three modes. Without `--target`, it writes flow JSON to stdout for inspection:

```
$ paxc flow.pax > flow.json
```

With `--target pa-legacy`, it writes an importable package zip:

```
$ paxc --target pa-legacy --name myflow --out myflow.zip flow.pax
```

The package is a legacy-format Power Automate zip. Import it through the Power Automate portal at **My flows > Import > Import Package (Legacy)**. If `--name` is omitted, paxc looks for a `displayName` field in `pa/flow.json` and uses it; if that's also missing, the package name falls back to the source filename. This means a decoded flow re-encodes back to its original Power Automate display name without any flag plumbing.

With `--decode`, paxc runs in reverse: it reads an exported PA flow JSON and produces a `.pax` source file plus a `pa/` folder of opaque action bodies (see the next section).

```
$ paxc --decode flow.json --out-dir flow_src/
```

paxr takes a pax source file and interprets it locally:

```
$ paxr flow.pax            # default: debug output plus end-of-run state dump
$ paxr -v flow.pax         # verbose: adds a trace event for every action
$ paxr -q flow.pax         # quiet: suppresses all output (exit code only)
$ paxr -d flow.pax         # debug: prints only debug() output
```

Both binaries also accept `--version` (or `-V`) to print the installed version.

Running a flow through paxr first is a fast sanity check before making the round-trip through Power Automate.

## Round-tripping from PA exports

Existing Power Automate flows live as JSON inside legacy-format export packages (the same shape paxc produces with `--target pa-legacy`). paxc can decode the inner `definition.json` back into pax source so you can refactor, version-control, and recompile a flow without rebuilding it in the designer:

```
$ paxc --decode MyFlow_2026.zip                # zip input — output dir defaults to MyFlow_2026/
$ paxc --decode definition.json --out-dir my_flow/   # raw inner-definition input
```

`--decode` accepts either a PA legacy import package `.zip` (the file you get from PA's "Export → Package (Legacy)") or the inner `Microsoft.Flow/flows/<guid>/definition.json` directly. For zip input, the inner definition is extracted in-memory; if the zip contains zero or more than one flow folder, paxc errors with a clear message rather than guessing. Default `--out-dir` is the input's parent directory for `.json`, or a sister directory named after the zip's stem for `.zip`.

This writes:

- `my_flow/definition.pax` — the decoded source (named after the input file's stem).
- `my_flow/pa/<TriggerName>.trigger.json` — the trigger body, byte-for-byte from the export.
- `my_flow/pa/<ActionName>.json` — one file per action that didn't lower to a pax-native statement (connectors, ParseJson, anything whose required structural pieces reference PA accessors paxc doesn't yet model, etc.).
- `my_flow/pa/connectionReferences.json` — when present in the export, the top-level connection references dict.
- `my_flow/pa/flow.json` — small metadata file with the flow's `displayName`, `contentVersion`, and (when applicable) an `actionNameMap` recording PA action keys that had to be normalized to valid pax identifiers (anything containing `(`, `)`, `!`, spaces, etc.).

What lowers natively today: the variable lifecycle actions (`InitializeVariable`, `SetVariable`, `IncrementVariable`, `DecrementVariable`, `AppendToStringVariable`, `AppendToArrayVariable`), `Compose`, the container actions (`If`, `Foreach`, `Until`, `Switch`, `Scope`), `Terminate`, and on-handlers (a `Scope` whose `runAfter` points at a single addressable target with non-default statuses). Values can be JSON literals or PA expression strings (`@variables('x')`, `@add(x, 1)`, `@triggerBody()?['body/email']`, interpolated templates like `"hello @{variables('name')}!"`); the decoder translates expressions to pax expression source whenever every node has a pax-renderable form. `If` conditions accept both the `@`-string form and PA designer's structured-object form (`{"and": [{"equals": [...]}]}`). Inside a foreach body, `items('<For_each_action_key>')` lowers to the pax iterator name (the action key, normalized). PA accessors (`triggerBody()`, `triggerOutputs()`, `trigger()`, `parameters(...)`, `body(...)`, `actions(...)`, `outputs(<non-Compose>)`, `iterationIndexes(...)`, `item()`) lower as ordinary pax call expressions. Path segments whose key isn't a valid pax identifier (SharePoint/Forms-style `body/raf2bb...` paths, numeric indexes, names with spaces) lower via the subscript form `?["<key>"]`; identifier keys still get the ergonomic `.field` form. A container whose required structural piece (condition, collection, switch subject, case literal) doesn't render falls back as one opaque action; its children stay nested in the same opaque body. For `Compose`, the action key must also have shape `Compose_<identifier>` for paxc to recover a `let <identifier>` binding name; a bare `Compose` (Power Automate's default for the first unrenamed Compose) falls back. An `on` handler natively lowers only when its target is addressable as a pax identifier (a named scope or `pa <Name>` block); handlers attached to natively-lowered variables, lets, or anonymous scopes fall back. Anything else falls back to `pa <Name>` with the action body in `pa/<Name>.json`. This is intentional: the round-trip is lossless even when the native-decode coverage is partial, because falls-back are emitted byte-for-byte from the file. paxc prints a stderr warning per fallback so you can see exactly what didn't decode natively.

PA action keys with characters outside pax's identifier rules (parentheses, spaces, etc.) are normalized on decode (`Send_an_email_(V2)` → `Send_an_email_V2`) and the original→normalized mapping is recorded in `pa/flow.json.actionNameMap`. The encode side reads that file and overrides the emit-side action key, so `pa Send_an_email_V2` re-encodes to `Send_an_email_(V2)` byte-for-byte. The override is best-effort: missing or malformed `pa/flow.json` falls back to using the user-written name verbatim.

Re-encoding the decoded source with `paxc --target pa-legacy` reproduces the original flow's structure. The corpus harness in `tests/decoder.rs` round-trips a set of real PA exports as part of `cargo test`; drop a `definition.json` into `tests/corpus/<name>/input.json` to add coverage for new patterns.

## More examples

The `examples/slice*.pax` files each focus on a single feature and are useful when you want a minimal example of one thing. `examples/tour.pax` is a broader walkthrough including variables, foreach, if/else, function calls, member access, string concat, and the `pa <Name>` opaque-action primitive. Features added since the original tour (the `debug()` statement, `terminate`, the expanded paxr function library, the control-flow sweep additions of `switch`, `scope`, `until`, and `on` handlers, and the file-based trigger convention introduced in 3.0.0) appear in their dedicated slice examples rather than in the tour. Opaque action bodies live in `examples/pa/`; the file-based trigger demo lives at `examples/pa/Recurrence.trigger.json`.
