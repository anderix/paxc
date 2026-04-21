# paxc — the pax compiler

`paxc` compiles the **pax** DSL into [Power Automate](https://powerautomate.microsoft.com/) cloud flow definitions. Write terse, readable code; get the verbose `definition.json` that Power Automate expects. Companion interpreter `paxr` runs the same source locally for fast iteration.

For the full language reference, see [REFERENCE.md](REFERENCE.md).

## Status

v1.2 shipped. Every construct listed in REFERENCE.md is implemented and tested, end-to-end deployment to Power Automate has been validated, and a legacy-format package target lets you import compiled flows directly through the Power Automate portal.

## Why

The Power Automate browser designer is slow and click-heavy. The underlying flow definition is JSON that's technically hand-editable but structured in ways that fight you: actions are a map keyed by name, dependencies are encoded as a `runAfter` graph, and expressions live inside escaped strings. pax is a small DSL that turns all of that into source code you can actually read and maintain, and `paxc` is the compiler that emits the JSON.

Equivalent pax and JSON for initializing a counter:

```
var counter: int = 1
```

```json
{
  "Initialize_counter": {
    "type": "InitializeVariable",
    "inputs": {
      "variables": [
        { "name": "counter", "type": "Integer", "value": 1 }
      ]
    },
    "runAfter": {}
  }
}
```

The source is shorter, and more importantly, the `runAfter` dependency graph is inferred from source order so you never hand-wire it.

## What pax covers

The language supports manual triggers, typed variables and Compose bindings, assignment and compound assignment, arithmetic and boolean expressions, string concatenation, member access, `if`/`else if`/`else` and `foreach` control flow, function calls that pass through to Power Automate's expression language, a `raw` escape hatch for anything pax doesn't model natively (including all connector actions), and a `debug()` statement that paxr prints at runtime and paxc strips at compile time.

## Building

Requires Rust (edition 2024, toolchain 1.85+).

```sh
cargo build --release
```

The binaries will be at `target/release/paxc` and `target/release/paxr`.

## License

MIT. See [LICENSE](LICENSE).
