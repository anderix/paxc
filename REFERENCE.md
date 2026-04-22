# pax Reference

pax is a small domain-specific language for Power Automate cloud flows. The `paxc` compiler emits the JSON that Power Automate expects, and the `paxr` interpreter runs the same source locally so you can see what a flow does before deploying it.

A pax source file declares a trigger, then lists actions in the order they should execute. The compiler infers the `runAfter` dependency graph from source order, which means the source you write looks like imperative code even though what gets emitted is a graph of named actions.

## A minimal example

Save this as `hello.pax`:

```
trigger manual

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

Every pax program starts with a trigger declaration. Two trigger forms are currently supported.

### Manual

```
trigger manual
```

Compiles to Power Automate's "manually trigger a flow" button. The trigger takes no arguments in its pax form.

### Schedule

```
trigger schedule every 5 minutes
trigger schedule every hour
trigger schedule every 2 days
```

Compiles to Power Automate's Recurrence trigger. The syntax is `trigger schedule every [N] <unit>`. The integer defaults to 1 when omitted. Units are `second`, `minute`, `hour`, `day`, `week`, `month`, with singular and plural forms both accepted. The emitted trigger is a `Recurrence` object with `frequency` (the capitalized unit name) and `interval` (the integer).

Advanced recurrence features such as `startTime`, `timeZone`, and the nested `schedule` object (for patterns like "every Monday at 9am") are not yet modeled in pax syntax. paxr does not execute triggers, so a scheduled flow runs through the interpreter exactly like a manual one.

## Variables and types

```
var counter: int = 0
var greeting: string = "hello"
var active: bool = true
var tags: array = ["urgent", "review"]
var config: object = {
  "region": "us-east",
  "retries": 3,
}
```

A `var` declaration compiles to a Power Automate `InitializeVariable` action named `Initialize_<name>`. The five v1 types are `int`, `string`, `bool`, `array`, and `object`. Array and object literals use JSON-like syntax, and trailing commas are permitted.

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

Variables declared with `var` support four assignment forms. Plain `=` compiles to `SetVariable`. The compound forms each map to a dedicated Power Automate action: `+=` on an int becomes `IncrementVariable`, `-=` becomes `DecrementVariable`, `&=` on a string becomes `AppendToStringVariable`, and `+=` on an array becomes `AppendToArrayVariable`. Each assignment is a separate action in the emitted flow. Compose bindings (`let`) cannot be reassigned.

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

## Member access

```
let region = config.region
let primary = config.endpoints.primary
var announcement: string = config.endpoints.fallback
```

Dot notation reads fields from object variables, object literals, and the iterator variable of a `foreach`. Chains are allowed. The emitted expression uses Power Automate's safe-navigation bracket form: `variables('config')?['endpoints']?['primary']`.

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

paxc emits PA's default iteration limit (60) and timeout (`PT1H`, one hour) with every until. User-tunable limits are not yet exposed in pax syntax. paxr caps its own local iteration at 60 too so runaway conditions do not hang the interpreter; when the cap is hit, paxr exits the loop cleanly and prints `<until "Until" hit iteration cap of 60>` so you can tell a capped exit from a normal one.

As with `foreach`, a `terminate` inside the body halts both the loop and the enclosing program, and a `let` declared inside the body is scoped to the body.

### scope

```
scope try_work {
  raw HTTP_Get_Data {
    "type": "Http",
    "inputs": { "method": "GET", "uri": "https://api.example.com/data" }
  }
}
```

`scope [<name>] { ... }` wraps a block of actions in a single Power Automate `Scope` action. A named scope compiles to action key `Scope_<name>`; an anonymous scope (no label) compiles to `Scope`, auto-suffixed if repeated.

A scope on its own is a no-op container, which matters for two reasons. First, the `runAfter` graph sees the scope as one unit: a statement following the scope chains back to the scope itself rather than to its internal actions, so the graph stays clean regardless of how many steps are inside. Second, scopes are the attachment point for error-path handlers described next.

Scope bodies follow the same nesting rules as `if` and `foreach` bodies: nested `var` is rejected, and `let` bindings are scoped to the body.

### on handlers

```
scope fetch_data {
  raw HTTP_Get_Data { ... }
}

on failed fetch_data {
  debug("the fetch failed")
}

on succeeded fetch_data {
  debug("fetched ok")
}
```

`on <status> <target> { ... }` attaches a handler to a named scope. The handler runs when the target scope reports the matching status. Supported statuses are `succeeded`, `failed`, `skipped`, and `timedout`, which mirrors the set Power Automate's own `runAfter` accepts. Each handler compiles to a Power Automate `Scope` action whose `runAfter` points at the target with the single chosen status.

Handlers sit off the main sibling chain. A statement written after any handlers does not chain its `runAfter` through the handlers; it chains back to whatever real action came before them. In the example above, a statement after `on succeeded fetch_data` would chain to `Scope_fetch_data`, not to the handler. The "normal path" of the flow runs through the scope; handlers are side-attached safety nets.

Multiple handlers on the same scope are independent parallel actions in the emitted graph. Handler action names follow the pattern `On_<status>_<target>` (for example, `On_failed_fetch_data`), auto-suffixed if a second handler with the same status and target is declared.

The target of an `on` handler must be a named scope declared somewhere earlier in the source. An unknown target raises a resolve error with a "not a named scope" diagnostic.

paxr walks the happy path, so `on succeeded` handlers fire locally and their side effects appear in the end-of-run state dump. The other statuses (`failed`, `skipped`, `timedout`) cannot be triggered from the interpreter, so paxr prints `<skipping on-<status> handler "...">` and moves on without executing them. The compiled flow in Power Automate still dispatches them correctly at runtime.

### terminate

```
terminate succeeded
terminate failed
terminate failed "queue is empty"
terminate failed "validation failed at step " & step_name
terminate cancelled
```

`terminate` early-exits the flow. The syntax is `terminate <status> [message]`. Status is one of `succeeded`, `failed`, or `cancelled`. A message is only accepted after `failed`, because Power Automate's `runError` field is ignored on the other statuses. The message is any expression that evaluates to a string, so `&` concat and variable references both work.

Compiles to a Power Automate `Terminate` action with `runStatus` set and, for the failed form, a `runError.message` field. paxr halts execution on reaching a `terminate`: subsequent statements do not run, including those enclosed by `foreach` or `if` at higher scopes. The end-of-run state dump still prints and reflects what was set up to the point of termination.

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

## The raw escape hatch

```
raw Post_to_webhook {
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

When pax doesn't model an action natively (which is the case for every connector, including SharePoint, Outlook, Teams, and HTTP, and for some less common primitive operations), `raw` lets you drop a verbatim Power Automate action definition into the flow. The name after `raw` (`Post_to_webhook` above) becomes the action key in the emitted JSON. The block inside `{ ... }` is JSON object syntax and is emitted into the flow definition with minimal transformation.

Inside a `raw` body you write Power Automate expression strings directly. Variables are referenced as `@{variables('name')}`, Compose outputs as `@{outputs('Compose_name')}`, and trigger data as `@{triggerBody()}`. This is the one place in a pax source file where you need to know PA expression syntax rather than pax syntax.

Raw blocks participate in the runAfter chain like any other statement: they run after the preceding statement, and the next statement runs after them. Paxr can't invoke real connectors, so when it encounters a `raw` block during interpretation it prints `<skipping raw "Name">` and moves on.

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

The practical consequence: you write pax the way you'd write any imperative code, and the dependency graph comes out correct. You never touch `runAfter` directly unless you're using a `raw` block with unusual dependency needs.

## Running paxc and paxr

paxc has two modes. Without `--target`, it writes flow JSON to stdout for inspection:

```
$ paxc flow.pax > flow.json
```

With `--target pa-legacy`, it writes an importable package zip:

```
$ paxc --target pa-legacy --name myflow --out myflow.zip flow.pax
```

The package is a legacy-format Power Automate zip. Import it through the Power Automate portal at **My flows > Import > Import Package (Legacy)**. If `--name` is omitted, the package name is derived from the source filename.

paxr takes a pax source file and interprets it locally:

```
$ paxr flow.pax            # default: debug output plus end-of-run state dump
$ paxr -v flow.pax         # verbose: adds a trace event for every action
$ paxr -q flow.pax         # quiet: suppresses all output (exit code only)
$ paxr -d flow.pax         # debug: prints only debug() output
```

Running a flow through paxr first is a fast sanity check before making the round-trip through Power Automate.

## More examples

The `examples/slice*.pax` files each focus on a single feature and are useful when you want a minimal example of one thing. `examples/tour.pax` is a broader walkthrough of the original v1.1 surface, including variables, foreach, if/else, function calls, member access, string concat, and the raw escape hatch. Features added since v1.1 (the `debug()` statement, the Schedule trigger, `terminate`, the expanded paxr function library, and the control-flow sweep additions of `switch`, `scope`, `until`, and `on` handlers) appear in their dedicated slice examples rather than in the tour.
