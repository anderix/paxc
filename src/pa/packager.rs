//! Wraps paxc's compiled output in deployment-format artifacts.
//!
//! Currently supports the Power Automate "Import Package (Legacy)" format:
//! a flat zip with `manifest.json` at the root and the flow assets under
//! `Microsoft.Flow/flows/<package-guid>/`. The paxc emitter's JSON is the
//! raw Logic Apps workflow definition; the package wraps it in PA-specific
//! envelopes (flow resource, display name, connection references) and adds
//! the minor quirks PA expects (`$authentication` / `$connections`
//! parameters, schema wrapper on the manual trigger's `inputs`, lowercase
//! variable type names).
//!
//! The envelope shape was determined by round-tripping a real minimal flow
//! export from a tenant, not guessed -- see `examples/tour.pax` and the
//! packager tests for the artifact-matching invariants.

use crate::pa::emitter;
use crate::resolver::ResolvedProgram;
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use uuid::Uuid;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

/// Output targets paxc can produce beyond the raw JSON.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// Power Automate "Import Package (Legacy)" zip.
    PaLegacy,
}

#[derive(Debug)]
pub enum PackageError {
    Io(io::Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::Io(e) => write!(f, "io: {e}"),
            PackageError::Zip(e) => write!(f, "zip: {e}"),
            PackageError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<io::Error> for PackageError {
    fn from(e: io::Error) -> Self {
        PackageError::Io(e)
    }
}
impl From<zip::result::ZipError> for PackageError {
    fn from(e: zip::result::ZipError) -> Self {
        PackageError::Zip(e)
    }
}
impl From<serde_json::Error> for PackageError {
    fn from(e: serde_json::Error) -> Self {
        PackageError::Json(e)
    }
}

pub fn package(
    program: &ResolvedProgram,
    target: Target,
    name: &str,
    out_path: &Path,
) -> Result<(), PackageError> {
    match target {
        Target::PaLegacy => package_pa_legacy(program, name, out_path),
    }
}

fn package_pa_legacy(
    program: &ResolvedProgram,
    name: &str,
    out_path: &Path,
) -> Result<(), PackageError> {
    // Compile, then transform to PA-inner shape.
    let compiled = emitter::emit(program);
    let inner_def = transform_for_pa(&compiled);
    // Connection references come from `pa/connectionReferences.json` via
    // the resolver and end up at the top level of the compiled object.
    // Without forwarding them into the envelope, PA's importer rejects
    // every connector action ("API connection reference '<x>' could not
    // be found"). An absent file -> the existing empty default.
    let connection_references = compiled
        .get("connectionReferences")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Build per-connection-reference resource descriptors that the legacy
    // package wires together via apisMap, connectionsMap, and the root
    // manifest's `resources` block. PA's importer needs the full graph;
    // an empty connectionsMap fails with PackageFlowMissingConnectionMap.
    let conn_resources = build_connection_resources(&connection_references);

    let package_guid = Uuid::new_v4().to_string();
    let flow_guid = Uuid::new_v4().to_string();
    let telemetry_guid = Uuid::new_v4().to_string();
    // Timestamp is cosmetic; PA regenerates creator/time fields on import.
    // A static value keeps packages reproducible from paxc's perspective.
    let created_time = "2026-04-21T00:00:00.0000000Z";

    let root_manifest = build_root_manifest(
        name,
        &package_guid,
        &telemetry_guid,
        created_time,
        &conn_resources,
    );
    let inner_manifest = build_inner_manifest(&package_guid);
    let flow_def = build_flow_envelope(inner_def, name, &flow_guid, connection_references);

    let apis_map = build_apis_map(&conn_resources);
    let connections_map = build_connections_map(&conn_resources);

    let files: Vec<(String, Vec<u8>)> = vec![
        (
            "manifest.json".to_string(),
            serde_json::to_vec(&root_manifest)?,
        ),
        (
            "Microsoft.Flow/flows/manifest.json".to_string(),
            serde_json::to_vec(&inner_manifest)?,
        ),
        (
            format!("Microsoft.Flow/flows/{package_guid}/apisMap.json"),
            serde_json::to_vec(&apis_map)?,
        ),
        (
            format!("Microsoft.Flow/flows/{package_guid}/connectionsMap.json"),
            serde_json::to_vec(&connections_map)?,
        ),
        (
            format!("Microsoft.Flow/flows/{package_guid}/definition.json"),
            serde_json::to_vec(&flow_def)?,
        ),
    ];

    write_zip(out_path, &files)?;
    Ok(())
}

/// Transforms paxc's `{"definition": {...}}` output into the shape PA's
/// importer expects inside `properties.definition`. Rebuilds the object
/// in canonical key order: `$schema`, `contentVersion`, `parameters`,
/// `triggers`, `actions`.
fn transform_for_pa(compiled: &Value) -> Value {
    let old = compiled.get("definition").and_then(|v| v.as_object());
    let mut out = Map::new();

    if let Some(old) = old {
        if let Some(v) = old.get("$schema") {
            out.insert("$schema".to_string(), v.clone());
        }
        if let Some(v) = old.get("contentVersion") {
            out.insert("contentVersion".to_string(), v.clone());
        }
        // Parameters block: PA expects these even when unused.
        out.insert(
            "parameters".to_string(),
            json!({
                "$authentication": {"defaultValue": {}, "type": "SecureObject"},
                "$connections": {"defaultValue": {}, "type": "Object"}
            }),
        );
        if let Some(v) = old.get("triggers") {
            let mut triggers = v.clone();
            fix_manual_trigger_inputs(&mut triggers);
            out.insert("triggers".to_string(), triggers);
        }
        if let Some(v) = old.get("actions") {
            let mut actions = v.clone();
            lowercase_var_types(&mut actions);
            fix_connector_inputs(&mut actions);
            out.insert("actions".to_string(), actions);
        }
    }
    Value::Object(out)
}

/// PA's manual trigger expects `inputs: {schema: {...}}` even when empty,
/// not `inputs: {}` as paxc currently emits. Rewrites the matching shape.
fn fix_manual_trigger_inputs(triggers: &mut Value) {
    let Some(obj) = triggers.as_object_mut() else {
        return;
    };
    for (_, trig) in obj.iter_mut() {
        let Some(t) = trig.as_object_mut() else {
            continue;
        };
        let is_manual = t.get("type").and_then(|v| v.as_str()) == Some("Request")
            && t.get("kind").and_then(|v| v.as_str()) == Some("Button");
        if is_manual {
            t.insert(
                "inputs".to_string(),
                json!({"schema": {"type": "object", "properties": {}, "required": []}}),
            );
        }
    }
}

/// PA's designer exports variable types in lowercase (`integer`, `string`,
/// etc.). paxc's emitter uses the canonical Logic Apps capitalization. Both
/// forms probably work, but we match PA for safety. Recurses into the
/// `actions` subtrees of `Foreach` and `If` bodies.
fn lowercase_var_types(actions: &mut Value) {
    let Some(obj) = actions.as_object_mut() else {
        return;
    };
    for (_, action) in obj.iter_mut() {
        let Some(a) = action.as_object_mut() else {
            continue;
        };
        let kind = a
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "InitializeVariable" => {
                if let Some(vars) = a
                    .get_mut("inputs")
                    .and_then(|i| i.get_mut("variables"))
                    .and_then(|v| v.as_array_mut())
                {
                    for var in vars {
                        if let Some(ty) = var.get_mut("type")
                            && let Some(s) = ty.as_str()
                        {
                            *ty = Value::String(s.to_lowercase());
                        }
                    }
                }
            }
            "Foreach" => {
                if let Some(nested) = a.get_mut("actions") {
                    lowercase_var_types(nested);
                }
            }
            "If" => {
                if let Some(nested) = a.get_mut("actions") {
                    lowercase_var_types(nested);
                }
                if let Some(else_obj) = a.get_mut("else").and_then(|e| e.get_mut("actions")) {
                    lowercase_var_types(else_obj);
                }
            }
            _ => {}
        }
    }
}

/// Per-connector-action import fixups. PA's exporter and importer disagree
/// on connector input shapes in two ways that consistently appear together:
///
/// 1. `inputs.authentication: "@parameters('$authentication')"` is exported
///    but rejected on import (`WorkflowRunActionInputsInvalidProperty`).
///    Auto-injected from `connectionReferences` at runtime, so redundant.
/// 2. `inputs.host.connectionName` is the export label; the importer wants
///    `inputs.host.connectionReferenceName` (same VALUE — the connection
///    reference key — under a different field name). Without the rename
///    the importer fails with `WorkflowRunActionInputsMissingProperty`.
///
/// Applies to `OpenApiConnection` / `OpenApiConnectionWebhook` and recurses
/// through container bodies to catch nested connectors.
fn fix_connector_inputs(actions: &mut Value) {
    let Some(obj) = actions.as_object_mut() else {
        return;
    };
    for (_, action) in obj.iter_mut() {
        let Some(a) = action.as_object_mut() else {
            continue;
        };
        let kind = a
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "OpenApiConnection" | "OpenApiConnectionWebhook" => {
                if let Some(inputs) = a.get_mut("inputs").and_then(|v| v.as_object_mut()) {
                    inputs.remove("authentication");
                    if let Some(host) = inputs.get_mut("host").and_then(|v| v.as_object_mut()) {
                        // PA's import-time validator wants
                        // `host.connectionReferenceName`; PA's run-/save-time
                        // validator wants `host.connectionName`. The original
                        // export only carries the latter, but the importer
                        // rejects packages that lack the former. Set both to
                        // the same connection-reference key so each validator
                        // gets what it expects.
                        let conn = host
                            .get("connectionName")
                            .or_else(|| host.get("connectionReferenceName"))
                            .cloned();
                        if let Some(conn) = conn {
                            host.insert("connectionName".to_string(), conn.clone());
                            host.insert("connectionReferenceName".to_string(), conn);
                        }
                    }
                }
            }
            "Foreach" | "Scope" | "Until" => {
                if let Some(nested) = a.get_mut("actions") {
                    fix_connector_inputs(nested);
                }
            }
            "If" => {
                if let Some(nested) = a.get_mut("actions") {
                    fix_connector_inputs(nested);
                }
                if let Some(else_obj) = a.get_mut("else").and_then(|e| e.get_mut("actions")) {
                    fix_connector_inputs(else_obj);
                }
            }
            "Switch" => {
                if let Some(cases) = a.get_mut("cases").and_then(|v| v.as_object_mut()) {
                    for (_, case) in cases.iter_mut() {
                        if let Some(nested) = case.get_mut("actions") {
                            fix_connector_inputs(nested);
                        }
                    }
                }
                if let Some(default) = a.get_mut("default").and_then(|d| d.get_mut("actions")) {
                    fix_connector_inputs(default);
                }
            }
            _ => {}
        }
    }
}

/// Per-connection-reference resource descriptor used to wire up the legacy
/// package's manifest, apisMap, and connectionsMap. Built from
/// `pa/connectionReferences.json` (forwarded through the compiled envelope).
struct ConnResource {
    /// Connection reference name (the key in connectionReferences and the
    /// label inside `host.connectionReferenceName`).
    ref_name: String,
    /// API path (e.g., `/providers/Microsoft.PowerApps/apis/shared_sharepointonline`).
    api_id: String,
    /// User-facing API display name (best-effort from `apiName`; PA fills
    /// in the canonical name during import). Cosmetic — not load-bearing.
    api_display_name: String,
    /// Resource GUID for the API entry in the root manifest.
    api_guid: String,
    /// Resource GUID for the connection entry in the root manifest.
    connection_guid: String,
}

/// Walk the connectionReferences map and synthesize a ConnResource for each
/// reference. Generates package-local GUIDs for the API and connection
/// resources; the import experience prompts the user to map each connection
/// to one in their tenant.
fn build_connection_resources(connection_references: &Value) -> Vec<ConnResource> {
    let Some(map) = connection_references.as_object() else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, body)| {
            let api_id = body
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let api_name = body
                .get("apiName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // PA's display names for connectors are usually a friendly
            // capitalization of the apiName (`sharepointonline` →
            // `SharePoint`). We don't carry the friendly map; fall back to
            // the apiName itself, which the importer overrides with the
            // tenant's canonical name anyway.
            let api_display_name = if api_name.is_empty() {
                name.clone()
            } else {
                api_name
            };
            ConnResource {
                ref_name: name.clone(),
                api_id,
                api_display_name,
                api_guid: Uuid::new_v4().to_string(),
                connection_guid: Uuid::new_v4().to_string(),
            }
        })
        .collect()
}

fn build_apis_map(conn_resources: &[ConnResource]) -> Value {
    let mut m = Map::new();
    for r in conn_resources {
        m.insert(r.ref_name.clone(), Value::String(r.api_guid.clone()));
    }
    Value::Object(m)
}

fn build_connections_map(conn_resources: &[ConnResource]) -> Value {
    let mut m = Map::new();
    for r in conn_resources {
        m.insert(r.ref_name.clone(), Value::String(r.connection_guid.clone()));
    }
    Value::Object(m)
}

fn build_root_manifest(
    name: &str,
    package_guid: &str,
    telemetry_guid: &str,
    created_time: &str,
    conn_resources: &[ConnResource],
) -> Value {
    let mut resources = Map::new();

    // Flow resource depends on every API and connection it uses.
    let mut flow_depends_on: Vec<Value> = Vec::new();
    for r in conn_resources {
        flow_depends_on.push(Value::String(r.api_guid.clone()));
        flow_depends_on.push(Value::String(r.connection_guid.clone()));
    }
    resources.insert(
        package_guid.to_string(),
        json!({
            "type": "Microsoft.Flow/flows",
            // paxc-emitted packages are always fresh: each compile gets a
            // new flow GUID. Suggesting "Update" makes PA's importer try
            // to UPDATE an existing flow (matching by some identity), which
            // for a fresh package silently lands in a not-quite-visible
            // state on first import and only surfaces the flow on a second
            // pass. Suggesting "New" tells the importer to create the flow
            // as a new entity. `creationType: "New, Update"` still lets the
            // user override and update an existing flow if they want to.
            "suggestedCreationType": "New",
            "creationType": "New, Update",
            "details": {"displayName": name},
            "configurableBy": "User",
            "hierarchy": "Root",
            "dependsOn": flow_depends_on
        }),
    );

    // For each connection reference, declare two resources: the API and the
    // user-mapped connection. The connection depends on its API.
    for r in conn_resources {
        resources.insert(
            r.api_guid.clone(),
            json!({
                "id": r.api_id,
                "name": r.ref_name,
                "type": "Microsoft.PowerApps/apis",
                "suggestedCreationType": "Existing",
                "details": {"displayName": r.api_display_name},
                "configurableBy": "System",
                "hierarchy": "Child",
                "dependsOn": []
            }),
        );
        resources.insert(
            r.connection_guid.clone(),
            json!({
                "type": "Microsoft.PowerApps/apis/connections",
                "suggestedCreationType": "Existing",
                "creationType": "Existing",
                "details": {"displayName": r.ref_name},
                "configurableBy": "User",
                "hierarchy": "Child",
                "dependsOn": [r.api_guid.clone()]
            }),
        );
    }

    json!({
        "schema": "1.0",
        "details": {
            "displayName": name,
            "description": "",
            "createdTime": created_time,
            "packageTelemetryId": telemetry_guid,
            "creator": "N/A",
            "sourceEnvironment": ""
        },
        "resources": resources
    })
}

fn build_inner_manifest(package_guid: &str) -> Value {
    json!({
        "packageSchemaVersion": "1.0",
        "flowAssets": {"assetPaths": [package_guid]}
    })
}

fn build_flow_envelope(
    inner_def: Value,
    name: &str,
    flow_guid: &str,
    connection_references: Value,
) -> Value {
    json!({
        "name": flow_guid,
        "id": format!("/providers/Microsoft.Flow/flows/{flow_guid}"),
        "type": "Microsoft.Flow/flows",
        "properties": {
            "apiId": "/providers/Microsoft.PowerApps/apis/shared_logicflows",
            "displayName": name,
            "definition": inner_def,
            "connectionReferences": connection_references,
            "flowFailureAlertSubscribed": false,
            "isManaged": false
        }
    })
}

fn write_zip(out_path: &Path, files: &[(String, Vec<u8>)]) -> Result<(), PackageError> {
    let file = File::create(out_path)?;
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for (path, data) in files {
        zip.start_file(path, options)?;
        zip.write_all(data)?;
    }
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_connector_inputs_removes_authentication_field() {
        let mut actions = json!({
            "Get_items": {
                "type": "OpenApiConnection",
                "inputs": {
                    "parameters": { "$top": 5 },
                    "host": { "apiId": "x" },
                    "authentication": "@parameters('$authentication')"
                }
            }
        });
        fix_connector_inputs(&mut actions);
        let inputs = &actions["Get_items"]["inputs"];
        assert!(inputs.get("authentication").is_none());
        assert!(inputs.get("parameters").is_some());
        assert!(inputs.get("host").is_some());
    }

    #[test]
    fn fix_connector_inputs_emits_both_connection_name_fields() {
        // PA's import-time and save-time validators want different field
        // names for the same value. We set both.
        let mut actions = json!({
            "Get_items": {
                "type": "OpenApiConnection",
                "inputs": {
                    "host": {
                        "apiId": "x",
                        "connectionName": "shared_sharepointonline",
                        "operationId": "GetItems"
                    }
                }
            }
        });
        fix_connector_inputs(&mut actions);
        let host = &actions["Get_items"]["inputs"]["host"];
        assert_eq!(
            host.get("connectionName").and_then(|v| v.as_str()),
            Some("shared_sharepointonline")
        );
        assert_eq!(
            host.get("connectionReferenceName").and_then(|v| v.as_str()),
            Some("shared_sharepointonline")
        );
    }

    #[test]
    fn fix_connector_inputs_recurses_through_containers() {
        let mut actions = json!({
            "Apply_to_each": {
                "type": "Foreach",
                "actions": {
                    "If_check": {
                        "type": "If",
                        "actions": {
                            "Inner_call": {
                                "type": "OpenApiConnectionWebhook",
                                "inputs": {
                                    "host": {},
                                    "authentication": "@parameters('$authentication')"
                                }
                            }
                        },
                        "else": {
                            "actions": {
                                "Else_call": {
                                    "type": "OpenApiConnection",
                                    "inputs": {
                                        "host": {},
                                        "authentication": "@parameters('$authentication')"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        fix_connector_inputs(&mut actions);
        let inner =
            &actions["Apply_to_each"]["actions"]["If_check"]["actions"]["Inner_call"]["inputs"];
        let else_call = &actions["Apply_to_each"]["actions"]["If_check"]["else"]["actions"]["Else_call"]
            ["inputs"];
        assert!(inner.get("authentication").is_none());
        assert!(else_call.get("authentication").is_none());
    }

    #[test]
    fn fix_connector_inputs_leaves_non_connectors_alone() {
        // Variable / Compose actions don't have inputs.authentication, but
        // the pass should be a no-op even on hypothetical other types.
        let mut actions = json!({
            "Initialize_x": {
                "type": "InitializeVariable",
                "inputs": { "variables": [{ "name": "x", "type": "Integer" }] }
            },
            "Compose_y": {
                "type": "Compose",
                "inputs": "hello"
            }
        });
        let snapshot = actions.clone();
        fix_connector_inputs(&mut actions);
        assert_eq!(actions, snapshot);
    }
}
