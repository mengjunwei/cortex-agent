//! Schema adapter for Anthropic's Claude models.
//!
//! Anthropic has the most permissive JSON Schema support among major LLM providers.
//! The [`AnthropicSchemaAdapter`] applies only minimal transforms:
//!
//! 1. Strip `$schema` keyword (meta-keyword no provider supports)
//! 2. Strip conditional keywords (`if`/`then`/`else`)
//! 3. Add implicit `"type": "object"` when `properties` exists without a `type` field
//!
//! Everything else passes through unchanged: `$ref`, `$defs`, `anyOf`, `oneOf`,
//! `allOf`, `additionalProperties`, type arrays, `const`, and all `format` values.
//!
//! # Example
//!
//! ```rust
//! use adk_model::anthropic::AnthropicSchemaAdapter;
//! use adk_rust::SchemaAdapter;
//! use serde_json::json;
//!
//! let adapter = AnthropicSchemaAdapter;
//! let schema = json!({
//!     "$schema": "http://json-schema.org/draft-07/schema#",
//!     "type": "object",
//!     "properties": {
//!         "name": { "type": "string", "const": "fixed" }
//!     },
//!     "$ref": "#/$defs/Foo",
//!     "$defs": { "Foo": { "type": "string" } },
//!     "anyOf": [{ "type": "string" }, { "type": "number" }],
//!     "additionalProperties": false
//! });
//!
//! let normalized = adapter.normalize_schema(schema);
//! // $schema removed
//! assert!(normalized.get("$schema").is_none());
//! // const preserved (Anthropic supports it natively)
//! assert_eq!(normalized["properties"]["name"]["const"], "fixed");
//! // $ref, $defs, anyOf, additionalProperties all preserved
//! assert!(normalized.get("$ref").is_some());
//! assert!(normalized.get("$defs").is_some());
//! assert!(normalized.get("anyOf").is_some());
//! assert!(normalized.get("additionalProperties").is_some());
//! ```

use std::borrow::Cow;

use adk_rust::{SchemaAdapter, schema_utils};
use serde_json::Value;

/// Schema adapter for Anthropic's Claude models (near pass-through).
///
/// Anthropic's function-calling API accepts most JSON Schema features natively.
/// This adapter only strips the meta-keywords that no provider supports and adds
/// implicit object types where needed.
///
/// ## Preserved Features
///
/// - `$ref` and `$defs` (reference resolution not needed)
/// - `anyOf`, `oneOf`, `allOf` (combiners supported)
/// - `additionalProperties` (supported as-is)
/// - Type arrays (e.g., `["string", "null"]`)
/// - `const` keyword (native support)
/// - All `format` values (no stripping)
///
/// ## Transforms Applied
///
/// 1. `strip_schema_keyword` — removes `$schema` meta-keyword
/// 2. `strip_conditional_keywords` — removes `if`/`then`/`else`
/// 3. `add_implicit_object_type` — adds `"type": "object"` when `properties` exists
///
/// ## Tool Name Normalization
///
/// Truncates tool names exceeding 64 characters at the nearest valid UTF-8
/// character boundary, preserving the prefix.
#[derive(Debug)]
pub struct AnthropicSchemaAdapter;

impl SchemaAdapter for AnthropicSchemaAdapter {
    fn normalize_schema(&self, mut schema: Value) -> Value {
        schema_utils::strip_schema_keyword(&mut schema);
        schema_utils::strip_conditional_keywords(&mut schema);
        schema_utils::add_implicit_object_type(&mut schema);
        schema
    }

    fn normalize_tool_name<'a>(&self, name: &'a str) -> Cow<'a, str> {
        if name.len() <= 64 {
            Cow::Borrowed(name)
        } else {
            // Find the largest valid UTF-8 boundary at or before 64 bytes.
            let mut end = 64;
            while end > 0 && !name.is_char_boundary(end) {
                end -= 1;
            }
            Cow::Owned(name[..end].to_string())
        }
    }
}
