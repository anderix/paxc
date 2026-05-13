# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub Security Advisories at https://github.com/excelano/paxc/security/advisories/new. If you would rather not use GitHub, email david.anderson@excelano.com instead. I aim to respond within seven days.

Please do not open public issues for security problems.

## Supported versions

The latest 3.x release receives security fixes. Earlier major versions are not supported. paxc is distributed as source via `cargo install`; pull and rebuild to apply fixes.

## What paxc can access

paxc is a local compiler that runs entirely on your machine. It reads pax source files and optional companion `pa/` JSON files from a project directory, and writes a Power Automate `definition.json` (and optionally a `.zip` legacy package) to a path you specify. It makes no network connections, has no authentication surface, and does not communicate with Microsoft 365, the Power Automate service, or any other external system. There is no telemetry and no analytics.

The companion `paxr` interpreter behaves identically with respect to system access: it reads the same input files and prints execution traces to stdout. It does not perform real Power Automate calls.

## What paxc stores

paxc writes only the output files you ask it to write. It does not maintain any persistent state, cache, or credential store on disk.

## Working with real Power Automate exports

When you run `paxc --decode <flow.json>` against a real Power Automate export, the input file contains tenant data: tenant IDs, user object IDs, SharePoint site URLs, connection references, and similar identifiers. paxc preserves these byte-for-byte during round-trip so that re-encoded flows deploy identically to where they came from.

Treat both the original export and the `pa/` companion files paxc emits with the same care you treat any other tenant artifact. In particular:

- Do not commit real PA exports or generated `pa/` folders to public repositories.
- Use the redacted fixtures in `examples/` as test material for sharing or demonstrating pax.
- Strip tenant-specific identifiers from any export you intend to publish.

This guidance is about handling your tenant's data, not about a vulnerability in paxc itself; paxc never transmits the contents of these files anywhere.
