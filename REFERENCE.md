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

Every pax program starts with a trigger declaration. In v1, the only trigger is `manual`:

```
trigger manual
```

This compiles to Power Automate's "manually trigger a flow" button. The trigger takes no arguments in its pax form.

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

This rule applies recursively inside control flow. Inside an `if` body, statements chain to each other starting fresh at the first branch statement. Inside a `foreach` body, same rule. `debug()` statements are stripped at compile time and don't participate at all.

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

Every construct above appears in `examples/tour.pax`. The `examples/slice*.pax` files each focus on a single feature, which is useful when you want a minimal example of one thing.
