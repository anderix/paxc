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

use crate::emitter;
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

    let package_guid = Uuid::new_v4().to_string();
    let flow_guid = Uuid::new_v4().to_string();
    let telemetry_guid = Uuid::new_v4().to_string();
    // Timestamp is cosmetic; PA regenerates creator/time fields on import.
    // A static value keeps packages reproducible from paxc's perspective.
    let created_time = "2026-04-21T00:00:00.0000000Z";

    let root_manifest = build_root_manifest(name, &package_guid, &telemetry_guid, created_time);
    let inner_manifest = build_inner_manifest(&package_guid);
    let flow_def = build_flow_envelope(inner_def, name, &flow_guid);

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
            b"{}".to_vec(),
        ),
        (
            format!("Microsoft.Flow/flows/{package_guid}/connectionsMap.json"),
            b"{}".to_vec(),
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
                        if let Some(ty) = var.get_mut("type") {
                            if let Some(s) = ty.as_str() {
                                *ty = Value::String(s.to_lowercase());
                            }
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

fn build_root_manifest(
    name: &str,
    package_guid: &str,
    telemetry_guid: &str,
    created_time: &str,
) -> Value {
    let mut resources = Map::new();
    resources.insert(
        package_guid.to_string(),
        json!({
            "type": "Microsoft.Flow/flows",
            "suggestedCreationType": "Update",
            "creationType": "Existing, New, Update",
            "details": {"displayName": name},
            "configurableBy": "User",
            "hierarchy": "Root",
            "dependsOn": []
        }),
    );
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

fn build_flow_envelope(inner_def: Value, name: &str, flow_guid: &str) -> Value {
    json!({
        "name": flow_guid,
        "id": format!("/providers/Microsoft.Flow/flows/{flow_guid}"),
        "type": "Microsoft.Flow/flows",
        "properties": {
            "apiId": "/providers/Microsoft.PowerApps/apis/shared_logicflows",
            "displayName": name,
            "definition": inner_def,
            "connectionReferences": {},
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
