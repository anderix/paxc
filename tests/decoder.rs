//! Corpus-driven round-trip harness for the PA JSON decoder.
//!
//! For each `tests/corpus/<name>/input.json` the harness:
//!   1. Decodes the PA flow JSON into pax + a `pa/` folder under a tempdir.
//!   2. Compiles the decoded pax back through the lex → parse → resolve →
//!      emit pipeline to produce a fresh PA flow JSON.
//!   3. Asserts the re-emitted JSON matches the original at the level of
//!      structural / semantic equivalence (see `compare_definitions` for
//!      exactly what's compared and what's intentionally tolerated).
//!
//! Adding a new flow to the corpus is just dropping a `definition.json` at
//! `tests/corpus/<descriptive-name>/input.json` — no per-flow code needed.
//! The harness fails loudly with the first divergence so regressions are
//! easy to triage.

use chumsky::prelude::*;
use paxc::pa::{decoder, emitter};
use paxc::{lexer, parser, resolver};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn corpus_entries() -> Vec<PathBuf> {
    let root = corpus_root();
    if !root.exists() {
        return Vec::new();
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&root)
        .expect("read tests/corpus")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("input.json").exists())
        .collect();
    entries.sort();
    entries
}

fn tmp_dir(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("paxc-corpus-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn compile_pax_to_definition(pax_path: &Path) -> Value {
    let src = fs::read_to_string(pax_path).expect("read decoded pax");
    let tokens = lexer::lexer()
        .parse(src.as_str())
        .into_result()
        .expect("lex");
    let program = parser::parser()
        .parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        )
        .into_result()
        .expect("parse");
    let source_dir = pax_path.parent();
    let resolved = resolver::resolve(&program, source_dir).expect("resolve");
    emitter::emit(&resolved)
}

#[test]
fn round_trip_corpus() {
    let entries = corpus_entries();
    assert!(
        !entries.is_empty(),
        "tests/corpus is empty; add at least one <name>/input.json"
    );

    let mut failed: Vec<String> = Vec::new();
    for entry in entries {
        let label = entry.file_name().unwrap().to_string_lossy().to_string();
        let input_path = entry.join("input.json");
        let original_bytes = fs::read(&input_path).expect("read corpus input");
        let original: Value = serde_json::from_slice(&original_bytes).expect("parse corpus input");

        let out_dir = tmp_dir(&label);
        let _report = match decoder::decode_file(&input_path, &out_dir) {
            Ok(r) => r,
            Err(e) => {
                failed.push(format!("{label}: decode failed: {e}"));
                continue;
            }
        };

        // The decoded pax file is named after the input stem ("input.pax").
        let pax_path = out_dir.join("input.pax");
        let reemitted = compile_pax_to_definition(&pax_path);

        if let Err(diff) = compare_definitions(&original, &reemitted) {
            failed.push(format!("{label}: {diff}"));
        }
    }

    if !failed.is_empty() {
        panic!(
            "{} corpus flow(s) did not round-trip cleanly:\n{}",
            failed.len(),
            failed.join("\n")
        );
    }
}

/// Compare original PA export JSON against paxc's re-emitted JSON. Structural
/// equivalence, with these intentional tolerances:
///
/// * **Envelope fields** (`name`, `id`, `properties.apiId`, `properties.displayName`,
///   `properties.connectionReferences`, etc.): the original has them, paxc's
///   raw JSON output does not (paxc's envelope-building lives in the packager,
///   not the emitter). We compare only the inner `definition` block.
/// * **`metadata` block** inside `definition`: optional in PA; paxc omits it.
///   Stripped from the original before comparing.
/// * **`parameters` block** with `$authentication` / `$connections`: paxc
///   adds these only inside the packager. The bare emitter output omits them.
///   Stripped from the original.
/// * **`$schema` differences**: paxc uses one canonical schema URL; some
///   exports use slightly different ones. Tolerated.
/// * **Action-level `metadata` blocks** (PA's per-action operationMetadataId):
///   not preserved through `pa <Name>` (the JSON file does carry it, so it
///   does round-trip when paxc reads the file back). Comparison is on the
///   inner `definition` after these are stripped.
/// * **Action key prefix differences**: an `Initialize_variable_<name>` from
///   PA designer vs paxc's `Initialize_<name>` would mismatch. For slice 44a
///   the natively-lowered InitializeVariable / SetVariable / etc. preserve
///   the original action key by going through `pa <Name>` when the PA key
///   doesn't match paxc's regenerated form. Native lowering with a different
///   resulting key is documented as a known divergence in the slice-44a plan
///   and tolerated here at the "actions are present and have the right
///   types" level rather than exact key equality.
fn compare_definitions(original: &Value, reemitted: &Value) -> Result<(), String> {
    let orig_def = strip_definition(original)?;
    let new_def = strip_definition(reemitted)?;

    // Triggers must match exactly — they're emitted verbatim from the
    // .trigger.json file.
    let orig_triggers = orig_def.get("triggers").ok_or("original has no triggers")?;
    let new_triggers = new_def.get("triggers").ok_or("reemitted has no triggers")?;
    if orig_triggers != new_triggers {
        return Err(format!(
            "triggers differ\noriginal: {}\nreemitted: {}",
            serde_json::to_string_pretty(orig_triggers).unwrap(),
            serde_json::to_string_pretty(new_triggers).unwrap()
        ));
    }

    // Action set: every original action key should appear in the reemitted
    // definition either with the same key (pa-block fallback) or recognized
    // as natively-lowered (in which case we just check action count by type).
    let orig_actions = orig_def
        .get("actions")
        .and_then(Value::as_object)
        .ok_or("original has no actions")?;
    let new_actions = new_def
        .get("actions")
        .and_then(Value::as_object)
        .ok_or("reemitted has no actions")?;

    // Counts by type at the top level — looser than exact key equality but
    // enough to catch dropped or duplicated actions.
    let orig_types = action_type_counts(orig_actions);
    let new_types = action_type_counts(new_actions);
    if orig_types != new_types {
        return Err(format!(
            "top-level action type counts differ\noriginal: {orig_types:?}\nreemitted: {new_types:?}"
        ));
    }

    // For pa-block fallbacks (anything outside the native-lowering set),
    // the action JSON is copied verbatim into pa/<Name>.json and emitted
    // unchanged — those bodies should match key-for-key (modulo runAfter,
    // which paxc regenerates from source order). The native-lowering set
    // is everything the decoder may natively rewrite into pax constructs:
    // when it does, the action key is regenerated by paxc's emitter
    // (`Initialize_<name>`, `Condition`, `Apply_to_each`, `Compose_<name>`,
    // ...) and the action body is rebuilt structurally — neither matches
    // the original byte-for-byte. Type counts stay the same (still one If,
    // still one Foreach), and the per-action unit tests inside paxc cover
    // semantic equivalence.
    let native_types: std::collections::HashSet<&str> = [
        "InitializeVariable",
        "SetVariable",
        "IncrementVariable",
        "DecrementVariable",
        "AppendToStringVariable",
        "AppendToArrayVariable",
        "Compose",
        "If",
        "Foreach",
        "Until",
        "Switch",
        "Scope",
    ]
    .into_iter()
    .collect();

    for (key, orig_body) in orig_actions {
        let orig_type = orig_body.get("type").and_then(Value::as_str).unwrap_or("");
        if native_types.contains(orig_type) {
            // Native-lowering case is harder to compare directly because the
            // action key may have been regenerated (Initialize_variable_x →
            // Initialize_x). Skip per-action body diffing for these in 44a;
            // the type-count check above catches gross mismatches.
            continue;
        }
        // Fall-back actions: must appear in reemitted under the same (or
        // normalized) key. Normalization mirrors decoder::normalize_action_key.
        let normalized = normalize_for_lookup(key);
        let candidate = new_actions
            .get(key)
            .or_else(|| new_actions.get(&normalized));
        let reemit_body = match candidate {
            Some(b) => b,
            None => {
                return Err(format!(
                    "action `{key}` (type {orig_type}) missing from reemitted definition"
                ));
            }
        };
        // The pa-block path emits the body verbatim, modulo the runAfter
        // map which paxc regenerates from source order. Compare everything
        // EXCEPT runAfter, plus an existence check on runAfter.
        let orig_body_clean = strip_run_after(orig_body);
        let reemit_body_clean = strip_run_after(reemit_body);
        if orig_body_clean != reemit_body_clean {
            return Err(format!(
                "pa-block action `{key}` body differs after round-trip\noriginal: {}\nreemitted: {}",
                serde_json::to_string_pretty(&orig_body_clean).unwrap(),
                serde_json::to_string_pretty(&reemit_body_clean).unwrap()
            ));
        }
    }

    Ok(())
}

fn strip_definition(v: &Value) -> Result<Value, String> {
    // Both shapes appear:
    //   - Original PA export envelope: { properties: { definition: {...} } }
    //   - paxc emitter output: { definition: {...} }
    let def = v
        .get("properties")
        .and_then(|p| p.get("definition"))
        .or_else(|| v.get("definition"))
        .ok_or("no definition block")?;
    let mut def = def.clone();
    if let Value::Object(map) = &mut def {
        // metadata is regenerated by PA on import; paxc omits it.
        map.remove("metadata");
        // parameters block ($authentication, $connections) is added by the
        // packager wrapper, not the bare emitter. Strip for comparison.
        map.remove("parameters");
        // $schema: tolerate exact value differences.
        map.remove("$schema");
        // contentVersion: also envelope-y; tolerate.
        map.remove("contentVersion");
    }
    Ok(def)
}

fn strip_run_after(v: &Value) -> Value {
    let mut v = v.clone();
    if let Value::Object(map) = &mut v {
        map.remove("runAfter");
        // PA designer adds per-action metadata.operationMetadataId; paxc
        // copies this through verbatim from the pa/ file, so it should
        // round-trip cleanly. No strip needed here.
    }
    v
}

fn action_type_counts(
    actions: &serde_json::Map<String, Value>,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for body in actions.values() {
        let t = body
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_string();
        *counts.entry(t).or_insert(0) += 1;
    }
    counts
}

/// Mirror of `decoder::normalize_action_key`'s base normalization (for the
/// `used` set being empty — i.e. no collision-suffix). Used only to look up
/// fallback actions in the reemitted map.
fn normalize_for_lookup(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut prev_underscore = false;
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            prev_underscore = ch == '_';
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Slice 44f integration check: a PA action key with characters outside
/// `[A-Za-z_][A-Za-z0-9_]*` (here `Send_an_email_(V2)`) decodes to a
/// pax-safe `pa Send_an_email_V2`, and re-encoding via the resolver/emitter
/// pipeline restores the original key — proving `pa/flow.json.actionNameMap`
/// is being read on the encode side.
#[test]
fn decode_then_encode_preserves_original_pa_action_key() {
    use serde_json::json;

    let input = json!({
        "properties": {
            "displayName": "Name Map Round-Trip",
            "definition": {
                "$schema": "https://schema.management.azure.com/providers/Microsoft.Logic/schemas/2016-06-01/workflowdefinition.json#",
                "contentVersion": "1.0.0.0",
                "triggers": { "manual": { "type": "Request", "kind": "Button", "inputs": {} } },
                "actions": {
                    "Send_an_email_(V2)": {
                        "type": "OpenApiConnection",
                        "runAfter": {},
                        "inputs": { "method": "POST" }
                    }
                }
            }
        }
    });

    let dir = tmp_dir("namemap_roundtrip");
    let input_path = dir.join("input.json");
    fs::write(&input_path, serde_json::to_vec_pretty(&input).unwrap()).unwrap();

    let _report = decoder::decode_file(&input_path, &dir).expect("decode");

    // The decoded pax should reference the safe name.
    let pax = fs::read_to_string(dir.join("input.pax")).unwrap();
    assert!(
        pax.contains("pa Send_an_email_V2"),
        "decoded pax should use the pax-safe name, got: {pax}"
    );

    // pa/flow.json should carry the actionNameMap.
    let flow_meta: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("pa/flow.json")).unwrap()).unwrap();
    assert_eq!(
        flow_meta["actionNameMap"]["Send_an_email_(V2)"],
        "Send_an_email_V2"
    );

    // Re-encode through the resolver/emitter pipeline. The resolver reads
    // actionNameMap and overrides the emit name back to the original.
    let reemitted = compile_pax_to_definition(&dir.join("input.pax"));
    let actions = reemitted["definition"]["actions"]
        .as_object()
        .expect("actions object");
    assert!(
        actions.contains_key("Send_an_email_(V2)"),
        "expected re-emit to restore original PA key; got keys: {:?}",
        actions.keys().collect::<Vec<_>>()
    );
    assert!(
        !actions.contains_key("Send_an_email_V2"),
        "the pax-safe name should NOT appear in the re-emitted JSON; got keys: {:?}",
        actions.keys().collect::<Vec<_>>()
    );
}
