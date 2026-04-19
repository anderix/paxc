# paxc — the pax compiler

`paxc` compiles the **pax** DSL into [Power Automate](https://powerautomate.microsoft.com/) cloud flow definitions. Write terse, readable code; get the verbose `definition.json` that Power Automate expects.

## Status

Pre-alpha. The language is designed, the compiler is not yet implemented. See below for what v1 will support.

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

## v1 scope

Built-in constructs that compile natively:

- Variable declaration and mutation (`var`, `let`, `=`, `+=`, etc.)
- `Compose` via `let`
- `if` / `else if` / `else`
- `foreach item in collection { ... }`
- Manual button trigger
- Raw-JSON escape hatch for any action pax doesn't support natively (including all connector actions like SharePoint, Outlook, Teams, etc.)

## Building

Requires Rust (edition 2024, toolchain 1.85+).

```sh
cargo build --release
```

The binary will be at `target/release/paxc`.

## License

MIT. See [LICENSE](LICENSE).
