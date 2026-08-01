use std::collections::BTreeSet;

use serde_json::Value;

use crate::builtin::{FunctionDescriptor, NamespaceDescriptor};

pub(crate) fn render_inventory(namespaces: &[NamespaceDescriptor]) -> String {
    namespaces
        .iter()
        .flat_map(|namespace| {
            namespace
                .functions
                .iter()
                .map(move |function| render_function(namespace, function))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_function(namespace: &NamespaceDescriptor, function: &FunctionDescriptor) -> String {
    let path = format!("{}.{}", namespace.path, function.name);
    let signature = if path == "lam.dir" {
        "lam.dir(query?: { path?: string }): NamespaceDescriptor[]".to_owned()
    } else if path == "lam.result" {
        "lam.result<T extends JsonValue>(value: T): T".to_owned()
    } else {
        let input = schema_type(&function.input_schema, 0);
        let output = schema_type(&function.output_schema, 0);
        format!("{path}(input: {input}): Promise<{output}>")
    };
    let docs = first_paragraph(&function.docs);
    if docs.is_empty() {
        format!("- `{signature}`")
    } else {
        format!("- `{signature}` — {docs}")
    }
}

fn first_paragraph(docs: &str) -> String {
    docs.split("\n\n")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn schema_type(schema: &Value, depth: usize) -> String {
    render_schema_type(schema, schema, depth)
}

fn render_schema_type(schema: &Value, root: &Value, depth: usize) -> String {
    if depth >= 4 {
        return schema_title(schema).unwrap_or_else(|| "unknown".to_owned());
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
        {
            return render_schema_type(target, root, depth + 1);
        }
        return reference
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map_or_else(|| "unknown".to_owned(), decode_reference_name);
    }
    if let Some(value) = schema.get("const") {
        return literal(value);
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return enum_type(values);
    }
    for alternatives in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(alternatives).and_then(Value::as_array) {
            let mut rendered = Vec::new();
            for variant in variants.iter().take(4) {
                let variant = render_schema_type(variant, root, depth + 1);
                if !rendered.contains(&variant) {
                    rendered.push(variant);
                }
            }
            if variants.len() > 4 {
                rendered.push("…".to_owned());
            }
            return rendered.join(" | ");
        }
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array) {
        return types
            .iter()
            .map(|kind| primitive_type(kind.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => object_type(schema, root, depth),
        Some("array") => {
            let item = schema.get("items").map_or_else(
                || "unknown".to_owned(),
                |item| render_schema_type(item, root, depth + 1),
            );
            format!("{item}[]")
        }
        Some(kind) => primitive_type(kind),
        None => {
            if schema.get("properties").is_some() {
                object_type(schema, root, depth)
            } else {
                schema_title(schema).unwrap_or_else(|| "unknown".to_owned())
            }
        }
    }
}

fn object_type(schema: &Value, root: &Value, depth: usize) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return schema
            .get("additionalProperties")
            .filter(|additional| additional.is_object())
            .map_or_else(
                || "object".to_owned(),
                |additional| {
                    format!(
                        "Record<string, {}>",
                        render_schema_type(additional, root, depth + 1)
                    )
                },
            );
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut fields = properties
        .iter()
        .take(6)
        .map(|(name, value)| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            format!(
                "{name}{optional}: {}",
                render_schema_type(value, root, depth + 1)
            )
        })
        .collect::<Vec<_>>();
    if properties.len() > 6 {
        fields.push("…".to_owned());
    }
    format!("{{ {} }}", fields.join("; "))
}

fn primitive_type(kind: &str) -> String {
    match kind {
        "integer" | "number" => "number",
        "boolean" => "boolean",
        "null" => "null",
        "string" => "string",
        "object" => "object",
        "array" => "unknown[]",
        _ => "unknown",
    }
    .to_owned()
}

fn enum_type(values: &[Value]) -> String {
    if values.len() > 6 {
        return "string".to_owned();
    }
    values.iter().map(literal).collect::<Vec<_>>().join(" | ")
}

fn literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned())
}

fn schema_title(schema: &Value) -> Option<String> {
    schema
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn decode_reference_name(name: &str) -> String {
    name.replace("~1", "/").replace("~0", "~")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_only_the_first_documentation_paragraph() {
        assert_eq!(
            first_paragraph("Read one file.\nThe first paragraph wraps.\n\nMore detail."),
            "Read one file. The first paragraph wraps."
        );
    }

    #[test]
    fn resolves_local_schema_references_with_a_depth_bound() {
        let schema = serde_json::json!({
            "type": "array",
            "items": { "$ref": "#/$defs/Item" },
            "$defs": {
                "Item": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        });

        assert_eq!(schema_type(&schema, 0), "{ path: string }[]");
    }
}
