//! Decoder: turns an exported Power Automate flow JSON into pax source plus
//! a `pa/` folder of opaque action bodies. Inverse of `pa::emitter`.
//!
//! Slice 44a covered the skeleton plus the variable lifecycle action types
//! (InitializeVariable / SetVariable / IncrementVariable / DecrementVariable
//! / AppendToStringVariable / AppendToArrayVariable). Slice 44b adds Compose
//! → `let` lowering when the action key has shape `Compose_<identifier>`.
//! Every other action type still falls back to `pa <Name>` plus a JSON body
//! file, with a stderr-bound warning so the user knows what didn't decode
//! natively. Future sub-slices (44c–44f) extend coverage to PA expression
//! translation, container actions, on-handlers, and terminate.
//!
//! The decoder is intentionally lossless-leaning: anything we can't faithfully
//! represent in pax stays as a `pa <Name>` block whose body is the original
//! action JSON byte-for-byte. Re-encoding through `paxc --target pa-legacy`
//! reproduces the action verbatim.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
        }
    }
}

impl std::error::Error for DecodeError {}

/// Top-level entry: read a PA flow JSON from `input_path` and write the
/// decoded pax source + `pa/` folder under `out_dir`. Output basename is
/// derived from the input file stem.
pub fn decode_file(input_path: &Path, out_dir: &Path) -> Result<DecodeReport, DecodeError> {
    let bytes = fs::read(input_path).map_err(|e| DecodeError::Io {
        path: input_path.to_path_buf(),
        source: e,
    })?;
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
    let empty_actions = Map::new();
    let actions = definition
        .get("actions")
        .and_then(Value::as_object)
        .unwrap_or(&empty_actions);
    let order = topo_sort_actions(actions)?;

    let mut name_map: HashMap<String, String> = HashMap::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut pax_lines: Vec<String> = Vec::new();
    // Pax bindings (vars + lets) declared natively so far. Subsequent
    // assigns can only natively-lower if their target is in this set —
    // otherwise the emitted pax has an undeclared reference and won't
    // compile. New lets/vars also collide if the name is already here
    // (pax shares one namespace across vars and lets).
    let mut native_bindings: HashSet<String> = HashSet::new();

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
        if handler_edge {
            report.warnings.push(format!(
                "note: action `{original_key}` has handler-style runAfter; falling back to pa <Name> (slice 44a does not yet decode on-handlers)"
            ));
        }

        let native = if handler_edge {
            None
        } else {
            try_decode_native(original_key, &action_type, action_obj)
                .filter(|stmt| native_target_available(stmt, &native_bindings))
        };

        if let Some(stmt) = native {
            // Track new bindings so downstream actions can resolve them.
            match &stmt {
                NativeStmt::VarInit { name, .. } | NativeStmt::Let { name, .. } => {
                    native_bindings.insert(name.clone());
                }
                NativeStmt::Assign { .. } => {}
            }
            pax_lines.push(format_native_stmt(&stmt));
            // Natively-lowered actions don't need pa/ files.
        } else {
            // Fall back to `pa <Name>`. Normalize the key, write the body
            // file verbatim, emit the pa statement.
            let normalized = normalize_action_key(original_key, &mut used_names);
            if normalized != *original_key {
                name_map.insert(original_key.clone(), normalized.clone());
            }
            let path = pa_dir.join(format!("{normalized}.json"));
            write_json(&path, action)?;
            report.pa_files_written.push(path);
            if !handler_edge {
                report.warnings.push(format!(
                    "note: action `{original_key}` (type {}) not yet decoded natively; emitted as pa block",
                    if action_type.is_empty() {
                        "<unknown>"
                    } else {
                        action_type.as_str()
                    }
                ));
            }
            pax_lines.push(format!("pa {normalized}"));
        }
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
/// is the caller's escape hatch when `try_decode_native` returns None.
#[derive(Debug, Clone)]
enum NativeStmt {
    VarInit {
        name: String,
        ty: PaxType,
        value: Option<PaxLiteral>,
    },
    Assign {
        name: String,
        op: &'static str,
        value: PaxLiteral,
    },
    /// A pax `let` binding recovered from a `Compose` action. `name` is the
    /// pax-side identifier (the part after `Compose_` in the PA action key).
    /// The action key on re-encode will be `Compose_<name>` again, so this
    /// only fires when the original PA key has the `Compose_<id>` shape with
    /// `<id>` being a valid pax identifier.
    Let { name: String, value: PaxLiteral },
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

#[derive(Debug, Clone)]
enum PaxLiteral {
    Null,
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    EmptyArray,
    EmptyObject,
}

impl PaxLiteral {
    /// Try to read a JSON value as a pax literal. Returns None for anything
    /// pax can't render in source today: PA-expression strings (`@...`),
    /// non-empty arrays/objects, integers outside i64 range.
    fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Null => Some(PaxLiteral::Null),
            Value::Bool(b) => Some(PaxLiteral::Bool(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(PaxLiteral::Int(i))
                } else {
                    n.as_f64().map(PaxLiteral::Float)
                }
            }
            Value::String(s) => {
                if s.starts_with('@') {
                    None
                } else {
                    Some(PaxLiteral::String(s.clone()))
                }
            }
            Value::Array(items) => {
                if items.is_empty() {
                    Some(PaxLiteral::EmptyArray)
                } else {
                    None
                }
            }
            Value::Object(map) => {
                if map.is_empty() {
                    Some(PaxLiteral::EmptyObject)
                } else {
                    None
                }
            }
        }
    }

    fn render(&self) -> String {
        match self {
            PaxLiteral::Null => "null".to_string(),
            PaxLiteral::Int(n) => n.to_string(),
            PaxLiteral::Float(x) => format_float_for_pax(*x),
            PaxLiteral::String(s) => format!("\"{}\"", escape_pax_string(s)),
            PaxLiteral::Bool(true) => "true".to_string(),
            PaxLiteral::Bool(false) => "false".to_string(),
            PaxLiteral::EmptyArray => "[]".to_string(),
            PaxLiteral::EmptyObject => "{}".to_string(),
        }
    }
}

/// Pax source-form float — always at least one fractional digit so the
/// lexer parses it as a Float, not an Int. Mirrors `pa::emitter::format_float`.
fn format_float_for_pax(x: f64) -> String {
    if x.is_finite() && x == x.trunc() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn escape_pax_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Returns Some(stmt) for action types we know how to natively lower. None
/// forces fallback to `pa <Name>`. The `action_key` is needed for Compose
/// because the let-binding name comes from the key suffix, not the inputs.
fn try_decode_native(
    action_key: &str,
    action_type: &str,
    action: &Map<String, Value>,
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
                Some(v) => Some(PaxLiteral::from_json(v)?),
                None => None,
            };
            Some(NativeStmt::VarInit { name, ty, value })
        }
        "SetVariable" => decode_simple_assign(action, "="),
        "IncrementVariable" => decode_numeric_assign(action, "+="),
        "DecrementVariable" => decode_numeric_assign(action, "-="),
        "AppendToStringVariable" => {
            let inputs = action.get("inputs")?.as_object()?;
            let name = inputs.get("name")?.as_str()?.to_string();
            let raw = inputs.get("value")?;
            // Append-to-string only natively supports literal string values.
            // `42` (int) or `@expr` falls back.
            let lit = match raw {
                Value::String(s) if !s.starts_with('@') => PaxLiteral::String(s.clone()),
                _ => return None,
            };
            Some(NativeStmt::Assign {
                name,
                op: "&=",
                value: lit,
            })
        }
        "AppendToArrayVariable" => decode_simple_assign(action, "+="),
        "Compose" => {
            // The pax name lives in the action key suffix: `Compose_<id>`.
            // A bare `Compose` (PA designer's default for the first one) has
            // no recoverable pax name and falls back. A suffix that isn't a
            // valid pax identifier also falls back.
            let name = compose_let_name(action_key)?;
            let inputs = action.get("inputs")?;
            let value = PaxLiteral::from_json(inputs)?;
            Some(NativeStmt::Let { name, value })
        }
        _ => None,
    }
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
    }
}

fn decode_simple_assign(action: &Map<String, Value>, op: &'static str) -> Option<NativeStmt> {
    let inputs = action.get("inputs")?.as_object()?;
    let name = inputs.get("name")?.as_str()?.to_string();
    let value = PaxLiteral::from_json(inputs.get("value")?)?;
    Some(NativeStmt::Assign { name, op, value })
}

fn decode_numeric_assign(action: &Map<String, Value>, op: &'static str) -> Option<NativeStmt> {
    let inputs = action.get("inputs")?.as_object()?;
    let name = inputs.get("name")?.as_str()?.to_string();
    let value = match PaxLiteral::from_json(inputs.get("value")?)? {
        v @ (PaxLiteral::Int(_) | PaxLiteral::Float(_)) => v,
        _ => return None,
    };
    Some(NativeStmt::Assign { name, op, value })
}

fn format_native_stmt(stmt: &NativeStmt) -> String {
    match stmt {
        NativeStmt::VarInit { name, ty, value } => match value {
            Some(v) => format!("var {name}: {} = {}", ty.keyword(), v.render()),
            None => format!("var {name}: {}", ty.keyword()),
        },
        NativeStmt::Assign { name, op, value } => format!("{name} {op} {}", value.render()),
        NativeStmt::Let { name, value } => format!("let {name} = {}", value.render()),
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
    fn try_decode(action: &Value) -> Option<NativeStmt> {
        try_decode_native(
            "TestKey",
            action["type"].as_str().unwrap(),
            action.as_object().unwrap(),
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
        assert_eq!(format_native_stmt(&stmt), "var counter: int = 5");
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
        assert_eq!(format_native_stmt(&stmt), "var todo: string");
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
        assert_eq!(format_native_stmt(&stmt), "var todo: string");
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
            assert_eq!(format_native_stmt(&stmt), expected);
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

    #[test]
    fn native_compose_with_string_literal_lowers_to_let() {
        let action = json!({"type": "Compose", "inputs": "hello"});
        let stmt =
            try_decode_native("Compose_greeting", "Compose", action.as_object().unwrap()).unwrap();
        assert_eq!(format_native_stmt(&stmt), "let greeting = \"hello\"");
    }

    #[test]
    fn native_compose_with_int_literal_lowers_to_let() {
        let action = json!({"type": "Compose", "inputs": 42});
        let stmt =
            try_decode_native("Compose_answer", "Compose", action.as_object().unwrap()).unwrap();
        assert_eq!(format_native_stmt(&stmt), "let answer = 42");
    }

    #[test]
    fn native_compose_falls_back_on_pa_expression_input() {
        let action = json!({"type": "Compose", "inputs": "@triggerBody()"});
        assert!(try_decode_native("Compose_x", "Compose", action.as_object().unwrap()).is_none());
    }

    #[test]
    fn native_compose_falls_back_on_bare_compose_key() {
        // Even a literal-input Compose must fall back when the key has no
        // recoverable pax name suffix.
        let action = json!({"type": "Compose", "inputs": "ok"});
        assert!(try_decode_native("Compose", "Compose", action.as_object().unwrap()).is_none());
    }

    #[test]
    fn native_compose_falls_back_on_non_literal_input() {
        // Non-empty arrays/objects can't be rendered in pax source.
        let action = json!({"type": "Compose", "inputs": [1, 2, 3]});
        assert!(try_decode_native("Compose_a", "Compose", action.as_object().unwrap()).is_none());
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
    fn decode_assign_falls_back_when_init_did() {
        // Real corpus pattern: Initialize_variable_x has a PA-expression
        // initializer (so its `var x: T = ...` can't be expressed yet and
        // falls back), but Decrement_variable for the same x is just `1`.
        // If we natively emitted `x -= 1`, the pax wouldn't compile because
        // x was never declared. The decoder must keep the assign as a pa
        // block alongside the init.
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
            pax.contains("pa Decrement_variable"),
            "expected Decrement_variable to fall back; got: {pax}"
        );
        assert!(
            !pax.contains("ridx -= 1"),
            "expected ridx to NOT be natively assigned (no pax declaration exists); got: {pax}"
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
        // Compose isn't natively decoded yet either, so both fall back. The
        // important behavior here is that On_failure surfaces the
        // handler-edge warning specifically.
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("On_failure") && w.contains("handler-style")),
            "expected handler warning, got: {:?}",
            report.warnings
        );
    }
}
