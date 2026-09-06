//! JSON-schema builder for structured parameter extraction (ROUT-03, plan
//! 09-04). `build_param_schema` reads ONLY the matched workflow's own
//! `parameters` map -- never the registry, never the utterance, never any
//! other workflow's schema -- so an instruction embedded in an utterance can
//! at worst distort the VALUES extracted for the workflow the similarity
//! match already selected; it can never retarget which workflow's schema is
//! built (T-09-01).

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::definition::{ParameterSpec, ParameterType};

/// Exact JSON Schema `type` strings `ParameterType` maps to. No catch-all
/// arm exists in the match below -- a new `ParameterType` variant must break
/// this build loudly at compile time rather than silently be treated as a
/// string.
const PARAM_TYPE_STRING: &str = "string";
const PARAM_TYPE_INTEGER: &str = "integer";
const PARAM_TYPE_BOOLEAN: &str = "boolean";

/// Builds a JSON Schema object constraining Ollama's `/api/generate`
/// `format` field to exactly `params`' declared shape.
///
/// Property order is deterministic: parameter names are sorted before the
/// schema is built, so the emitted bytes are byte-identical across calls for
/// the same input -- `HashMap` iteration order is not stable, and an
/// unstable prompt would make the model's output unreproducible for no
/// benefit (ROUT-03 ordering probe).
///
/// `additionalProperties: false` means the model cannot invent a key the
/// workflow never declared. `required` lists the required parameter names,
/// in the same sorted order.
pub fn build_param_schema(params: &HashMap<String, ParameterSpec>) -> Value {
    let mut names: Vec<&String> = params.keys().collect();
    names.sort();

    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();

    for name in &names {
        // Present in `params` by construction -- `names` was collected from
        // `params.keys()` immediately above.
        let spec = &params[name.as_str()];

        let type_str = match spec.type_ {
            ParameterType::String => PARAM_TYPE_STRING,
            ParameterType::Int => PARAM_TYPE_INTEGER,
            ParameterType::Bool => PARAM_TYPE_BOOLEAN,
        };

        let mut property = Map::new();
        property.insert("type".to_string(), Value::String(type_str.to_string()));
        if let Some(description) = &spec.description {
            property.insert("description".to_string(), Value::String(description.clone()));
        }

        properties.insert((*name).clone(), Value::Object(property));
        if spec.required {
            required.push(Value::String((*name).clone()));
        }
    }

    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert("required".to_string(), Value::Array(required));
    schema.insert("additionalProperties".to_string(), Value::Bool(false));
    Value::Object(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(type_: ParameterType, required: bool, description: Option<&str>) -> ParameterSpec {
        ParameterSpec {
            type_,
            description: description.map(|d| d.to_string()),
            required,
        }
    }

    #[test]
    fn maps_string_int_and_bool_to_their_json_schema_type_names() {
        let mut params = HashMap::new();
        params.insert("name".to_string(), spec(ParameterType::String, false, None));
        params.insert("count".to_string(), spec(ParameterType::Int, false, None));
        params.insert("enabled".to_string(), spec(ParameterType::Bool, false, None));

        let schema = build_param_schema(&params);
        let properties = schema["properties"].as_object().expect("properties must be an object");

        assert_eq!(properties["name"]["type"], "string");
        assert_eq!(properties["count"]["type"], "integer");
        assert_eq!(properties["enabled"]["type"], "boolean");
    }

    #[test]
    fn only_required_parameters_appear_in_the_required_array() {
        let mut params = HashMap::new();
        params.insert("required_one".to_string(), spec(ParameterType::String, true, None));
        params.insert("optional_one".to_string(), spec(ParameterType::String, false, None));

        let schema = build_param_schema(&params);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required must be an array")
            .iter()
            .map(|v| v.as_str().expect("required entries must be strings"))
            .collect();

        assert_eq!(required, vec!["required_one"]);
    }

    #[test]
    fn additional_properties_is_always_false() {
        let schema = build_param_schema(&HashMap::new());
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn a_description_when_present_is_carried_into_the_property() {
        let mut params = HashMap::new();
        params.insert(
            "duration_minutes".to_string(),
            spec(ParameterType::Int, true, Some("how many minutes")),
        );

        let schema = build_param_schema(&params);
        assert_eq!(schema["properties"]["duration_minutes"]["description"], "how many minutes");
    }

    #[test]
    fn an_absent_description_never_adds_the_key() {
        let mut params = HashMap::new();
        params.insert("plain".to_string(), spec(ParameterType::String, false, None));

        let schema = build_param_schema(&HashMap::new());
        assert!(schema["properties"].as_object().unwrap().is_empty());

        let schema_with_plain = build_param_schema(&params);
        assert!(!schema_with_plain["properties"]["plain"]
            .as_object()
            .expect("property must be an object")
            .contains_key("description"));
    }

    #[test]
    fn properties_and_required_are_emitted_in_sorted_parameter_name_order() {
        let mut params = HashMap::new();
        params.insert("zeta".to_string(), spec(ParameterType::String, true, None));
        params.insert("alpha".to_string(), spec(ParameterType::String, true, None));
        params.insert("mid".to_string(), spec(ParameterType::String, true, None));

        let schema = build_param_schema(&params);
        let property_names: Vec<&str> =
            schema["properties"].as_object().unwrap().keys().map(|s| s.as_str()).collect();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert_eq!(property_names, vec!["alpha", "mid", "zeta"]);
        assert_eq!(required, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn calling_build_param_schema_twice_on_the_same_input_produces_byte_identical_output() {
        let mut params = HashMap::new();
        params.insert("b_param".to_string(), spec(ParameterType::Int, true, Some("second")));
        params.insert("a_param".to_string(), spec(ParameterType::String, false, Some("first")));

        let first = serde_json::to_string(&build_param_schema(&params)).unwrap();
        let second = serde_json::to_string(&build_param_schema(&params)).unwrap();

        assert_eq!(first, second, "expected byte-identical schema bytes across repeated calls");
    }

    #[test]
    fn an_empty_parameter_map_still_produces_a_well_formed_object_schema() {
        let schema = build_param_schema(&HashMap::new());
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].as_object().unwrap().is_empty());
        assert!(schema["required"].as_array().unwrap().is_empty());
        assert_eq!(schema["additionalProperties"], false);
    }
}
