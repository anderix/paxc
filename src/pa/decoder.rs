//! Decoder: turns an exported Power Automate flow JSON into pax source plus
//! a `pa/` folder of opaque action bodies. Inverse of `pa::emitter`.
//!
//! Slice 44a covered the skeleton plus the variable lifecycle action types
//! (InitializeVariable / SetVariable / IncrementVariable / DecrementVariable
//! / AppendToStringVariable / AppendToArrayVariable). Slice 44b added Compose
//! → `let` lowering when the action key has shape `Compose_<identifier>`.
//! Slice 44c lifts the literal-only restriction on values: PA expressions
//! (`@variables('x')`, `@add(x, 1)`, `@{outputs('Compose_y')?['field']}`,
//! interpolated templates, etc.) are now translated to pax expression source
//! when every node has a pax-renderable form. The translator lives in
//! `src/pa/paexpr.rs`. Every other action type still falls back to
//! `pa <Name>` plus a JSON body file, with a stderr-bound warning so the
//! user knows what didn't decode natively. Future sub-slices (44d–44f)
//! extend coverage to container actions, on-handlers, and terminate.
//!
//! The decoder is intentionally lossless-leaning: anything we can't faithfully
//! represent in pax stays as a `pa <Name>` block whose body is the original
//! action JSON byte-for-byte. Re-encoding through `paxc --target pa-legacy`
//! reproduces the action verbatim.

use crate::pa::paexpr;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Two-space indent, applied per nesting level when rendering pax source for
/// a container body. Keeping it as a constant rather than inlining lets
/// future-us swap to tabs or four-space without spelunking through formatters.
const PAX_INDENT_UNIT: &str = "  ";

/// Files written and warnings collected during a successful decode. The
/// caller (typically the paxc binary) prints `warnings` to stderr.
#[derive(Debug, Clone)]
pub struct DecodeReport {
    pub pax_path: PathBuf,
    pub pa_files_written: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum DecodeError {
    /// IO error reading or writing a specific path.
    Io { path: PathBuf, source: io::Error },
    /// Input file is not valid JSON.
    JsonParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// Input JSON is well-formed but lacks the structure paxc expects
    /// (no `properties.definition`, no triggers, etc.).
    BadShape(String),
    /// Slice 44a only handles single-trigger flows. Multi-trigger flows
    /// surface this error so the user knows why decoding stopped.
    MultipleTriggers(Vec<String>),
    /// `runAfter` graph contains a cycle — defensive; PA shouldn't allow it.
    Cycle { actions_remaining: Vec<String> },
    /// Input was a zip but contained no `Microsoft.Flow/flows/<GUID>/definition.json`
    /// entry — not a recognizable PA legacy package.
    NoFlowInPackage { path: PathBuf },
    /// Input zip contains more than one flow folder. Pax decodes a single
    /// flow; users must extract the desired `definition.json` and point
    /// `--decode` at that file directly.
    MultipleFlowsInPackage { path: PathBuf, flows: Vec<String> },
    /// Underlying zip read error (corrupt archive, etc.).
    Zip {
        path: PathBuf,
        source: zip::result::ZipError,
    },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            DecodeError::JsonParse { path, source } => {
                write!(f, "invalid JSON in {}: {source}", path.display())
            }
            DecodeError::BadShape(msg) => write!(f, "unexpected flow shape: {msg}"),
            DecodeError::MultipleTriggers(keys) => write!(
                f,
                "flow has multiple triggers ({}); slice 44a supports single-trigger flows only",
                keys.join(", ")
            ),
            DecodeError::Cycle { actions_remaining } => write!(
                f,
                "runAfter graph contains a cycle; unresolvable: {}",
                actions_remaining.join(", ")
            ),
            DecodeError::NoFlowInPackage { path } => write!(
                f,
                "no Microsoft.Flow/flows/<GUID>/definition.json entry in {}",
                path.display()
            ),
            DecodeError::MultipleFlowsInPackage { path, flows } => write!(
                f,
                "package {} contains {} flows ({}); decode targets a single flow — extract the desired definition.json and point --decode at that file directly",
                path.display(),
                flows.len(),
                flows.join(", ")
            ),
            DecodeError::Zip { path, source } => {
                write!(f, "zip read error at {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Top-level entry: read a PA flow JSON from `input_path` and write the
/// decoded pax source + `pa/` folder under `out_dir`. Output basename is
/// derived from the input file stem.
///
/// `input_path` may be either a raw `definition.json` or a PA legacy import
/// package `.zip`. Detection is by file extension (case-insensitive); when
/// it's a zip, the inner `Microsoft.Flow/flows/<GUID>/definition.json` is
/// extracted in-memory and decoded as if it had been passed directly.
pub fn decode_file(input_path: &Path, out_dir: &Path) -> Result<DecodeReport, DecodeError> {
    let is_zip = input_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
    let bytes = if is_zip {
        read_definition_from_zip(input_path)?
    } else {
        fs::read(input_path).map_err(|e| DecodeError::Io {
            path: input_path.to_path_buf(),
            source: e,
        })?
    };
    let input: Value = serde_json::from_slice(&bytes).map_err(|e| DecodeError::JsonParse {
        path: input_path.to_path_buf(),
        source: e,
    })?;
    let basename = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("flow")
        .to_string();
    decode(&input, &basename, out_dir)
}

/// Locate and extract the inner `definition.json` from a PA legacy package.
/// Returns the file's bytes verbatim, ready to feed into the JSON parser.
/// Errors if the zip is unreadable, contains zero flows, or contains more
/// than one flow folder under `Microsoft.Flow/flows/`.
fn read_definition_from_zip(input_path: &Path) -> Result<Vec<u8>, DecodeError> {
    use std::io::Read;
    let file = fs::File::open(input_path).map_err(|e| DecodeError::Io {
        path: input_path.to_path_buf(),
        source: e,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| DecodeError::Zip {
        path: input_path.to_path_buf(),
        source: e,
    })?;
    // Collect inner definition.json paths grouped by their parent flow folder.
    // Legacy package shape: `Microsoft.Flow/flows/<GUID>/definition.json`. We
    // accept either forward or back slashes (some zip producers vary).
    let mut definitions: Vec<(String, String)> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| DecodeError::Zip {
            path: input_path.to_path_buf(),
            source: e,
        })?;
        let name = entry.name().replace('\\', "/");
        if let Some(flow_id) = match_flow_definition_path(&name) {
            definitions.push((flow_id, name));
        }
    }
    match definitions.len() {
        0 => Err(DecodeError::NoFlowInPackage {
            path: input_path.to_path_buf(),
        }),
        1 => {
            let (_, name) = definitions.into_iter().next().unwrap();
            let mut entry = archive.by_name(&name).map_err(|e| DecodeError::Zip {
                path: input_path.to_path_buf(),
                source: e,
            })?;
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes).map_err(|e| DecodeError::Io {
                path: input_path.to_path_buf(),
                source: e,
            })?;
            Ok(bytes)
        }
        _ => Err(DecodeError::MultipleFlowsInPackage {
            path: input_path.to_path_buf(),
            flows: definitions.into_iter().map(|(id, _)| id).collect(),
        }),
    }
}

/// Recognize `Microsoft.Flow/flows/<flow_id>/definition.json` and return
/// the flow id segment. Returns None for any other entry. The flow id is
/// usually a GUID but the matcher doesn't enforce that — anything that
/// isn't empty and doesn't contain a slash is accepted.
fn match_flow_definition_path(name: &str) -> Option<String> {
    let suffix = name.strip_prefix("Microsoft.Flow/flows/")?;
    let (flow_id, tail) = suffix.split_once('/')?;
    if tail != "definition.json" || flow_id.is_empty() {
        return None;
    }
    Some(flow_id.to_string())
}

/// Decode an in-memory PA flow JSON to disk. `basename` (without extension)
/// becomes the `.pax` filename. Useful for tests that build the input
/// programmatically.
pub fn decode(input: &Value, basename: &str, out_dir: &Path) -> Result<DecodeReport, DecodeError> {
    let envelope = unwrap_envelope(input)?;
    let definition = envelope
        .get("definition")
        .and_then(Value::as_object)
        .ok_or_else(|| DecodeError::BadShape("missing properties.definition".to_string()))?;

    let display_name = envelope
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content_version = definition
        .get("contentVersion")
        .and_then(Value::as_str)
        .unwrap_or("1.0.0.0")
        .to_string();

    let pa_dir = out_dir.join("pa");
    ensure_dir(&pa_dir)?;

    let mut report = DecodeReport {
        pax_path: out_dir.join(format!("{basename}.pax")),
        pa_files_written: Vec::new(),
        warnings: Vec::new(),
    };

    // 1. Trigger -> pa/<key>.trigger.json
    let trigger_key = decode_trigger(definition, &pa_dir, &mut report)?;

    // 2. connectionReferences -> pa/connectionReferences.json
    if let Some(refs) = envelope.get("connectionReferences")
        && !refs.is_null()
        && !is_empty_obj(refs)
    {
        let path = pa_dir.join("connectionReferences.json");
        write_json(&path, refs)?;
        report.pa_files_written.push(path);
    }

    // 3. Actions: topo-sort, then native-decode-or-fallback per action.
    // Containers (If/Foreach/Until/Switch/Scope) recurse into this same
    // helper for their bodies. The helper writes any fallback pa/ files
    // eagerly via the shared `DecodeCtx` and returns a vector of NativeStmts
    // we then format into pax source with proper indentation.
    let empty_actions = Map::new();
    let actions = definition
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty_actions);

    let mut name_map: HashMap<String, String> = HashMap::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut pa_files_written: Vec<PathBuf> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut target_names: HashMap<String, String> = HashMap::new();

    let stmts = {
        let mut ctx = DecodeCtx {
            pa_dir: &pa_dir,
            used_names: &mut used_names,
            name_map: &mut name_map,
            pa_files_written: &mut pa_files_written,
            warnings: &mut warnings,
            target_names: &mut target_names,
        };
        decode_actions_block(actions, &HashSet::new(), &HashMap::new(), &mut ctx)?
    };

    report.pa_files_written.extend(pa_files_written);
    report.warnings.extend(warnings);

    let mut pax_lines: Vec<String> = Vec::new();
    for stmt in &stmts {
        pax_lines.push(format_native_stmt(stmt, 0));
    }

    // 4. flow.json with envelope metadata + name map.
    let mut flow_meta = Map::new();
    if !display_name.is_empty() {
        flow_meta.insert("displayName".to_string(), Value::String(display_name));
    }
    if !content_version.is_empty() {
        flow_meta.insert("contentVersion".to_string(), Value::String(content_version));
    }
    if !name_map.is_empty() {
        let mut entries: Vec<(String, String)> = name_map.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut nm = Map::new();
        for (k, v) in entries {
            nm.insert(k, Value::String(v));
        }
        flow_meta.insert("actionNameMap".to_string(), Value::Object(nm));
    }
    let flow_json_path = pa_dir.join("flow.json");
    write_json(&flow_json_path, &Value::Object(flow_meta))?;
    report.pa_files_written.push(flow_json_path);

    // 5. Pax source. Trailing newline so editors are happy.
    let trigger_comment =
        format!("// Decoded from PA flow. Trigger: pa/{trigger_key}.trigger.json\n");
    let body = pax_lines.join("\n");
    let pax_source = format!("{trigger_comment}{body}\n");
    fs::write(&report.pax_path, pax_source).map_err(|e| DecodeError::Io {
        path: report.pax_path.clone(),
        source: e,
    })?;

    Ok(report)
}

/// Accept either the full PA legacy export envelope (`{name, properties:
/// {displayName, definition, ...}}`) or just the inner `properties` map.
/// Returns the `properties` object.
fn unwrap_envelope(input: &Value) -> Result<&Map<String, Value>, DecodeError> {
    let obj = input
        .as_object()
        .ok_or_else(|| DecodeError::BadShape("input JSON is not an object".to_string()))?;
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        // Full envelope shape: {name, id, type, properties: {...}}.
        return Ok(props);
    }
    if obj.contains_key("definition") {
        // Already the inner properties map.
        return Ok(obj);
    }
    Err(DecodeError::BadShape(
        "input lacks `properties` envelope and `definition` key".to_string(),
    ))
}

fn decode_trigger(
    definition: &Map<String, Value>,
    pa_dir: &Path,
    report: &mut DecodeReport,
) -> Result<String, DecodeError> {
    let triggers = definition
        .get("triggers")
        .and_then(Value::as_object)
        .ok_or_else(|| DecodeError::BadShape("missing definition.triggers".to_string()))?;
    let keys: Vec<String> = triggers.keys().cloned().collect();
    if keys.len() != 1 {
        return Err(DecodeError::MultipleTriggers(keys));
    }
    let trigger_key = keys.into_iter().next().unwrap();
    let trigger_body = &triggers[&trigger_key];
    let path = pa_dir.join(format!("{trigger_key}.trigger.json"));
    write_json(&path, trigger_body)?;
    report.pa_files_written.push(path);
    Ok(trigger_key)
}

/// Kahn's algorithm with alpha-sorted tiebreak so the order is deterministic
/// across runs (HashMap iteration order is not). Returns action keys in an
/// order such that every action's runAfter predecessors come earlier in the
/// list. Cycles surface as `DecodeError::Cycle`.
fn topo_sort_actions(actions: &Map<String, Value>) -> Result<Vec<String>, DecodeError> {
    let names: Vec<String> = actions.keys().cloned().collect();
    let mut in_count: HashMap<String, usize> = names.iter().map(|n| (n.clone(), 0)).collect();
    // dependents[name] = list of actions that have name in their runAfter.
    let mut dependents: HashMap<String, Vec<String>> =
        names.iter().map(|n| (n.clone(), Vec::new())).collect();

    for name in &names {
        if let Some(run_after) = actions[name].get("runAfter").and_then(Value::as_object) {
            for predecessor in run_after.keys() {
                // PA sometimes references actions that aren't siblings (e.g. a
                // sibling-chain link to a parent's chain). Only count edges
                // where the predecessor is a peer in this same actions map.
                if dependents.contains_key(predecessor) {
                    dependents.get_mut(predecessor).unwrap().push(name.clone());
                    *in_count.get_mut(name).unwrap() += 1;
                }
            }
        }
    }

    // Initial queue: all zero-in-degree nodes, alpha-sorted.
    let mut zero: Vec<String> = in_count
        .iter()
        .filter_map(|(n, &c)| if c == 0 { Some(n.clone()) } else { None })
        .collect();
    zero.sort();
    let mut queue: VecDeque<String> = zero.into();
    let mut result: Vec<String> = Vec::with_capacity(names.len());

    while let Some(node) = queue.pop_front() {
        result.push(node.clone());
        // Re-sort newly-unblocked dependents into the queue alphabetically
        // so output order doesn't depend on iteration order of the map.
        let mut newly_zero: Vec<String> = Vec::new();
        for dep in &dependents[&node] {
            let c = in_count.get_mut(dep).unwrap();
            *c -= 1;
            if *c == 0 {
                newly_zero.push(dep.clone());
            }
        }
        newly_zero.sort();
        for d in newly_zero {
            queue.push_back(d);
        }
    }

    if result.len() != names.len() {
        let remaining: Vec<String> = names.into_iter().filter(|n| !result.contains(n)).collect();
        return Err(DecodeError::Cycle {
            actions_remaining: remaining,
        });
    }
    Ok(result)
}

fn is_handler_runafter(run_after: Option<&Value>) -> bool {
    let Some(obj) = run_after.and_then(Value::as_object) else {
        return false;
    };
    for arr in obj.values() {
        let Some(arr) = arr.as_array() else { continue };
        for status in arr {
            let s = status.as_str().unwrap_or("");
            if s != "Succeeded" && !s.is_empty() {
                return true;
            }
        }
    }
    false
}

/// One pax statement we know how to emit natively. The `pa <Name>` fallback
/// is encoded as `NativeStmt::Pa` so containers can mix natively-decoded
/// children with opaque-fallback children inside the same body.
///
/// The `value` / `condition` / `collection` fields hold already-rendered pax
/// source (a string), produced by the `paexpr` translator. Storing the
/// rendered form lets the decoder stay agnostic about whether the source
/// value was a literal, a single PA accessor, a synthesized binary op, or a
/// templated string — any of those shapes round-trips through `format!` the
/// same way. Container variants hold a recursive `Vec<NativeStmt>` for
/// their body.
#[derive(Debug, Clone)]
enum NativeStmt {
    VarInit {
        name: String,
        ty: PaxType,
        /// `None` for `var x: T` no-initializer form.
        value: Option<String>,
    },
    Assign {
        name: String,
        op: &'static str,
        value: String,
    },
    /// A pax `let` binding recovered from a `Compose` action. `name` is the
    /// pax-side identifier (the part after `Compose_` in the PA action key).
    /// The action key on re-encode will be `Compose_<name>` again, so this
    /// only fires when the original PA key has the `Compose_<id>` shape with
    /// `<id>` being a valid pax identifier.
    Let { name: String, value: String },
    /// Pax `if <cond> { ... } [else { ... }]`. `else_body` is empty for the
    /// no-else form; PA's `else: { actions: {} }` and a missing `else` key
    /// both decode to an empty vector here.
    If {
        condition: String,
        then_body: Vec<NativeStmt>,
        else_body: Vec<NativeStmt>,
    },
    /// Pax `foreach <iter> in <collection> { ... }`. `iter` is the pax
    /// iterator name in scope inside the body (derived from the PA action
    /// key). Body code referencing `items('<pa_action_key>')` resolves to
    /// this iterator name via the `iterators` field of `RenderCtx`.
    Foreach {
        iter: String,
        collection: String,
        body: Vec<NativeStmt>,
    },
    /// Pax `until <cond> [max N] [timeout "..."] { ... }`. PA's defaults
    /// (`count: 60`, `timeout: "PT1H"`) decode to `None`, so re-emit
    /// reproduces the same defaults.
    Until {
        condition: String,
        max: Option<u32>,
        timeout: Option<String>,
        body: Vec<NativeStmt>,
    },
    /// Pax `switch <subject> { case <lit> { ... } default { ... } }`. Each
    /// `cases` tuple holds the case-value pax source (a literal expression)
    /// and the case body. `default` is `None` when PA's source omitted the
    /// `default` block entirely; an explicit empty `default { }` decodes to
    /// `Some(vec![])` so the source's intent round-trips.
    Switch {
        subject: String,
        cases: Vec<(String, Vec<NativeStmt>)>,
        default: Option<Vec<NativeStmt>>,
    },
    /// Pax `scope [<name>] { ... }`. Anonymous scope when `name` is None.
    Scope {
        name: Option<String>,
        body: Vec<NativeStmt>,
    },
    /// Pax `terminate <status> [message]`. The PA action is `Terminate` with
    /// `inputs.runStatus` carrying the status and an optional
    /// `inputs.runError.message` carrying the message (only when status is
    /// Failed). Message is already-rendered pax source (a string literal or
    /// PA-expression-translated value); when None, no message clause is
    /// emitted.
    Terminate {
        status: TerminateKind,
        message: Option<String>,
    },
    /// Pax `on <statuses> <target> { ... }`. Decoded from a Scope action
    /// whose `runAfter` is handler-style (a single target with one or more
    /// non-Succeeded statuses, or a status set that PA wouldn't generate as
    /// a structural sibling-chain edge). `target` is the pax-side
    /// identifier referencing the addressable scope or pa block this
    /// handler is attached to. `statuses` is non-empty and in source order
    /// (the order they appeared in PA's runAfter status array).
    OnHandler {
        statuses: Vec<HandlerKind>,
        target: String,
        body: Vec<NativeStmt>,
    },
    /// Fallback: `pa <Name>` referencing a `pa/<Name>.json` file written
    /// alongside. Used when an action type isn't natively decodable, OR
    /// when a container's required structural piece (condition expression,
    /// foreach collection, switch subject, etc.) doesn't render — the whole
    /// container falls back as one opaque action.
    Pa { name: String },
}

/// The three runStatus values PA's Terminate action accepts. Mirrors
/// `ast::TerminateStatus` but lives in the decoder so the decoder doesn't
/// reach across module boundaries for an enum it owns the meaning of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminateKind {
    Succeeded,
    Failed,
    Cancelled,
}

impl TerminateKind {
    fn from_pa(s: &str) -> Option<Self> {
        match s {
            "Succeeded" => Some(TerminateKind::Succeeded),
            "Failed" => Some(TerminateKind::Failed),
            "Cancelled" => Some(TerminateKind::Cancelled),
            _ => None,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            TerminateKind::Succeeded => "succeeded",
            TerminateKind::Failed => "failed",
            TerminateKind::Cancelled => "cancelled",
        }
    }
}

/// The four runAfter statuses PA recognizes. Mirrors `ast::HandlerStatus`.
/// The on-handler decoder accepts any subset; the formatter joins them with
/// `or` in source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandlerKind {
    Succeeded,
    Failed,
    Skipped,
    TimedOut,
}

impl HandlerKind {
    fn from_pa(s: &str) -> Option<Self> {
        match s {
            "Succeeded" => Some(HandlerKind::Succeeded),
            "Failed" => Some(HandlerKind::Failed),
            "Skipped" => Some(HandlerKind::Skipped),
            "TimedOut" => Some(HandlerKind::TimedOut),
            _ => None,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            HandlerKind::Succeeded => "succeeded",
            HandlerKind::Failed => "failed",
            HandlerKind::Skipped => "skipped",
            HandlerKind::TimedOut => "timedout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaxType {
    Int,
    Float,
    String,
    Bool,
    Array,
    Object,
}

impl PaxType {
    fn from_pa(s: &str) -> Option<Self> {
        // PA's designer exports lowercase variable types ("integer",
        // "string"); paxc's emitter writes the capitalized form
        // ("Integer", "String"). Both shapes appear in the wild — paxc must
        // round-trip cleanly across that case difference.
        match s.to_ascii_lowercase().as_str() {
            "integer" => Some(PaxType::Int),
            "float" => Some(PaxType::Float),
            "string" => Some(PaxType::String),
            "boolean" => Some(PaxType::Bool),
            "array" => Some(PaxType::Array),
            "object" => Some(PaxType::Object),
            _ => None,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            PaxType::Int => "int",
            PaxType::Float => "float",
            PaxType::String => "string",
            PaxType::Bool => "bool",
            PaxType::Array => "array",
            PaxType::Object => "object",
        }
    }
}

/// Mutable state threaded through every decode call: the shared `pa/`
/// directory, the global used-names set (so fallback action keys are unique
/// across the whole flow including nested fallbacks), the original→normalized
/// name map, the list of files written to disk, the warnings vec, and the
/// target-names map (PA action key → pax-side addressable identifier, used
/// by on-handler decoding to look up the target's pax name).
///
/// Per-scope state (bindings, iterators) is passed separately by value so
/// nested scopes get clone-and-extend semantics — an inner `let` in an if
/// branch doesn't leak to outer code, mirroring pax's own resolver scoping.
struct DecodeCtx<'a> {
    pa_dir: &'a Path,
    used_names: &'a mut HashSet<String>,
    name_map: &'a mut HashMap<String, String>,
    pa_files_written: &'a mut Vec<PathBuf>,
    warnings: &'a mut Vec<String>,
    /// PA action key → pax-side identifier, registered as actions decode.
    /// Only populated for actions an `on <status> <target>` handler can
    /// reference: named scopes (Scope_<label> → label) and pa blocks
    /// (PA_key → normalized_pax_name). A natively-decoded variable, let,
    /// foreach, etc. has no pax-side identifier addressable from outside,
    /// so it never enters this map; an on-handler whose target is one of
    /// those falls back.
    target_names: &'a mut HashMap<String, String>,
}

/// Decode a PA actions map (top-level or inside a container) into a vector
/// of pax statements in source order. Recurses into container bodies via
/// `try_decode_container`. Each child action either decodes natively or
/// falls back to a `NativeStmt::Pa` (with its body written eagerly to
/// `pa/<Name>.json`).
///
/// `bindings` and `iterators` describe the pax scope visible at this body's
/// entry. The function clones them locally and extends as inner statements
/// introduce new pax-side names (vars, lets, foreach iters), but the
/// extended sets do NOT leak back to the caller — that's how pax's lexical
/// scoping is preserved through the round-trip.
fn decode_actions_block(
    actions: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Vec<NativeStmt>, DecodeError> {
    let order = topo_sort_actions(actions)?;
    let mut local_bindings = bindings.clone();
    let local_iterators = iterators.clone();
    let mut out: Vec<NativeStmt> = Vec::with_capacity(order.len());

    for original_key in &order {
        let action = &actions[original_key];
        let action_obj = action.as_object().ok_or_else(|| {
            DecodeError::BadShape(format!("action `{original_key}` is not an object"))
        })?;
        let action_type = action_obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let handler_edge = is_handler_runafter(action_obj.get("runAfter"));

        let stmt = if handler_edge {
            // Try native on-handler decoding first. The target's pax-side
            // name comes from `ctx.target_names`, populated by earlier
            // iterations as Scopes and Pa fallbacks were processed.
            try_decode_on_handler(
                action_obj,
                &action_type,
                &local_bindings,
                &local_iterators,
                ctx,
            )?
        } else {
            let render_ctx = paexpr::RenderCtx::new(&local_bindings, &local_iterators);
            match try_decode_native(original_key, &action_type, action_obj, &render_ctx) {
                Some(s) if native_target_available(&s, &local_bindings) => Some(s),
                _ => try_decode_container(
                    original_key,
                    &action_type,
                    action_obj,
                    &local_bindings,
                    &local_iterators,
                    ctx,
                )?,
            }
        };

        let resolved = match stmt {
            Some(s) => {
                track_new_binding(&s, &mut local_bindings);
                s
            }
            None => {
                let reason = if handler_edge {
                    " (handler-style runAfter; target not addressable as a scope/pa name)"
                } else {
                    ""
                };
                ctx.warnings.push(format!(
                    "note: action `{original_key}` (type {}){reason} not decoded natively; emitted as pa block",
                    if action_type.is_empty() {
                        "<unknown>"
                    } else {
                        action_type.as_str()
                    }
                ));
                fallback_to_pa(original_key, action, ctx)?
            }
        };

        // Register addressable targets so later on-handlers can find them.
        register_target_name(original_key, &resolved, ctx);
        out.push(resolved);
    }

    Ok(out)
}

/// Record a PA action key → pax-side identifier mapping when the resolved
/// statement is something an on-handler can target. Named scopes register
/// their label; pa-block fallbacks register their normalized name. Other
/// shapes (variables, lets, anonymous scopes, containers) have no
/// addressable identifier from outside their declaration site, so they're
/// not registered — a handler whose target is one of them would fall back.
fn register_target_name(pa_key: &str, stmt: &NativeStmt, ctx: &mut DecodeCtx<'_>) {
    match stmt {
        NativeStmt::Scope {
            name: Some(label), ..
        } => {
            ctx.target_names.insert(pa_key.to_string(), label.clone());
        }
        NativeStmt::Pa { name } => {
            ctx.target_names.insert(pa_key.to_string(), name.clone());
        }
        _ => {}
    }
}

/// Try to decode a Scope action with handler-style runAfter as a pax
/// `on <statuses> <target> { ... }` block. Returns Ok(None) when:
///   - the action isn't a Scope (handlers in PA are always Scope-shaped),
///   - the runAfter has multiple target keys (pax `on` addresses ONE target),
///   - the target's pax-side identifier isn't known yet (it's an action
///     type pax can't reference by name — variable lifecycle, let,
///     anonymous scope, container — or it's somehow not in topo-sort
///     order before the handler).
///
/// In any of those cases the caller falls back to a pa block. The whole
/// handler body still tries to decode natively (each child action goes
/// through the normal path), so even a fallback handler keeps inner
/// natives where possible.
fn try_decode_on_handler(
    action: &Map<String, Value>,
    action_type: &str,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    if action_type != "Scope" {
        return Ok(None);
    }
    let run_after = match action.get("runAfter").and_then(Value::as_object) {
        Some(o) if o.len() == 1 => o,
        _ => return Ok(None),
    };
    let (target_pa_key, statuses_value) = run_after.iter().next().unwrap();
    let Some(statuses_arr) = statuses_value.as_array() else {
        return Ok(None);
    };
    let mut statuses: Vec<HandlerKind> = Vec::with_capacity(statuses_arr.len());
    for s in statuses_arr {
        let Some(kind) = s.as_str().and_then(HandlerKind::from_pa) else {
            return Ok(None);
        };
        statuses.push(kind);
    }
    if statuses.is_empty() {
        return Ok(None);
    }
    let Some(target) = ctx.target_names.get(target_pa_key).cloned() else {
        return Ok(None);
    };
    let empty = Map::new();
    let inner = action
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let body = decode_actions_block(inner, bindings, iterators, ctx)?;
    Ok(Some(NativeStmt::OnHandler {
        statuses,
        target,
        body,
    }))
}

/// Track new pax bindings introduced by a freshly-decoded statement. Vars
/// and lets both go into the same set (pax shares one namespace).
fn track_new_binding(stmt: &NativeStmt, bindings: &mut HashSet<String>) {
    match stmt {
        NativeStmt::VarInit { name, .. } | NativeStmt::Let { name, .. } => {
            bindings.insert(name.clone());
        }
        _ => {}
    }
}

/// Write the action's body verbatim to a `pa/<Name>.json` file (with a
/// uniquified normalized name) and return a `NativeStmt::Pa` referring to it.
fn fallback_to_pa(
    original_key: &str,
    action: &Value,
    ctx: &mut DecodeCtx<'_>,
) -> Result<NativeStmt, DecodeError> {
    let normalized = normalize_action_key(original_key, ctx.used_names);
    if normalized != *original_key {
        ctx.name_map
            .insert(original_key.to_string(), normalized.clone());
    }
    let path = ctx.pa_dir.join(format!("{normalized}.json"));
    write_json(&path, action)?;
    ctx.pa_files_written.push(path);
    Ok(NativeStmt::Pa { name: normalized })
}

/// Returns Some(stmt) for action types we know how to natively lower. None
/// forces fallback to `pa <Name>`. The `action_key` is needed for Compose
/// because the let-binding name comes from the key suffix, not the inputs.
/// The `ctx` is threaded into `paexpr::json_to_pax` so PA expression
/// translation can resolve `variables('x')` / `outputs('Compose_y')` against
/// the bindings already declared.
///
/// Container types (If/Foreach/Until/Switch/Scope) are NOT handled here —
/// they go through `try_decode_container` which has the additional plumbing
/// to recurse into child bodies and write any nested fallback files.
fn try_decode_native(
    action_key: &str,
    action_type: &str,
    action: &Map<String, Value>,
    ctx: &paexpr::RenderCtx<'_>,
) -> Option<NativeStmt> {
    match action_type {
        "InitializeVariable" => {
            let var_entry = action
                .get("inputs")?
                .get("variables")?
                .as_array()?
                .first()?
                .as_object()?;
            let name = var_entry.get("name")?.as_str()?.to_string();
            let ty_str = var_entry.get("type")?.as_str()?;
            let ty = PaxType::from_pa(ty_str)?;
            let value = match var_entry.get("value") {
                Some(v) => Some(paexpr::json_to_pax(v, ctx)?),
                None => None,
            };
            Some(NativeStmt::VarInit { name, ty, value })
        }
        "SetVariable" => decode_simple_assign(action, "=", ctx),
        "IncrementVariable" => decode_simple_assign(action, "+=", ctx),
        "DecrementVariable" => decode_simple_assign(action, "-=", ctx),
        "AppendToStringVariable" => decode_simple_assign(action, "&=", ctx),
        "AppendToArrayVariable" => decode_simple_assign(action, "+=", ctx),
        "Compose" => {
            // The pax name lives in the action key suffix: `Compose_<id>`.
            // A bare `Compose` (PA designer's default for the first one) has
            // no recoverable pax name and falls back. A suffix that isn't a
            // valid pax identifier also falls back.
            let name = compose_let_name(action_key)?;
            let inputs = action.get("inputs")?;
            let value = paexpr::json_to_pax(inputs, ctx)?;
            Some(NativeStmt::Let { name, value })
        }
        "Terminate" => decode_terminate(action, ctx),
        _ => None,
    }
}

/// Recover `terminate <status> [message]` from a PA Terminate action.
/// Returns None if `runStatus` is missing or unrecognized; for Failed with
/// a `runError.message`, the message must render via paexpr (literal or
/// translatable expression), otherwise the whole action falls back so the
/// message isn't lost. Other status values can't carry a message in pax,
/// so an extraneous `runError` block on Succeeded/Cancelled is dropped
/// quietly — PA's contract is that those statuses don't use the field.
fn decode_terminate(
    action: &Map<String, Value>,
    ctx: &paexpr::RenderCtx<'_>,
) -> Option<NativeStmt> {
    let inputs = action.get("inputs")?.as_object()?;
    let status_str = inputs.get("runStatus")?.as_str()?;
    let status = TerminateKind::from_pa(status_str)?;
    let message = if status == TerminateKind::Failed {
        match inputs
            .get("runError")
            .and_then(Value::as_object)
            .and_then(|o| o.get("message"))
        {
            Some(v) => Some(paexpr::json_to_pax(v, ctx)?),
            None => None,
        }
    } else {
        None
    };
    Some(NativeStmt::Terminate { status, message })
}

/// Container action types: If/Foreach/Until/Switch/Scope. Each recurses
/// into child bodies via `decode_actions_block`. Returns Ok(None) when the
/// container's required structural piece (condition, collection, subject,
/// case literal) can't be rendered — the caller then falls back to
/// `pa <Name>` for the whole container as one opaque action. Returns
/// Ok(Some(stmt)) for natively decoded containers.
fn try_decode_container(
    action_key: &str,
    action_type: &str,
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    match action_type {
        "Scope" => decode_scope(action_key, action, bindings, iterators, ctx),
        "If" => decode_if(action, bindings, iterators, ctx),
        "Foreach" => decode_foreach(action_key, action, bindings, iterators, ctx),
        "Until" => decode_until(action, bindings, iterators, ctx),
        "Switch" => decode_switch(action, bindings, iterators, ctx),
        _ => Ok(None),
    }
}

fn decode_scope(
    action_key: &str,
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    let empty = Map::new();
    let inner = action
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let body = decode_actions_block(inner, bindings, iterators, ctx)?;
    let name = scope_label(action_key);
    Ok(Some(NativeStmt::Scope { name, body }))
}

/// Recover the optional pax label from a Scope action key. PA designer's
/// default for an unnamed scope is the bare key `Scope` (or `Scope_2`,
/// `Scope_3` for collisions); paxc's named scope uses `Scope_<label>`. We
/// return None for the bare/auto-suffixed forms so the source reads as
/// `scope { ... }`, and Some(label) for `Scope_<label>` where label is a
/// valid pax identifier.
fn scope_label(action_key: &str) -> Option<String> {
    let suffix = action_key.strip_prefix("Scope_")?;
    // Numeric-only suffix is paxc's auto-uniquify pattern (`Scope_2`,
    // `Scope_3`); render as anonymous.
    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !is_pax_identifier(suffix) {
        return None;
    }
    Some(suffix.to_string())
}

fn decode_if(
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    let render_ctx = paexpr::RenderCtx::new(bindings, iterators);
    let Some(condition) = action
        .get("expression")
        .and_then(|v| paexpr::condition_value_to_pax(v, &render_ctx))
    else {
        return Ok(None);
    };
    let empty = Map::new();
    let then_actions = action
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let else_actions = action
        .get("else")
        .and_then(|e| e.get("actions"))
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let then_body = decode_actions_block(then_actions, bindings, iterators, ctx)?;
    let else_body = decode_actions_block(else_actions, bindings, iterators, ctx)?;
    Ok(Some(NativeStmt::If {
        condition,
        then_body,
        else_body,
    }))
}

fn decode_foreach(
    action_key: &str,
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    let render_ctx = paexpr::RenderCtx::new(bindings, iterators);
    let Some(collection) = action
        .get("foreach")
        .and_then(|v| paexpr::json_to_pax(v, &render_ctx))
    else {
        return Ok(None);
    };
    // Pick a pax-side iterator name from the PA action key. PA's items()
    // calls inside the body reference the action key; mapping
    // PA_action_key → pax_iter_name in the body's RenderCtx lets the
    // translator resolve those calls.
    let iter = foreach_iter_name(action_key, bindings, iterators);
    let mut child_iterators = iterators.clone();
    child_iterators.insert(action_key.to_string(), iter.clone());
    let empty = Map::new();
    let inner = action
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let body = decode_actions_block(inner, bindings, &child_iterators, ctx)?;
    Ok(Some(NativeStmt::Foreach {
        iter,
        collection,
        body,
    }))
}

/// Derive a pax-side iterator name from a PA `Foreach` action key. We start
/// with the normalized key, then suffix `_2`, `_3`, ... if it would shadow
/// an outer binding or iterator, or collide with a pax keyword. The result
/// is always a valid pax identifier.
fn foreach_iter_name(
    action_key: &str,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
) -> String {
    let mut taken: HashSet<String> = HashSet::new();
    let base = if is_pax_identifier(action_key) {
        action_key.to_string()
    } else {
        let mut local_used = HashSet::new();
        normalize_action_key(action_key, &mut local_used)
    };
    let mut candidate = base.clone();
    let mut n = 2u32;
    loop {
        let collides = bindings.contains(&candidate)
            || iterators.values().any(|v| v == &candidate)
            || taken.contains(&candidate)
            || PAX_RESERVED.contains(&candidate.as_str());
        if !collides {
            return candidate;
        }
        taken.insert(candidate.clone());
        candidate = format!("{base}_{n}");
        n += 1;
    }
}

const PAX_RESERVED: &[&str] = &[
    "var",
    "let",
    "if",
    "else",
    "foreach",
    "in",
    "until",
    "pa",
    "debug",
    "terminate",
    "switch",
    "case",
    "default",
    "scope",
    "on",
    "null",
    "true",
    "false",
    "int",
    "float",
    "string",
    "bool",
    "array",
    "object",
    "max",
    "timeout",
];

fn is_pax_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn decode_until(
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    let render_ctx = paexpr::RenderCtx::new(bindings, iterators);
    let Some(condition) = action
        .get("expression")
        .and_then(|v| paexpr::condition_value_to_pax(v, &render_ctx))
    else {
        return Ok(None);
    };
    // PA's defaults: count=60, timeout="PT1H". Decode to None so the pax
    // source omits `max` / `timeout` and the encoder reapplies the defaults
    // identically.
    let limit = action.get("limit").and_then(Value::as_object);
    let max = limit
        .and_then(|l| l.get("count"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n != 60);
    let timeout = limit
        .and_then(|l| l.get("timeout"))
        .and_then(Value::as_str)
        .filter(|&t| t != "PT1H")
        .map(str::to_string);
    let empty = Map::new();
    let inner = action
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let body = decode_actions_block(inner, bindings, iterators, ctx)?;
    Ok(Some(NativeStmt::Until {
        condition,
        max,
        timeout,
        body,
    }))
}

fn decode_switch(
    action: &Map<String, Value>,
    bindings: &HashSet<String>,
    iterators: &HashMap<String, String>,
    ctx: &mut DecodeCtx<'_>,
) -> Result<Option<NativeStmt>, DecodeError> {
    let render_ctx = paexpr::RenderCtx::new(bindings, iterators);
    let Some(subject) = action
        .get("expression")
        .and_then(|v| paexpr::json_to_pax(v, &render_ctx))
    else {
        return Ok(None);
    };
    let empty_map = Map::new();
    let cases_map = action
        .get("cases")
        .and_then(Value::as_object)
        .unwrap_or(&empty_map);
    let mut cases: Vec<(String, Vec<NativeStmt>)> = Vec::with_capacity(cases_map.len());
    for case_body in cases_map.values() {
        let case_obj = case_body
            .as_object()
            .ok_or_else(|| DecodeError::BadShape("Switch case is not an object".to_string()))?;
        let Some(case_value) = case_obj.get("case").and_then(switch_case_literal) else {
            return Ok(None);
        };
        let inner = case_obj
            .get("actions")
            .and_then(Value::as_object)
            .unwrap_or(&empty_map);
        let body = decode_actions_block(inner, bindings, iterators, ctx)?;
        cases.push((case_value, body));
    }
    let default = match action.get("default").and_then(Value::as_object) {
        Some(d) => {
            let inner = d
                .get("actions")
                .and_then(Value::as_object)
                .unwrap_or(&empty_map);
            Some(decode_actions_block(inner, bindings, iterators, ctx)?)
        }
        None => None,
    };
    Ok(Some(NativeStmt::Switch {
        subject,
        cases,
        default,
    }))
}

/// Render a case-value JSON literal as pax source. PA's Switch only allows
/// scalar literals here (string / int / bool / float), and pax mirrors that
/// constraint via `SwitchCase::value: Literal`. Anything else falls back.
fn switch_case_literal(v: &Value) -> Option<String> {
    paexpr::json_to_pa_lit(v).and_then(|lit| {
        // EmptyArray / EmptyObject would render but PA's Switch wouldn't
        // accept them as case values. Reject defensively.
        match lit {
            paexpr::PaLit::EmptyArray | paexpr::PaLit::EmptyObject | paexpr::PaLit::Null => None,
            _ => paexpr::render_pa_lit(&lit),
        }
    })
}

/// Recover the pax let-binding name from a `Compose_<id>` action key. Returns
/// None for any key that doesn't have the `Compose_` prefix or whose suffix
/// isn't a valid pax identifier (`[A-Za-z_][A-Za-z0-9_]*`).
fn compose_let_name(action_key: &str) -> Option<String> {
    let suffix = action_key.strip_prefix("Compose_")?;
    if suffix.is_empty() {
        return None;
    }
    let mut chars = suffix.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(suffix.to_string())
}

/// Decides whether a candidate native statement's downstream targets are
/// reachable in the pax we'll emit. VarInit and Let always *introduce* a
/// new binding; Assign only fires if its target was already introduced. The
/// gate prevents us from emitting `x = 5` when `var x` fell back. Let with
/// a name that already exists in `declared` collides with the existing
/// binding (pax shares a namespace across vars and lets) and falls back.
fn native_target_available(stmt: &NativeStmt, declared: &HashSet<String>) -> bool {
    match stmt {
        NativeStmt::VarInit { name, .. } | NativeStmt::Let { name, .. } => !declared.contains(name),
        NativeStmt::Assign { name, .. } => declared.contains(name),
        // Container statements always pass this gate — their internals are
        // gated separately by the recursive decode in their bodies. The
        // `Pa` fallback is unconditional (the body's already on disk).
        // `Terminate` and `OnHandler` are leaf-or-container statements
        // with no binding interaction.
        NativeStmt::If { .. }
        | NativeStmt::Foreach { .. }
        | NativeStmt::Until { .. }
        | NativeStmt::Switch { .. }
        | NativeStmt::Scope { .. }
        | NativeStmt::Terminate { .. }
        | NativeStmt::OnHandler { .. }
        | NativeStmt::Pa { .. } => true,
    }
}

fn decode_simple_assign(
    action: &Map<String, Value>,
    op: &'static str,
    ctx: &paexpr::RenderCtx<'_>,
) -> Option<NativeStmt> {
    let inputs = action.get("inputs")?.as_object()?;
    let name = inputs.get("name")?.as_str()?.to_string();
    let value = paexpr::json_to_pax(inputs.get("value")?, ctx)?;
    Some(NativeStmt::Assign { name, op, value })
}

/// Render a single statement to pax source at the given indent depth. The
/// returned string has no leading indent on the first line — the caller
/// glues it after its own prefix — but inner lines (container body lines)
/// carry the appropriate indentation.
fn format_native_stmt(stmt: &NativeStmt, depth: usize) -> String {
    let indent = PAX_INDENT_UNIT.repeat(depth);
    match stmt {
        NativeStmt::VarInit { name, ty, value } => match value {
            Some(v) => format!("{indent}var {name}: {} = {v}", ty.keyword()),
            None => format!("{indent}var {name}: {}", ty.keyword()),
        },
        NativeStmt::Assign { name, op, value } => format!("{indent}{name} {op} {value}"),
        NativeStmt::Let { name, value } => format!("{indent}let {name} = {value}"),
        NativeStmt::Pa { name } => format!("{indent}pa {name}"),
        NativeStmt::If {
            condition,
            then_body,
            else_body,
        } => {
            let mut s = format!("{indent}if {condition} {{\n");
            push_body(&mut s, then_body, depth + 1);
            if else_body.is_empty() {
                s.push_str(&format!("{indent}}}"));
            } else {
                s.push_str(&format!("{indent}}} else {{\n"));
                push_body(&mut s, else_body, depth + 1);
                s.push_str(&format!("{indent}}}"));
            }
            s
        }
        NativeStmt::Foreach {
            iter,
            collection,
            body,
        } => {
            let mut s = format!("{indent}foreach {iter} in {collection} {{\n");
            push_body(&mut s, body, depth + 1);
            s.push_str(&format!("{indent}}}"));
            s
        }
        NativeStmt::Until {
            condition,
            max,
            timeout,
            body,
        } => {
            let mut header = format!("{indent}until {condition}");
            if let Some(n) = max {
                header.push_str(&format!(" max {n}"));
            }
            if let Some(t) = timeout {
                header.push_str(&format!(" timeout \"{t}\""));
            }
            let mut s = format!("{header} {{\n");
            push_body(&mut s, body, depth + 1);
            s.push_str(&format!("{indent}}}"));
            s
        }
        NativeStmt::Switch {
            subject,
            cases,
            default,
        } => {
            let mut s = format!("{indent}switch {subject} {{\n");
            let inner_indent = PAX_INDENT_UNIT.repeat(depth + 1);
            for (case_value, body) in cases {
                s.push_str(&format!("{inner_indent}case {case_value} {{\n"));
                push_body(&mut s, body, depth + 2);
                s.push_str(&format!("{inner_indent}}}\n"));
            }
            if let Some(body) = default {
                s.push_str(&format!("{inner_indent}default {{\n"));
                push_body(&mut s, body, depth + 2);
                s.push_str(&format!("{inner_indent}}}\n"));
            }
            s.push_str(&format!("{indent}}}"));
            s
        }
        NativeStmt::Scope { name, body } => {
            let header = match name {
                Some(n) => format!("{indent}scope {n} {{"),
                None => format!("{indent}scope {{"),
            };
            let mut s = format!("{header}\n");
            push_body(&mut s, body, depth + 1);
            s.push_str(&format!("{indent}}}"));
            s
        }
        NativeStmt::Terminate { status, message } => match message {
            Some(msg) => format!("{indent}terminate {} {msg}", status.keyword()),
            None => format!("{indent}terminate {}", status.keyword()),
        },
        NativeStmt::OnHandler {
            statuses,
            target,
            body,
        } => {
            let status_part = statuses
                .iter()
                .map(|s| s.keyword())
                .collect::<Vec<_>>()
                .join(" or ");
            let mut s = format!("{indent}on {status_part} {target} {{\n");
            push_body(&mut s, body, depth + 1);
            s.push_str(&format!("{indent}}}"));
            s
        }
    }
}

fn push_body(out: &mut String, body: &[NativeStmt], depth: usize) {
    for stmt in body {
        out.push_str(&format_native_stmt(stmt, depth));
        out.push('\n');
    }
}

/// Map a PA action key to a valid pax identifier. Replaces every char outside
/// `[A-Za-z0-9_]` with `_`, collapses runs, strips leading/trailing `_`, and
/// suffixes `_2`/`_3`/... on collision with `used`.
pub fn normalize_action_key(original: &str, used: &mut HashSet<String>) -> String {
    let base = base_normalize(original);
    if !used.contains(&base) {
        used.insert(base.clone());
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}_{n}");
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

fn base_normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_underscore = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            prev_underscore = ch == '_';
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        return "_action".to_string();
    }
    if trimmed
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return format!("_{trimmed}");
    }
    trimmed
}

fn ensure_dir(path: &Path) -> Result<(), DecodeError> {
    fs::create_dir_all(path).map_err(|e| DecodeError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn write_json(path: &Path, value: &Value) -> Result<(), DecodeError> {
    let text = serde_json::to_string_pretty(value).expect("re-serializing parsed JSON cannot fail");
    fs::write(path, text).map_err(|e| DecodeError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn is_empty_obj(v: &Value) -> bool {
    matches!(v, Value::Object(map) if map.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn used() -> HashSet<String> {
        HashSet::new()
    }

    // ---------- 45a polish: zip-input detection ----------

    #[test]
    fn match_flow_definition_path_recognizes_legacy_layout() {
        assert_eq!(
            match_flow_definition_path("Microsoft.Flow/flows/abc-123/definition.json"),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn match_flow_definition_path_rejects_other_files() {
        assert!(match_flow_definition_path("Microsoft.Flow/flows/abc-123/manifest.json").is_none());
        assert!(match_flow_definition_path("manifest.json").is_none());
        assert!(
            match_flow_definition_path("Microsoft.Flow/flows/abc-123/sub/definition.json")
                .is_none()
        );
        assert!(match_flow_definition_path("Microsoft.Flow/flows//definition.json").is_none());
    }

    // ---------- normalize_action_key ----------

    #[test]
    fn normalize_passes_through_valid_identifier() {
        let mut u = used();
        assert_eq!(normalize_action_key("Initialize_x", &mut u), "Initialize_x");
        assert!(u.contains("Initialize_x"));
    }

    #[test]
    fn normalize_replaces_parens_with_underscore() {
        let mut u = used();
        assert_eq!(
            normalize_action_key("Send_an_email_(V2)", &mut u),
            "Send_an_email_V2"
        );
    }

    #[test]
    fn normalize_collapses_runs_and_strips_edges() {
        let mut u = used();
        // ! becomes _, then runs collapse, then leading/trailing _ get trimmed.
        assert_eq!(
            normalize_action_key("if_Maintenance_AND_!Leadership", &mut u),
            "if_Maintenance_AND_Leadership"
        );
    }

    #[test]
    fn normalize_handles_collision_with_suffix() {
        let mut u = used();
        assert_eq!(normalize_action_key("Foo (1)", &mut u), "Foo_1");
        assert_eq!(normalize_action_key("Foo (1)", &mut u), "Foo_1_2");
        assert_eq!(normalize_action_key("Foo_1", &mut u), "Foo_1_3");
    }

    #[test]
    fn normalize_prefixes_leading_digit() {
        let mut u = used();
        assert_eq!(normalize_action_key("3rd action", &mut u), "_3rd_action");
    }

    #[test]
    fn normalize_falls_back_for_all_invalid() {
        let mut u = used();
        assert_eq!(normalize_action_key("!@#$%^", &mut u), "_action");
    }

    // ---------- topo_sort_actions ----------

    #[test]
    fn topo_sort_linear_chain() {
        let actions: Map<String, Value> = serde_json::from_value(json!({
            "B": { "type": "Compose", "runAfter": { "A": ["Succeeded"] } },
            "A": { "type": "Compose", "runAfter": {} },
            "C": { "type": "Compose", "runAfter": { "B": ["Succeeded"] } }
        }))
        .unwrap();
        let order = topo_sort_actions(&actions).unwrap();
        assert_eq!(order, vec!["A", "B", "C"]);
    }

    #[test]
    fn topo_sort_alpha_tiebreak_for_independent_actions() {
        let actions: Map<String, Value> = serde_json::from_value(json!({
            "Z_init": { "type": "Compose", "runAfter": {} },
            "A_init": { "type": "Compose", "runAfter": {} },
            "M_init": { "type": "Compose", "runAfter": {} }
        }))
        .unwrap();
        let order = topo_sort_actions(&actions).unwrap();
        assert_eq!(order, vec!["A_init", "M_init", "Z_init"]);
    }

    #[test]
    fn topo_sort_detects_cycle() {
        let actions: Map<String, Value> = serde_json::from_value(json!({
            "A": { "type": "Compose", "runAfter": { "B": ["Succeeded"] } },
            "B": { "type": "Compose", "runAfter": { "A": ["Succeeded"] } }
        }))
        .unwrap();
        let err = topo_sort_actions(&actions).unwrap_err();
        assert!(matches!(err, DecodeError::Cycle { .. }));
    }

    #[test]
    fn topo_sort_ignores_external_predecessors() {
        // PA flows can have actions inside containers whose runAfter refers
        // to actions in a parent scope. At the level we sort, those should
        // be treated as zero-in-degree.
        let actions: Map<String, Value> = serde_json::from_value(json!({
            "Inner": { "type": "Compose", "runAfter": { "OutsideThisScope": ["Succeeded"] } }
        }))
        .unwrap();
        let order = topo_sort_actions(&actions).unwrap();
        assert_eq!(order, vec!["Inner"]);
    }

    // ---------- try_decode_native ----------

    /// Test helper: pull out type + object and call try_decode_native with
    /// a placeholder action key. Tests that care about the key (Compose let
    /// extraction) call try_decode_native directly with their own key.
    /// Bindings default to empty; tests exercising expression translation
    /// against declared bindings use `try_decode_with_bindings` instead.
    fn try_decode(action: &Value) -> Option<NativeStmt> {
        let bindings: HashSet<String> = HashSet::new();
        let iters = HashMap::new();
        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        try_decode_native(
            "TestKey",
            action["type"].as_str().unwrap(),
            action.as_object().unwrap(),
            &ctx,
        )
    }

    fn try_decode_with_bindings(action: &Value, names: &[&str]) -> Option<NativeStmt> {
        let bindings: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        let iters = HashMap::new();
        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        try_decode_native(
            "TestKey",
            action["type"].as_str().unwrap(),
            action.as_object().unwrap(),
            &ctx,
        )
    }

    #[test]
    fn native_initialize_variable_with_int_literal() {
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "counter", "type": "Integer", "value": 5 }]
            }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "var counter: int = 5");
    }

    #[test]
    fn native_initialize_variable_accepts_lowercase_pa_type() {
        // PA designer exports lowercase ("integer", "string"); paxc emits
        // capitalized ("Integer", "String"). Both round-trip.
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "todo", "type": "string" }]
            }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "var todo: string");
    }

    #[test]
    fn native_initialize_variable_no_value_field() {
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "todo", "type": "String" }]
            }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "var todo: string");
    }

    #[test]
    fn native_initialize_variable_falls_back_on_pa_expression() {
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "x", "type": "String", "value": "@variables('y')" }]
            }
        });
        assert!(try_decode(&action).is_none());
    }

    #[test]
    fn native_set_increment_decrement_append() {
        let cases = vec![
            (
                json!({"type": "SetVariable", "inputs": { "name": "x", "value": 7 }}),
                "x = 7",
            ),
            (
                json!({"type": "IncrementVariable", "inputs": { "name": "n", "value": 1 }}),
                "n += 1",
            ),
            (
                json!({"type": "DecrementVariable", "inputs": { "name": "n", "value": 2 }}),
                "n -= 2",
            ),
            (
                json!({"type": "AppendToStringVariable", "inputs": { "name": "s", "value": "hi" }}),
                "s &= \"hi\"",
            ),
            (
                json!({"type": "AppendToArrayVariable", "inputs": { "name": "a", "value": "tag" }}),
                "a += \"tag\"",
            ),
        ];
        for (action, expected) in cases {
            let stmt = try_decode(&action).unwrap_or_else(|| panic!("decode failed for: {action}"));
            assert_eq!(format_native_stmt(&stmt, 0), expected);
        }
    }

    #[test]
    fn native_falls_back_for_unknown_action_type() {
        let action = json!({"type": "OpenApiConnection", "inputs": {}});
        assert!(try_decode(&action).is_none());
    }

    // ---------- 44b: Compose -> let ----------

    #[test]
    fn compose_let_name_extracts_valid_suffix() {
        assert_eq!(compose_let_name("Compose_total"), Some("total".to_string()));
        assert_eq!(compose_let_name("Compose_x_1"), Some("x_1".to_string()));
        assert_eq!(
            compose_let_name("Compose__leading_underscore"),
            Some("_leading_underscore".to_string())
        );
    }

    #[test]
    fn compose_let_name_rejects_bare_compose() {
        // PA designer's first unrenamed Compose is keyed exactly "Compose";
        // there's no pax name to recover, so it falls back to pa <Name>.
        assert_eq!(compose_let_name("Compose"), None);
    }

    #[test]
    fn compose_let_name_rejects_invalid_suffix() {
        // Suffix starting with a digit is not a valid pax identifier.
        assert_eq!(compose_let_name("Compose_1"), None);
        // Suffix containing non-identifier chars.
        assert_eq!(compose_let_name("Compose_my-thing"), None);
    }

    #[test]
    fn compose_let_name_rejects_non_compose_prefix() {
        assert_eq!(compose_let_name("Composer_x"), None);
        assert_eq!(compose_let_name("HTTP_Call"), None);
    }

    /// Compose-key-sensitive helper: callers pass the action key so the
    /// `Compose_<id>` suffix-extraction path is exercised directly. Bindings
    /// default to empty; tests that need declared bindings build the ctx
    /// inline.
    fn try_decode_compose(action_key: &str, action: &Value) -> Option<NativeStmt> {
        let bindings: HashSet<String> = HashSet::new();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        try_decode_native(action_key, "Compose", action.as_object().unwrap(), &ctx)
    }

    #[test]
    fn native_compose_with_string_literal_lowers_to_let() {
        let action = json!({"type": "Compose", "inputs": "hello"});
        let bindings: HashSet<String> = HashSet::new();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        let stmt = try_decode_native(
            "Compose_greeting",
            "Compose",
            action.as_object().unwrap(),
            &ctx,
        )
        .unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "let greeting = \"hello\"");
    }

    #[test]
    fn native_compose_with_int_literal_lowers_to_let() {
        let action = json!({"type": "Compose", "inputs": 42});
        let bindings: HashSet<String> = HashSet::new();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        let stmt = try_decode_native(
            "Compose_answer",
            "Compose",
            action.as_object().unwrap(),
            &ctx,
        )
        .unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "let answer = 42");
    }

    #[test]
    fn native_compose_renders_pa_accessor_call() {
        // Slice 45a lifted the forbidden-accessor list; triggerBody() and
        // the other PA accessors now render as native call forms.
        let action = json!({"type": "Compose", "inputs": "@triggerBody()"});
        let stmt = try_decode_compose("Compose_x", &action).expect("should lower natively");
        assert_eq!(format_native_stmt(&stmt, 0), "let x = triggerBody()");
    }

    #[test]
    fn native_compose_falls_back_on_bare_compose_key() {
        // Even a literal-input Compose must fall back when the key has no
        // recoverable pax name suffix.
        let action = json!({"type": "Compose", "inputs": "ok"});
        let bindings: HashSet<String> = HashSet::new();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        assert!(
            try_decode_native("Compose", "Compose", action.as_object().unwrap(), &ctx).is_none()
        );
    }

    #[test]
    fn native_compose_falls_back_on_non_literal_input() {
        // Non-empty arrays/objects can't be rendered in pax source.
        let action = json!({"type": "Compose", "inputs": [1, 2, 3]});
        let bindings: HashSet<String> = HashSet::new();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        assert!(
            try_decode_native("Compose_a", "Compose", action.as_object().unwrap(), &ctx).is_none()
        );
    }

    // ---------- is_handler_runafter ----------

    #[test]
    fn handler_runafter_detects_failed() {
        let ra = json!({"A": ["Failed"]});
        assert!(is_handler_runafter(Some(&ra)));
    }

    #[test]
    fn handler_runafter_passes_succeeded_only() {
        let ra = json!({"A": ["Succeeded"]});
        assert!(!is_handler_runafter(Some(&ra)));
    }

    #[test]
    fn handler_runafter_ignores_empty_or_missing() {
        let empty = json!({});
        assert!(!is_handler_runafter(Some(&empty)));
        assert!(!is_handler_runafter(None));
    }

    // ---------- end-to-end via decode() ----------

    fn tmp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("paxc-decoder-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn decode_minimal_flow_writes_pax_and_trigger() {
        let input = json!({
            "name": "abc",
            "id": "/providers/Microsoft.Flow/flows/abc",
            "type": "Microsoft.Flow/flows",
            "properties": {
                "displayName": "Test Flow",
                "definition": {
                    "$schema": "https://schema.management.azure.com/...",
                    "contentVersion": "1.0.0.0",
                    "triggers": {
                        "manual": { "type": "Request", "kind": "Button", "inputs": {} }
                    },
                    "actions": {
                        "Initialize_counter": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": {
                                "variables": [{ "name": "counter", "type": "Integer", "value": 5 }]
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("minimal");
        let report = decode(&input, "test_flow", &dir).unwrap();

        let pax = read(&report.pax_path);
        assert!(pax.contains("var counter: int = 5"), "pax was: {pax}");

        let trigger_path = dir.join("pa/manual.trigger.json");
        assert!(trigger_path.exists());

        let flow_meta_path = dir.join("pa/flow.json");
        let meta: Value = serde_json::from_str(&read(&flow_meta_path)).unwrap();
        assert_eq!(meta["displayName"], "Test Flow");
    }

    #[test]
    fn decode_falls_back_to_pa_block_for_connector_action() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "contentVersion": "1.0.0.0",
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "HTTP_call": {
                            "type": "Http",
                            "runAfter": {},
                            "inputs": { "method": "GET", "uri": "https://example.com" }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("fallback");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("pa HTTP_call"), "pax was: {pax}");
        assert!(dir.join("pa/HTTP_call.json").exists());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("HTTP_call") && w.contains("Http")),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn decode_normalizes_invalid_action_key_and_records_in_map() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "contentVersion": "1.0.0.0",
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Send_an_email_(V2)": {
                            "type": "OpenApiConnection",
                            "runAfter": {},
                            "inputs": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("normalize");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("pa Send_an_email_V2"), "pax was: {pax}");
        let meta: Value = serde_json::from_str(&read(&dir.join("pa/flow.json"))).unwrap();
        assert_eq!(
            meta["actionNameMap"]["Send_an_email_(V2)"],
            "Send_an_email_V2"
        );
    }

    #[test]
    fn decode_writes_connection_references_when_present() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "contentVersion": "1.0.0.0",
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {}
                },
                "connectionReferences": {
                    "shared_sharepointonline": { "connectionName": "abc" }
                }
            }
        });
        let dir = tmp_dir("conn");
        let report = decode(&input, "x", &dir).unwrap();
        let conn_path = dir.join("pa/connectionReferences.json");
        assert!(conn_path.exists());
        let conn: Value = serde_json::from_str(&read(&conn_path)).unwrap();
        assert_eq!(conn["shared_sharepointonline"]["connectionName"], "abc");
        assert!(report.pa_files_written.contains(&conn_path));
    }

    #[test]
    fn decode_errors_on_multiple_triggers() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": {
                        "trigger_a": { "type": "Request", "kind": "Button", "inputs": {} },
                        "trigger_b": { "type": "Recurrence" }
                    },
                    "actions": {}
                }
            }
        });
        let dir = tmp_dir("multitrigger");
        let err = decode(&input, "x", &dir).unwrap_err();
        assert!(matches!(err, DecodeError::MultipleTriggers(_)));
    }

    #[test]
    fn decode_compose_with_literal_lowers_to_let_end_to_end() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Compose_greeting": {
                            "type": "Compose",
                            "runAfter": {},
                            "inputs": "hello"
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("compose_let");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("let greeting = \"hello\""),
            "expected let lowering; pax was: {pax}"
        );
        assert!(
            !dir.join("pa/Compose_greeting.json").exists(),
            "natively-lowered Compose should not write a pa/ file"
        );
    }

    #[test]
    fn decode_compose_let_collides_with_existing_var_falls_back() {
        // If a var is already declared with a name, a later Compose_<same>
        // would collide in pax's namespace. Force fallback.
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_total": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{ "name": "total", "type": "Integer", "value": 0 }] }
                        },
                        "Compose_total": {
                            "type": "Compose",
                            "runAfter": { "Initialize_total": ["Succeeded"] },
                            "inputs": "anything"
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("compose_collide");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("var total: int = 0"), "pax was: {pax}");
        assert!(
            pax.contains("pa Compose_total"),
            "expected Compose_total to fall back due to name collision; pax was: {pax}"
        );
    }

    #[test]
    fn decode_init_with_pa_expression_now_lowers_natively() {
        // Slice 45a: PA-expression initializers using accessors + subscript
        // path expressions (`@int(triggerOutputs()?['body/x'])`) now lower
        // to native pax `var x: int = ...` instead of falling back. The
        // companion Decrement also lowers natively because `x` is declared.
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_variable_ridx": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": {
                                "variables": [{
                                    "name": "ridx",
                                    "type": "integer",
                                    "value": "@int(triggerOutputs()?['body/x'])"
                                }]
                            }
                        },
                        "Decrement_variable": {
                            "type": "DecrementVariable",
                            "runAfter": { "Initialize_variable_ridx": ["Succeeded"] },
                            "inputs": { "name": "ridx", "value": 1 }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("assign_fallback");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("var ridx: int = int(triggerOutputs()?[\"body/x\"])"),
            "expected ridx initializer to lower natively; got: {pax}"
        );
        assert!(
            pax.contains("ridx -= 1"),
            "expected ridx to be natively decremented; got: {pax}"
        );
    }

    #[test]
    fn decode_handler_edge_falls_back_with_warning() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "A": { "type": "Compose", "runAfter": {}, "inputs": "x" },
                        "On_failure": {
                            "type": "Compose",
                            "runAfter": { "A": ["Failed"] },
                            "inputs": "boom"
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("handler");
        let report = decode(&input, "x", &dir).unwrap();
        // Both Compose actions fall back: `A` because its key has no
        // `Compose_<id>` suffix, `On_failure` because of the handler-style
        // runAfter. The important behavior is that the handler-edge warning
        // surfaces specifically for `On_failure`.
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("On_failure") && w.contains("handler-style")),
            "expected handler warning, got: {:?}",
            report.warnings
        );
    }

    // ---------- 44c: PA expression translation in value slots ----------

    #[test]
    fn native_initialize_with_at_variables_translates_when_target_declared() {
        // A `var y: int = @variables('x')` is fine when the prior actions
        // declared x natively. The decode loop maintains the bindings set;
        // here we check the per-action path with the bindings provided.
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "y", "type": "Integer", "value": "@variables('x')" }]
            }
        });
        let stmt = try_decode_with_bindings(&action, &["x"]).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "var y: int = x");
    }

    #[test]
    fn native_initialize_falls_back_when_referenced_var_undeclared() {
        // Same shape as above but x is unknown. The translator returns
        // None, the decoder falls back to `pa <Name>`.
        let action = json!({
            "type": "InitializeVariable",
            "inputs": {
                "variables": [{ "name": "y", "type": "Integer", "value": "@variables('x')" }]
            }
        });
        assert!(try_decode_with_bindings(&action, &[]).is_none());
    }

    #[test]
    fn native_set_with_arithmetic_lowers_to_pax_operator() {
        let action = json!({
            "type": "SetVariable",
            "inputs": { "name": "x", "value": "@add(variables('x'), 5)" }
        });
        let stmt = try_decode_with_bindings(&action, &["x"]).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "x = (x + 5)");
    }

    #[test]
    fn native_increment_with_variable_reference() {
        let action = json!({
            "type": "IncrementVariable",
            "inputs": { "name": "n", "value": "@variables('m')" }
        });
        let stmt = try_decode_with_bindings(&action, &["n", "m"]).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "n += m");
    }

    #[test]
    fn native_append_to_string_with_template() {
        let action = json!({
            "type": "AppendToStringVariable",
            "inputs": { "name": "msg", "value": "Hello @{variables('name')}!" }
        });
        let stmt = try_decode_with_bindings(&action, &["msg", "name"]).unwrap();
        assert_eq!(
            format_native_stmt(&stmt, 0),
            "msg &= (\"Hello \" & name & \"!\")"
        );
    }

    #[test]
    fn native_compose_with_expression_input_lowers_to_let() {
        // `let total = (a + b)` recovered from a Compose with an `@add`
        // expression input.
        let action = json!({
            "type": "Compose",
            "inputs": "@add(variables('a'), variables('b'))"
        });
        let bindings: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();

        let iters = HashMap::new();

        let ctx = paexpr::RenderCtx::new(&bindings, &iters);
        let stmt = try_decode_native(
            "Compose_total",
            "Compose",
            action.as_object().unwrap(),
            &ctx,
        )
        .unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "let total = (a + b)");
    }

    #[test]
    fn decode_initialize_with_expression_round_trips_through_full_pipeline() {
        // End-to-end: x is declared with a literal initializer (so it's in
        // native_bindings by the time y's action is processed), then y is
        // declared with `@variables('x')` as its initializer.
        let input = json!({
            "properties": {
                "displayName": "Expr Round-Trip",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_x": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": {
                                "variables": [{ "name": "x", "type": "Integer", "value": 5 }]
                            }
                        },
                        "Initialize_y": {
                            "type": "InitializeVariable",
                            "runAfter": { "Initialize_x": ["Succeeded"] },
                            "inputs": {
                                "variables": [{ "name": "y", "type": "Integer", "value": "@variables('x')" }]
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("init_expr");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("var x: int = 5"), "pax was: {pax}");
        assert!(pax.contains("var y: int = x"), "pax was: {pax}");
        // No fallback file should have been written for either action.
        assert!(!dir.join("pa/Initialize_x.json").exists());
        assert!(!dir.join("pa/Initialize_y.json").exists());
    }

    #[test]
    fn decode_compose_with_outputs_member_access_lowers_natively() {
        // Compose_a := obj      (declares pax let `a`)
        // Compose_b := outputs('Compose_a')?['name']   →  let b = a.name
        let input = json!({
            "properties": {
                "displayName": "Compose Member",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Compose_a": {
                            "type": "Compose",
                            "runAfter": {},
                            "inputs": "obj"
                        },
                        "Compose_b": {
                            "type": "Compose",
                            "runAfter": { "Compose_a": ["Succeeded"] },
                            "inputs": "@outputs('Compose_a')?['name']"
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("compose_member");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("let a = \"obj\""), "pax was: {pax}");
        assert!(pax.contains("let b = a.name"), "pax was: {pax}");
    }

    #[test]
    fn decode_initialize_with_pa_accessor_lowers_natively() {
        // Slice 45a: triggerOutputs() now has a native pax form (call form),
        // so the InitializeVariable lowers without needing a pa fallback.
        let input = json!({
            "properties": {
                "displayName": "Trigger Accessor",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_data": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": {
                                "variables": [{
                                    "name": "data",
                                    "type": "Object",
                                    "value": "@triggerOutputs()"
                                }]
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("trigger_accessor");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("var data: object = triggerOutputs()"),
            "pax was: {pax}"
        );
        assert!(!dir.join("pa/Initialize_data.json").exists());
    }

    // ---------- 44d: container decoding ----------

    #[test]
    fn decode_anonymous_scope_lowers_natively() {
        let input = json!({
            "properties": {
                "displayName": "Scope Native",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope": {
                            "type": "Scope",
                            "runAfter": {},
                            "actions": {
                                "Inner": {
                                    "type": "OpenApiConnection",
                                    "runAfter": {},
                                    "inputs": {}
                                }
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("scope_native");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("scope {\n  pa Inner\n}"),
            "expected nested anonymous scope; pax was: {pax}"
        );
        // The Scope itself was natively lowered, so no pa/Scope.json file.
        assert!(!dir.join("pa/Scope.json").exists());
        // Inner action fell back, so it gets a pa/ file.
        assert!(dir.join("pa/Inner.json").exists());
    }

    #[test]
    fn decode_named_scope_recovers_label() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope_try_work": {
                            "type": "Scope",
                            "runAfter": {},
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("scope_named");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("scope try_work {"), "pax was: {pax}");
    }

    #[test]
    fn decode_if_with_object_condition_natively_lowers() {
        // PA designer's exported If condition is the structured-object form:
        //   {"and": [{"equals": ["@variables('approved')", true]}]}
        // The decoder routes it through condition_value_to_pax which builds
        // a `PaExpr::Call` tree, then renders to pax source. Single-arg
        // `and` reads `and((expr))`; it compiles via the generic-call path.
        let input = json!({
            "properties": {
                "displayName": "If Object",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_approved": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{
                                "name": "approved",
                                "type": "Boolean",
                                "value": false
                            }]}
                        },
                        "If_check": {
                            "type": "If",
                            "runAfter": { "Initialize_approved": ["Succeeded"] },
                            "expression": {
                                "and": [{ "equals": ["@variables('approved')", true] }]
                            },
                            "actions": {},
                            "else": { "actions": {} }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("if_object");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("var approved: bool = false"), "pax was: {pax}");
        assert!(
            pax.contains("if and((approved == true)) {"),
            "expected natively-decoded If header; pax was: {pax}"
        );
        // No pa/If_check.json because the If decoded natively.
        assert!(!dir.join("pa/If_check.json").exists());
    }

    #[test]
    fn decode_if_with_connector_body_condition_lowers_natively() {
        // Slice 45a: body() + ?['body/field'] now both render. The If
        // container lowers natively; only the inner OpenApiConnection
        // action falls back to a pa block (which is what we want -- the
        // connector body stays opaque, the control flow becomes source).
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "If_complex": {
                            "type": "If",
                            "runAfter": {},
                            "expression": {
                                "equals": ["@body('Get_response')?['body/field']", "x"]
                            },
                            "actions": {
                                "Inner": { "type": "OpenApiConnection", "runAfter": {}, "inputs": {} }
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("if_fallback");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("if (body(\"Get_response\")?[\"body/field\"] == \"x\")"),
            "pax was: {pax}"
        );
        assert!(pax.contains("pa Inner"), "pax was: {pax}");
        assert!(!dir.join("pa/If_complex.json").exists());
        assert!(dir.join("pa/Inner.json").exists());
    }

    #[test]
    fn decode_foreach_with_compose_collection_translates_items() {
        // Compose_things lets us refer to its outputs natively. A foreach
        // over `outputs('Compose_things')` decodes to `foreach iter in things`,
        // and `items('For_each')?['name']` inside the body lowers to `For_each.name`
        // because the iterator name = action key by default.
        let input = json!({
            "properties": {
                "displayName": "Foreach Native",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Compose_things": {
                            "type": "Compose",
                            "runAfter": {},
                            "inputs": "list_placeholder"
                        },
                        "For_each": {
                            "type": "Foreach",
                            "runAfter": { "Compose_things": ["Succeeded"] },
                            "foreach": "@outputs('Compose_things')",
                            "actions": {
                                "Inner": {
                                    "type": "OpenApiConnection",
                                    "runAfter": {},
                                    "inputs": {
                                        "value": "@items('For_each')?['name']"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("foreach_native");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("foreach For_each in things {"),
            "expected natively-decoded foreach; pax was: {pax}"
        );
        // Inner action falls back (OpenApiConnection isn't natively lowered),
        // but its pa/ file's `items('For_each')` reference will be preserved
        // on round-trip because the inner body is byte-equal to original.
        assert!(dir.join("pa/Inner.json").exists());
    }

    #[test]
    fn decode_until_with_pa_defaults_omits_max_and_timeout() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_done": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{
                                "name": "done",
                                "type": "Boolean",
                                "value": false
                            }]}
                        },
                        "Until_done": {
                            "type": "Until",
                            "runAfter": { "Initialize_done": ["Succeeded"] },
                            "expression": "@equals(variables('done'), true)",
                            "limit": { "count": 60, "timeout": "PT1H" },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("until_defaults");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        // Default count/timeout decode to None → omitted in pax source.
        assert!(
            pax.contains("until (done == true) {"),
            "expected bare until without max/timeout; pax was: {pax}"
        );
        assert!(!pax.contains("max"), "max should be omitted for default 60");
        assert!(
            !pax.contains("timeout"),
            "timeout should be omitted for default PT1H"
        );
    }

    #[test]
    fn decode_until_with_custom_limits_renders_max_and_timeout() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_done": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{
                                "name": "done",
                                "type": "Boolean",
                                "value": false
                            }]}
                        },
                        "Until_done": {
                            "type": "Until",
                            "runAfter": { "Initialize_done": ["Succeeded"] },
                            "expression": "@equals(variables('done'), true)",
                            "limit": { "count": 5, "timeout": "PT30M" },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("until_custom");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("until (done == true) max 5 timeout \"PT30M\" {"),
            "pax was: {pax}"
        );
    }

    #[test]
    fn decode_switch_with_string_cases_lowers_natively() {
        let input = json!({
            "properties": {
                "displayName": "X",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_state": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{
                                "name": "state",
                                "type": "String",
                                "value": "ready"
                            }]}
                        },
                        "Switch_state": {
                            "type": "Switch",
                            "runAfter": { "Initialize_state": ["Succeeded"] },
                            "expression": "@variables('state')",
                            "cases": {
                                "Case": { "case": "ready", "actions": {} },
                                "Case_2": { "case": "done", "actions": {} }
                            },
                            "default": { "actions": {} }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("switch_native");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("switch state {"), "pax was: {pax}");
        assert!(pax.contains("case \"ready\" {"), "pax was: {pax}");
        assert!(pax.contains("case \"done\" {"), "pax was: {pax}");
        assert!(pax.contains("default {"), "pax was: {pax}");
    }

    #[test]
    fn decode_container_renames_iter_when_action_key_collides_with_pax_keyword() {
        // PA action keys can be anything; if a Foreach happened to be keyed
        // `if` (a pax reserved word), we'd need to pick a different iter
        // name. Defensive: foreach_iter_name suffixes _2 to dodge keywords.
        let bindings = HashSet::new();
        let iters = HashMap::new();
        let name = foreach_iter_name("if", &bindings, &iters);
        assert_eq!(name, "if_2");
    }

    #[test]
    fn decode_container_renames_iter_to_dodge_outer_binding() {
        let mut bindings = HashSet::new();
        bindings.insert("For_each".to_string());
        let iters = HashMap::new();
        let name = foreach_iter_name("For_each", &bindings, &iters);
        assert_eq!(name, "For_each_2");
    }

    #[test]
    fn decode_container_renames_iter_normalizes_invalid_action_key() {
        // PA action keys with parens / spaces normalize on the way in. The
        // iter name takes the normalized form.
        let bindings = HashSet::new();
        let iters = HashMap::new();
        let name = foreach_iter_name("Apply to each (1)", &bindings, &iters);
        assert_eq!(name, "Apply_to_each_1");
    }

    #[test]
    fn decode_scope_label_recognizes_paxc_auto_suffix() {
        // `Scope_2` is paxc's auto-uniquify shape; render as anonymous so
        // the source reads `scope { ... }` without a useless numeric label.
        assert_eq!(scope_label("Scope_2"), None);
        assert_eq!(scope_label("Scope_3"), None);
        // A real label survives.
        assert_eq!(scope_label("Scope_try_work"), Some("try_work".to_string()));
        // Bare `Scope` has no underscore → None.
        assert_eq!(scope_label("Scope"), None);
        // Non-pax-identifier labels fall back to anonymous.
        assert_eq!(scope_label("Scope_my-label"), None);
    }

    // ---------- 44e: Terminate ----------

    #[test]
    fn decode_terminate_succeeded_no_message() {
        let action = json!({
            "type": "Terminate",
            "inputs": { "runStatus": "Succeeded" }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "terminate succeeded");
    }

    #[test]
    fn decode_terminate_failed_with_string_message() {
        let action = json!({
            "type": "Terminate",
            "inputs": {
                "runStatus": "Failed",
                "runError": { "message": "boom" }
            }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "terminate failed \"boom\"");
    }

    #[test]
    fn decode_terminate_failed_with_expression_message() {
        let action = json!({
            "type": "Terminate",
            "inputs": {
                "runStatus": "Failed",
                "runError": { "message": "@variables('reason')" }
            }
        });
        let stmt = try_decode_with_bindings(&action, &["reason"]).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "terminate failed reason");
    }

    #[test]
    fn decode_terminate_cancelled_drops_runerror() {
        // PA's contract is that Cancelled doesn't carry a runError; if one
        // appears we ignore it and emit `terminate cancelled`.
        let action = json!({
            "type": "Terminate",
            "inputs": {
                "runStatus": "Cancelled",
                "runError": { "message": "ignored" }
            }
        });
        let stmt = try_decode(&action).unwrap();
        assert_eq!(format_native_stmt(&stmt, 0), "terminate cancelled");
    }

    #[test]
    fn decode_terminate_unknown_status_falls_back() {
        let action = json!({
            "type": "Terminate",
            "inputs": { "runStatus": "WeirdStatus" }
        });
        assert!(try_decode(&action).is_none());
    }

    #[test]
    fn decode_terminate_failed_with_pa_accessor_message_lowers_natively() {
        // Slice 45a: triggerBody() now renders, so a `terminate failed`
        // whose message references it lowers natively to pax source.
        let action = json!({
            "type": "Terminate",
            "inputs": {
                "runStatus": "Failed",
                "runError": { "message": "@triggerBody()" }
            }
        });
        let stmt = try_decode(&action).expect("should lower natively");
        assert_eq!(
            format_native_stmt(&stmt, 0),
            "terminate failed triggerBody()"
        );
    }

    // ---------- 44e: on-handler ----------

    #[test]
    fn decode_on_handler_targeting_named_scope() {
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope_try_work": {
                            "type": "Scope",
                            "runAfter": {},
                            "actions": {}
                        },
                        "On_failure": {
                            "type": "Scope",
                            "runAfter": { "Scope_try_work": ["Failed"] },
                            "actions": {
                                "Terminate": {
                                    "type": "Terminate",
                                    "runAfter": {},
                                    "inputs": { "runStatus": "Failed", "runError": { "message": "x" } }
                                }
                            }
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_handler_scope");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("scope try_work {"), "pax was: {pax}");
        assert!(pax.contains("on failed try_work {"), "pax was: {pax}");
        assert!(pax.contains("terminate failed \"x\""), "pax was: {pax}");
        assert!(!dir.join("pa/On_failure.json").exists());
    }

    #[test]
    fn decode_on_handler_multi_status() {
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope_work": { "type": "Scope", "runAfter": {}, "actions": {} },
                        "Bail": {
                            "type": "Scope",
                            "runAfter": { "Scope_work": ["Failed", "TimedOut", "Skipped"] },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_multistatus");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(
            pax.contains("on failed or timedout or skipped work {"),
            "pax was: {pax}"
        );
    }

    #[test]
    fn decode_on_handler_targeting_pa_block() {
        // The target is a non-natively-decoded action (OpenApiConnection),
        // which becomes a `pa Foo` block. The handler targets that pa block
        // by its (normalized) pax name.
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "HTTP_call": {
                            "type": "OpenApiConnection",
                            "runAfter": {},
                            "inputs": {}
                        },
                        "On_call_failed": {
                            "type": "Scope",
                            "runAfter": { "HTTP_call": ["Failed"] },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_pa_block");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("pa HTTP_call"), "pax was: {pax}");
        assert!(pax.contains("on failed HTTP_call {"), "pax was: {pax}");
    }

    #[test]
    fn decode_on_handler_with_unaddressable_target_falls_back() {
        // Target is a variable lifecycle action which has no pax-side
        // identifier accessible from outside. Handler falls back as a pa
        // block (and a warning surfaces).
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Initialize_x": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": { "variables": [{ "name": "x", "type": "Integer", "value": 0 }] }
                        },
                        "On_init_failed": {
                            "type": "Scope",
                            "runAfter": { "Initialize_x": ["Failed"] },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_unaddressable");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("var x: int = 0"), "pax was: {pax}");
        assert!(pax.contains("pa On_init_failed"), "pax was: {pax}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("On_init_failed") && w.contains("handler-style")),
            "warnings: {:?}",
            report.warnings
        );
    }

    #[test]
    fn decode_on_handler_anonymous_scope_target_falls_back() {
        // Anonymous scopes have no source-level identifier, so a handler
        // targeting one can't be expressed natively. Falls back.
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope": { "type": "Scope", "runAfter": {}, "actions": {} },
                        "On_anon_failed": {
                            "type": "Scope",
                            "runAfter": { "Scope": ["Failed"] },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_anon");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("scope {"), "pax was: {pax}");
        assert!(pax.contains("pa On_anon_failed"), "pax was: {pax}");
    }

    #[test]
    fn decode_on_handler_multi_target_runafter_falls_back() {
        // PA could have a Scope with runAfter pointing at multiple targets;
        // pax `on` addresses one. Falls back.
        let input = json!({
            "properties": {
                "displayName": "Handler",
                "definition": {
                    "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                    "actions": {
                        "Scope_a": { "type": "Scope", "runAfter": {}, "actions": {} },
                        "Scope_b": { "type": "Scope", "runAfter": {}, "actions": {} },
                        "On_either_failed": {
                            "type": "Scope",
                            "runAfter": { "Scope_a": ["Failed"], "Scope_b": ["Failed"] },
                            "actions": {}
                        }
                    }
                }
            }
        });
        let dir = tmp_dir("on_multitarget");
        let report = decode(&input, "x", &dir).unwrap();
        let pax = read(&report.pax_path);
        assert!(pax.contains("pa On_either_failed"), "pax was: {pax}");
    }

    // ---------- 45a polish: end-to-end zip input ----------

    /// Build a minimal PA legacy package zip in memory and return its path.
    /// `flows` is a list of `(flow_id, definition_json_bytes)` to embed —
    /// pass one entry for the happy path, two for the multi-flow case, zero
    /// for the no-flow case.
    fn write_test_zip(label: &str, flows: &[(&str, &[u8])]) -> PathBuf {
        use std::io::Write;
        let dir = tmp_dir(label);
        let zip_path = dir.join(format!("{label}.zip"));
        let f = fs::File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zw.start_file("manifest.json", opts).unwrap();
        zw.write_all(b"{}").unwrap();
        for (flow_id, def) in flows {
            zw.start_file(
                format!("Microsoft.Flow/flows/{flow_id}/definition.json"),
                opts,
            )
            .unwrap();
            zw.write_all(def).unwrap();
        }
        zw.finish().unwrap();
        zip_path
    }

    fn minimal_definition_json() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "properties": {
                "displayName": "Z",
                "definition": {
                    "triggers": {
                        "manual": { "type": "Request", "kind": "Button", "inputs": {} }
                    },
                    "actions": {
                        "Initialize_x": {
                            "type": "InitializeVariable",
                            "runAfter": {},
                            "inputs": {
                                "variables": [{ "name": "x", "type": "Integer", "value": 1 }]
                            }
                        }
                    }
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn decode_file_accepts_zip_input() {
        let zip_path = write_test_zip("zip_happy", &[("guid-1", &minimal_definition_json())]);
        let out = tmp_dir("zip_happy_out");
        let report = decode_file(&zip_path, &out).expect("decode failed");
        let pax = read(&report.pax_path);
        assert!(pax.contains("var x: int = 1"), "pax was: {pax}");
        assert!(report.pax_path.starts_with(&out));
    }

    #[test]
    fn decode_file_rejects_zip_with_no_flow() {
        let zip_path = write_test_zip("zip_empty", &[]);
        let out = tmp_dir("zip_empty_out");
        let err = decode_file(&zip_path, &out).expect_err("should fail");
        assert!(
            matches!(err, DecodeError::NoFlowInPackage { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn decode_file_rejects_zip_with_multiple_flows() {
        let def = minimal_definition_json();
        let zip_path = write_test_zip("zip_multi", &[("guid-a", &def), ("guid-b", &def)]);
        let out = tmp_dir("zip_multi_out");
        let err = decode_file(&zip_path, &out).expect_err("should fail");
        match err {
            DecodeError::MultipleFlowsInPackage { flows, .. } => {
                assert_eq!(flows.len(), 2);
                assert!(flows.contains(&"guid-a".to_string()));
                assert!(flows.contains(&"guid-b".to_string()));
            }
            other => panic!("wrong error: {other}"),
        }
    }
}
