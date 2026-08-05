//! Protocol v3 schema assembly — the single source of truth.
//!
//! The Rust wire types in `cu-core`/`cu-trace`/`cu-runtime` derive
//! `schemars::JsonSchema`; this crate assembles them into one JSON Schema
//! document (`protocol/computer-use.schema.json` via the binary) plus the
//! `x-protocol-meta` extension that carries the protocol version contract.
//!
//! Nothing else may define the wire format: the SDK, MCP server, and Pi
//! extension consume the TypeScript generated from this schema (`pnpm
//! generate:protocol`), and `pnpm check:protocol` fails when the committed
//! schema/generated files drift from the Rust source of truth.

use schemars::gen::{SchemaGenerator, SchemaSettings};

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use cu_core::actions::{ComputerAction, MouseButton, RedactedText, TextInputMethod, WaitPolicy};
use cu_core::coordinates::{CoordinateSpace, Point, Region};
use cu_core::errors::{
    BoundsDetail, ConfirmationDetail, ErrorCode, PermissionIssue, PermissionKind, StaleFrameDetail,
};
use cu_core::protocol::{
    ActParams, ActResult, ActionResultReport, CancelParams, CancelResult, ClientInfo,
    InspectMapping, InspectParams, InspectResult, ObserveParams, ObserveResult, RpcError,
    RpcRequest, RpcResponse, RuntimeVersionResult, SessionParams, SessionResult, SessionSummary,
    ShutdownParams, StabilizationInfo, TraceEntry, TraceExport, TraceExportParams, TraceGetParams,
    TraceReplayParams, TraceReport, TraceSummary, JSONRPC_VERSION,
};
use cu_core::security::{
    MAX_CLIENT_PROTOCOL_VERSION, MIN_CLIENT_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use cu_core::sessions::{SessionAction, SessionState};
use cu_runtime::stale_frame::StaleFramePolicy;
use cu_trace::recorder::TraceMode;

pub const SCHEMA_TITLE: &str = "Computer Use Protocol v3";
pub const SCHEMA_DESCRIPTION: &str = concat!(
    "JSON-RPC 2.0 wire protocol between the Computer Use daemon and its adapters ",
    "(SDK, MCP server, Pi extension). Generated from the Rust wire types — ",
    "edit the Rust source, then run `pnpm generate:protocol`. ",
    "Capability tokens (control / observation / admin) are 256-bit secrets ",
    "issued exactly once; the daemon stores only their SHA-256 hashes and never ",
    "repeats them in responses."
);

macro_rules! register {
    ($gen:expr, $t:ty) => {{
        $gen.subschema_for::<$t>();
    }};
}

/// Protocol version contract carried in `x-protocol-meta`. Read from the same
/// constants the daemon serves over `runtime.version` — one source of truth.
pub fn protocol_meta() -> Value {
    json!({
        "protocol_version": PROTOCOL_VERSION,
        "minimum_client_protocol_version": MIN_CLIENT_PROTOCOL_VERSION,
        "maximum_client_protocol_version": MAX_CLIENT_PROTOCOL_VERSION,
        "jsonrpc_version": JSONRPC_VERSION,
    })
}

/// Fields that are **always present on the wire as explicit `null`** even
/// though they are `Option` in Rust (`session.summary`'s contract: "every
/// field is always present — consumers read `summary.session_id == null`
/// without juggling absence"). schemars marks every `Option` field optional,
/// so assert these back into `required`.
const ALWAYS_PRESENT: &[(&str, &[&str])] = &[(
    "SessionSummary",
    &[
        "session_id",
        "state",
        "owner_client_id",
        "owner_client_name",
        "message",
    ],
)];

/// Drop the `null` alternative from an optional property schema (the wire
/// omits `skip_serializing_if` fields entirely; they are never `null`).
/// Returns true when a null alternative was removed.
fn remove_null_alternative(v: &mut Value) -> bool {
    let Value::Object(map) = v else {
        return false;
    };
    if let Some(Value::Array(types)) = map.get("type") {
        let without_null: Vec<Value> = types.iter().filter(|t| **t != "null").cloned().collect();
        if without_null.len() != types.len() {
            if without_null.len() == 1 {
                map.insert("type".into(), without_null[0].clone());
            } else {
                map.insert("type".into(), Value::Array(without_null));
            }
            return true;
        }
    }
    if let Some(Value::Array(any)) = map.get("anyOf") {
        let without_null: Vec<Value> = any
            .iter()
            .filter(|s| s["type"] != "null")
            .cloned()
            .collect();
        if without_null.len() != any.len() {
            if without_null.len() == 1 {
                // Replace the wrapper with the single branch, preserving any
                // documentation on the wrapper itself.
                let description = map.get("description").cloned();
                let mut single = without_null[0].clone();
                if let (Some(d), Value::Object(ref mut m)) = (description, &mut single) {
                    m.entry("description").or_insert(d);
                }
                *v = single;
            } else {
                map.insert("anyOf".into(), Value::Array(without_null));
            }
            return true;
        }
    }
    false
}

/// A schema entry that is only `{"description": ...}` describes
/// `serde_json::Value` — an unconstrained JSON value. Without this fixup it
/// would type as "object with unknown keys" in the generated TypeScript.
fn is_unconstrained_value(v: &Value) -> bool {
    let Value::Object(map) = v else { return false };
    map.keys().all(|k| k == "description")
}

/// Post-process one `$defs` entry so the emitted schema matches the actual
/// wire semantics (which the derive cannot always express):
/// - `Option` fields with `skip_serializing_if` are optional and never `null`;
/// - `ALWAYS_PRESENT` fields are required (even though `null`-typed);
/// - `serde_json::Value` fields are unconstrained JSON, not "any object".
fn fixup_def(def_name: &str, v: &mut Value) {
    let Value::Object(map) = v else { return };
    let mut required: Vec<String> = map
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| {
            r.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(props) = map.get_mut("properties").and_then(|p| p.as_object_mut()) else {
        return;
    };
    for (name, prop) in props.iter_mut() {
        let always_present = ALWAYS_PRESENT
            .iter()
            .any(|(d, fields)| *d == def_name && fields.contains(&name.as_str()));
        if always_present {
            if !required.contains(name) {
                required.push(name.clone());
            }
        } else if is_unconstrained_value(prop) {
            *prop = Value::Bool(true);
        } else {
            remove_null_alternative(prop);
        }
    }
    if !required.is_empty() {
        map.insert(
            "required".into(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
}

/// schemars 0.8 emits draft-07 tuple form (`items: [s1, s2]`) even under
/// 2019-09 settings; 2019-09 requires `prefixItems` + `items: false` for a
/// fixed-length tuple. Rewrite those nodes so the emitted document is valid
/// 2019-09.
fn fixup_tuple_items(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get("items") {
                let prefix = items.clone();
                map.insert("prefixItems".into(), Value::Array(prefix));
                map.insert("items".into(), Value::Bool(false));
            }
            for val in map.values_mut() {
                fixup_tuple_items(val);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                fixup_tuple_items(val);
            }
        }
        _ => {}
    }
}

/// Assemble the full protocol schema document. Deterministic: `$defs` are
/// sorted by name and the extension fields have a fixed order, so the emitted
/// file is byte-stable across runs (drift check relies on this).
pub fn build_protocol_schema() -> Value {
    let settings = SchemaSettings::draft2019_09().with(|s| {
        s.definitions_path = "#/$defs/".to_string();
    });
    let mut gen = SchemaGenerator::new(settings);

    // JSON-RPC 2.0 envelope.
    register!(gen, RpcRequest);
    register!(gen, RpcResponse);
    register!(gen, RpcError);
    // Sessions.
    register!(gen, SessionParams);
    register!(gen, SessionResult);
    register!(gen, SessionSummary);
    register!(gen, SessionState);
    register!(gen, SessionAction);
    register!(gen, ClientInfo);
    // Observe / inspect.
    register!(gen, ObserveParams);
    register!(gen, ObserveResult);
    register!(gen, InspectParams);
    register!(gen, InspectMapping);
    register!(gen, InspectResult);
    // Act.
    register!(gen, ActParams);
    register!(gen, ActionResultReport);
    register!(gen, StabilizationInfo);
    register!(gen, ActResult);
    register!(gen, TraceReport);
    register!(gen, ComputerAction);
    register!(gen, MouseButton);
    register!(gen, TextInputMethod);
    register!(gen, WaitPolicy);
    register!(gen, Point);
    register!(gen, CoordinateSpace);
    register!(gen, Region);
    // Cancel.
    register!(gen, CancelParams);
    register!(gen, CancelResult);
    // Traces.
    register!(gen, TraceGetParams);
    register!(gen, TraceExportParams);
    register!(gen, TraceReplayParams);
    register!(gen, TraceSummary);
    register!(gen, TraceEntry);
    register!(gen, TraceExport);
    register!(gen, TraceMode);
    // Runtime.
    register!(gen, RuntimeVersionResult);
    register!(gen, ShutdownParams);
    register!(gen, StaleFramePolicy);
    // Errors — the code enum plus the structured `error.data` detail payloads
    // (supersedes the former hand-written protocol/errors.schema.json).
    register!(gen, ErrorCode);
    register!(gen, StaleFrameDetail);
    register!(gen, PermissionIssue);
    register!(gen, PermissionKind);
    register!(gen, BoundsDetail);
    register!(gen, ConfirmationDetail);
    // RedactedText is referenced by TraceEntry; register explicitly so it is
    // always part of the generated type library.
    register!(gen, RedactedText);

    let defs: BTreeMap<String, Value> = gen
        .definitions_mut()
        .iter()
        .map(|(name, schema)| {
            (
                name.clone(),
                serde_json::to_value(schema).expect("schema entry serializes"),
            )
        })
        .collect();
    let defs_obj: Map<String, Value> = defs.into_iter().collect();

    let mut root = json!({
        "$schema": "https://json-schema.org/draft/2019-09/schema",
        "$id": "https://github.com/taotao135791-bit/oc-computer-use/blob/main/protocol/computer-use.schema.json",
        "title": SCHEMA_TITLE,
        "description": SCHEMA_DESCRIPTION,
        "type": "object",
        "x-protocol-meta": protocol_meta(),
        "$defs": defs_obj,
    });
    fixup_tuple_items(&mut root);
    // Apply the per-def wire-semantics fixups after the tuple fixup (which may
    // have replaced whole sub-schemas).
    let defs = root
        .get_mut("$defs")
        .and_then(|d| d.as_object_mut())
        .expect("root carries $defs");
    for (name, def) in defs.iter_mut() {
        fixup_def(name, def);
        fixup_tuple_items(def);
    }
    root
}
