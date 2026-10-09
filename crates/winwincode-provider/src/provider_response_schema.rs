// SPDX-License-Identifier: Apache-2.0

//! Bounded schema subset for explicitly configured Responses JSON-object compatibility.
//! Unsupported schema semantics fail before the request is sent.
//! Supported keywords are type, properties, required, additionalProperties (boolean),
//! items, enum (scalar safe integers/string/boolean/null), minItems, maxItems and
//! the project's fixed ASCII token pattern. Type unions are basic type plus null.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_DEPTH: usize = 64;
const MAX_OUTPUT_NODES: usize = 65_536;
const TOKEN_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._:/@-]*$";
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SchemaError;

#[derive(Clone)]
pub(crate) struct ResponseSchema {
    original: Value,
    root: Node,
}

#[derive(Clone)]
struct Node {
    kind: Kind,
    nullable: bool,
    properties: BTreeMap<String, Self>,
    required: BTreeSet<String>,
    additional: bool,
    items: Option<Box<Self>>,
    enumeration: Option<Vec<Value>>,
    min_items: usize,
    max_items: Option<usize>,
    token_pattern: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Object,
    Array,
    String,
    Integer,
    Number,
    Boolean,
    Null,
}

impl ResponseSchema {
    pub(crate) fn compile(schema: &Value) -> Result<Self, SchemaError> {
        if serde_json::to_vec(schema).map_err(|_| SchemaError)?.len() > MAX_SCHEMA_BYTES {
            return Err(SchemaError);
        }
        Ok(Self {
            original: schema.clone(),
            root: Node::compile(schema, 0, &mut 0)?,
        })
    }

    pub(crate) fn is_object(&self) -> bool {
        self.root.kind == Kind::Object && !self.root.nullable
    }

    pub(crate) fn json(&self) -> Result<String, SchemaError> {
        serde_json::to_string(&self.original).map_err(|_| SchemaError)
    }

    pub(crate) fn accepts(&self, text: &str) -> bool {
        if text.len() > MAX_OUTPUT_BYTES {
            return false;
        }
        if serde_json::from_str::<UniqueKeys>(text).is_err() || reserved_object_key(text) {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        bounded(&value, 0, &mut 0) && self.root.accepts(&value)
    }
}

impl Node {
    fn compile(value: &Value, depth: usize, nodes: &mut usize) -> Result<Self, SchemaError> {
        *nodes += 1;
        if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
            return Err(SchemaError);
        }
        let object = value.as_object().ok_or(SchemaError)?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "items"
                    | "enum"
                    | "minItems"
                    | "maxItems"
                    | "pattern"
            )
        }) {
            return Err(SchemaError);
        }
        let (kind, nullable) = parse_kind(object.get("type").ok_or(SchemaError)?)?;
        if (kind != Kind::Object
            && ["properties", "required", "additionalProperties"]
                .iter()
                .any(|key| object.contains_key(*key)))
            || (kind != Kind::Array
                && ["items", "minItems", "maxItems"]
                    .iter()
                    .any(|key| object.contains_key(*key)))
            || (kind != Kind::String && object.contains_key("pattern"))
        {
            return Err(SchemaError);
        }
        let mut properties = BTreeMap::new();
        if let Some(value) = object.get("properties") {
            for (name, schema) in value.as_object().ok_or(SchemaError)? {
                if reserved_key(name) {
                    return Err(SchemaError);
                }
                properties.insert(name.clone(), Self::compile(schema, depth + 1, nodes)?);
            }
        }
        let mut required = BTreeSet::new();
        if let Some(value) = object.get("required") {
            for name in value.as_array().ok_or(SchemaError)? {
                let name = name.as_str().ok_or(SchemaError)?;
                if reserved_key(name) || !required.insert(name.to_owned()) {
                    return Err(SchemaError);
                }
            }
        }
        let additional = object
            .get("additionalProperties")
            .map_or(Ok(true), |value| value.as_bool().ok_or(SchemaError))?;
        let items = object
            .get("items")
            .map(|schema| Self::compile(schema, depth + 1, nodes).map(Box::new))
            .transpose()?;
        let enumeration = object
            .get("enum")
            .map(|value| compile_enum(value, kind, nullable))
            .transpose()?;
        let min_items = object
            .get("minItems")
            .map(item_bound)
            .transpose()?
            .unwrap_or(0);
        let max_items = object.get("maxItems").map(item_bound).transpose()?;
        if max_items.is_some_and(|max| max < min_items) {
            return Err(SchemaError);
        }
        let token_pattern = match object.get("pattern") {
            None => false,
            Some(Value::String(pattern)) if pattern == TOKEN_PATTERN => true,
            Some(_) => return Err(SchemaError),
        };
        Ok(Self {
            kind,
            nullable,
            properties,
            required,
            additional,
            items,
            enumeration,
            min_items,
            max_items,
            token_pattern,
        })
    }

    fn accepts(&self, value: &Value) -> bool {
        if self
            .enumeration
            .as_ref()
            .is_some_and(|choices| !choices.iter().any(|choice| enum_equal(choice, value)))
        {
            return false;
        }
        if value.is_null() && self.nullable {
            return true;
        }
        match self.kind {
            Kind::Object => value.as_object().is_some_and(|object| {
                self.required.iter().all(|key| object.contains_key(key))
                    && object.iter().all(|(key, value)| {
                        self.properties
                            .get(key)
                            .map_or(self.additional, |schema| schema.accepts(value))
                    })
            }),
            Kind::Array => value.as_array().is_some_and(|values| {
                values.len() >= self.min_items
                    && self.max_items.is_none_or(|max| values.len() <= max)
                    && self
                        .items
                        .as_ref()
                        .is_none_or(|schema| values.iter().all(|value| schema.accepts(value)))
            }),
            Kind::String => value
                .as_str()
                .is_some_and(|value| !self.token_pattern || valid_token(value)),
            Kind::Integer => value
                .as_number()
                .is_some_and(|number| integer_parts(number).is_some()),
            Kind::Number => value.is_number(),
            Kind::Boolean => value.is_boolean(),
            Kind::Null => value.is_null(),
        }
    }
}

fn compile_enum(value: &Value, kind: Kind, nullable: bool) -> Result<Vec<Value>, SchemaError> {
    let values = value
        .as_array()
        .filter(|values| !values.is_empty())
        .ok_or(SchemaError)?;
    let mut choices = Vec::new();
    for value in values {
        let supported = match value {
            Value::Null => nullable || kind == Kind::Null,
            Value::String(_) => kind == Kind::String,
            Value::Bool(_) => kind == Kind::Boolean,
            Value::Number(_) => {
                matches!(kind, Kind::Integer | Kind::Number) && safe_integer(value).is_some()
            }
            _ => false,
        };
        if !supported {
            return Err(SchemaError);
        }
        if choices.iter().any(|choice| enum_equal(choice, value)) {
            return Err(SchemaError);
        }
        choices.push(value.clone());
    }
    Ok(choices)
}

fn parse_kind(value: &Value) -> Result<(Kind, bool), SchemaError> {
    let (name, nullable) = match value {
        Value::String(name) => (name.as_str(), false),
        Value::Array(types) if types.len() == 2 => {
            let nulls = types
                .iter()
                .filter(|value| value.as_str() == Some("null"))
                .count();
            if nulls != 1 {
                return Err(SchemaError);
            }
            (
                types
                    .iter()
                    .find_map(|value| value.as_str().filter(|name| *name != "null"))
                    .ok_or(SchemaError)?,
                true,
            )
        }
        _ => return Err(SchemaError),
    };
    let kind = match name {
        "object" => Kind::Object,
        "array" => Kind::Array,
        "string" => Kind::String,
        "integer" => Kind::Integer,
        "number" => Kind::Number,
        "boolean" => Kind::Boolean,
        "null" => Kind::Null,
        _ => return Err(SchemaError),
    };
    Ok((kind, nullable))
}

fn item_bound(value: &Value) -> Result<usize, SchemaError> {
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value <= MAX_OUTPUT_NODES)
        .ok_or(SchemaError)
}

fn safe_integer(value: &Value) -> Option<i64> {
    let (negative, digits, zeroes) = integer_parts(value.as_number()?)?;
    if digits.len().checked_add(zeroes)? > 16 {
        return None;
    }
    let magnitude = digits
        .parse::<i64>()
        .ok()?
        .checked_mul(10_i64.checked_pow(u32::try_from(zeroes).ok()?)?)?;
    let value = if negative { -magnitude } else { magnitude };
    (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER)
        .contains(&value)
        .then_some(value)
}

// Normalize the original decimal representation without f64 rounding. This
// also handles arbitrary_precision, enabled by the embedded Core dependency.
fn integer_parts(number: &serde_json::Number) -> Option<(bool, String, usize)> {
    let text = number.to_string();
    let (mantissa, exponent) =
        text.split_once(['e', 'E'])
            .map_or((text.as_str(), 0), |(mantissa, exponent)| {
                let exponent = exponent.parse::<i64>().unwrap_or_else(|_| {
                    if exponent.starts_with('-') {
                        i64::MIN
                    } else {
                        i64::MAX
                    }
                });
                (mantissa, exponent)
            });
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits = mantissa.replace('.', "");
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Some((false, "0".to_owned(), 0));
    }
    let shift = exponent.saturating_sub(i64::try_from(fraction).ok()?);
    if shift < 0 {
        let remove = usize::try_from(shift.checked_neg()?).ok()?;
        if digits.len() - digits.trim_end_matches('0').len() < remove {
            return None;
        }
        digits.truncate(digits.len() - remove);
        Some((negative, digits, 0))
    } else {
        Some((
            negative,
            digits,
            usize::try_from(shift).unwrap_or(usize::MAX),
        ))
    }
}

fn enum_equal(left: &Value, right: &Value) -> bool {
    if left.is_number() || right.is_number() {
        return safe_integer(left)
            .zip(safe_integer(right))
            .is_some_and(|(left, right)| left == right);
    }
    left == right
}

fn valid_token(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._:/@-".contains(&byte))
}

fn bounded(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    *nodes += 1;
    if depth > MAX_OUTPUT_DEPTH || *nodes > MAX_OUTPUT_NODES {
        return false;
    }
    match value {
        Value::Array(values) => values.iter().all(|value| bounded(value, depth + 1, nodes)),
        Value::Object(values) => values
            .values()
            .all(|value| bounded(value, depth + 1, nodes)),
        _ => true,
    }
}

// First validate key uniqueness without interpreting serde's synthetic numeric
// maps. The second Value parse retains the dependency's exact numeric semantics.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = UniqueKeys;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueKeys)
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                while sequence.next_element::<UniqueKeys>()?.is_some() {}
                Ok(UniqueKeys)
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    map.next_value::<UniqueKeys>()?;
                }
                Ok(UniqueKeys)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn reserved_key(key: &str) -> bool {
    key.starts_with("$serde_json::private::")
}

// Value's arbitrary_precision/raw_value decoders treat certain real object keys
// as private scalar markers. Inspect the original JSON tokens so synthetic maps
// produced for legitimate numbers cannot be confused with input object maps.
pub(crate) fn reserved_object_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => index += 2,
                b'"' => break,
                _ => index += 1,
            }
        }
        index += 1;
        if index > bytes.len() {
            return true;
        }
        let end = index;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) == Some(&b':') {
            let Ok(key) = serde_json::from_str::<String>(&text[start..end]) else {
                return true;
            };
            if reserved_key(&key) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_subset_validates_nested_verdict_and_exact_json() {
        let schema = ResponseSchema::compile(&json!({"type":"object","additionalProperties":false,"required":["findings"],"properties":{"findings":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["verdict"],"properties":{"verdict":{"type":"string","enum":["pass","fail"]}}}}}})).unwrap();
        assert!(schema.accepts(r#"{"findings":[{"verdict":"pass"}]}"#));
        for text in [
            "{}",
            r#"{"findings":{},"extra":1}"#,
            r#"{"findings":[{"verdict":"unknown"}]}"#,
            r#"{"findings":[{"verdict":"pass","extra":1}]}"#,
            r#"{"findings":[{"verdict":"fail","verdict":"pass"}]}"#,
            "```json\n{}\n```",
            "{} trailing",
            "{",
        ] {
            assert!(!schema.accepts(text), "invalid output was accepted");
        }
    }

    #[test]
    fn schema_subset_preserves_basic_types_and_default_additional_properties() {
        for (kind, accepted, rejected) in [
            (
                "integer",
                vec!["1", "1.0", "1e0", "-2.0", "9007199254740993"],
                vec!["1.5", "true", "\"1\""],
            ),
            ("number", vec!["1.5", "1e2"], vec!["null", "\"1\""]),
            ("boolean", vec!["true", "false"], vec!["0", "\"true\""]),
            ("null", vec!["null"], vec!["false", "0"]),
            ("array", vec!["[]", "[1,true]"], vec!["{}"]),
        ] {
            let schema = ResponseSchema::compile(&json!({"type":kind})).unwrap();
            for value in accepted {
                assert!(schema.accepts(value));
            }
            for value in rejected {
                assert!(!schema.accepts(value));
            }
        }
        let schema = ResponseSchema::compile(
            &json!({"type":"object","properties":{"known":{"type":"string"}}}),
        )
        .unwrap();
        assert!(schema.accepts(r#"{"extra":1}"#));
        assert!(!schema.accepts(r#"{"known":1}"#));
    }

    #[test]
    fn schema_subset_rejects_unknown_or_unsupported_semantics() {
        for schema in [
            json!({"type":"string","pattern":"a"}),
            json!({"type":["string","integer"]}),
            json!({"type":"object","additionalProperties":{"type":"string"}}),
            json!({"type":"integer","enum":[9_007_199_254_740_993_u64,9_007_199_254_740_992.0]}),
            json!({"type":"string","enum":["pass",1]}),
            json!({"type":"string","enum":[]}),
            json!({"type":"string","enum":["pass","pass"]}),
            json!({"type":"array","uniqueItems":true}),
            json!({"type":"object","required":["a","a"]}),
            json!(true),
        ] {
            assert!(ResponseSchema::compile(&schema).is_err());
        }
    }

    #[test]
    fn schema_subset_supports_project_nullable_types_and_scalar_enums_exactly() {
        let schema =
            ResponseSchema::compile(&json!({"type":["string","null"],"enum":["repair",null]}))
                .unwrap();
        for value in ["null", "\"repair\""] {
            assert!(schema.accepts(value));
        }
        for value in ["false", "0", "\"unknown\""] {
            assert!(!schema.accepts(value));
        }
        let schema = ResponseSchema::compile(&json!({"type":"boolean","enum":[false]})).unwrap();
        assert!(schema.accepts("false"));
        assert!(!schema.accepts("true"));
        let schema = ResponseSchema::compile(
            &json!({"type":"integer","enum":[1,-1,9_007_199_254_740_991_u64]}),
        )
        .unwrap();
        for value in ["1", "1.0", "1e0", "-1.0", "9007199254740991"] {
            assert!(schema.accepts(value));
        }
        for value in [
            "1.5",
            "true",
            "9007199254740992",
            "9007199254740993",
            "9007199254740992.0",
            "-9007199254740993",
        ] {
            assert!(!schema.accepts(value));
        }
        for value in [
            json!({"type":"integer","enum":[1,1.0]}),
            json!({"type":"number","enum":[1.5]}),
            json!({"type":["null","null"]}),
            json!({"type":["string","null","integer"]}),
        ] {
            assert!(ResponseSchema::compile(&value).is_err());
        }
    }

    #[test]
    fn schema_subset_rejects_real_private_marker_objects_and_preserves_decimal_numbers() {
        let integer = ResponseSchema::compile(&json!({"type":"integer"})).unwrap();
        for text in [
            r#"{"$serde_json::private::Number":"1"}"#,
            r#"{"\u0024serde_json::private::Number":"1"}"#,
            r#"{"$serde_json::private::RawValue":"1"}"#,
            "1.0000000000000001",
            "1e-400",
        ] {
            assert!(
                !integer.accepts(text),
                "object or fractional value was accepted as integer"
            );
        }
        for text in ["1.0", "1e0", "1.200e1", "0.000e-400", "9007199254740993"] {
            assert!(integer.accepts(text));
        }
        let object = ResponseSchema::compile(&json!({"type":"object","required":["value"],"properties":{"value":{"type":"integer"}}})).unwrap();
        for text in [
            r#"{"value":{"$serde_json::private::Number":"1"}}"#,
            r#"{"value":{"\u0024serde_json::private::Number":"1"}}"#,
            r#"{"value":1,"value":2}"#,
            r#"{"value":1,"extra":{"x":1,"x":2}}"#,
        ] {
            assert!(!object.accepts(text));
        }
        assert!(object.accepts(r#"{"value":1e0,"extra":"$serde_json::private::Number"}"#));
        let enumeration = ResponseSchema::compile(&json!({"type":"integer","enum":[1]})).unwrap();
        assert!(!enumeration.accepts("1.0000000000000001"));
        assert!(enumeration.accepts("1e0"));
        for schema in [
            json!({"type":"object","properties":{"$serde_json::private::Number":{"type":"integer"}}}),
            json!({"type":"object","required":["$serde_json::private::RawValue"]}),
        ] {
            assert!(ResponseSchema::compile(&schema).is_err());
        }
    }

    #[test]
    fn schema_subset_enforces_project_array_bounds_and_ascii_token_pattern() {
        let schema = ResponseSchema::compile(&json!({"type":"array","minItems":1,"maxItems":2,"items":{"type":"string","pattern":TOKEN_PATTERN}})).unwrap();
        for value in [r#"["A"]"#, r#"["a.b_c:d/e@f-g","0"]"#] {
            assert!(schema.accepts(value));
        }
        for value in [
            "[]",
            r#"["a","b","c"]"#,
            r#"[""]"#,
            r#"[".abc"]"#,
            r#"["a b"]"#,
            r#"["中文"]"#,
            "[\"a\\n\"]",
        ] {
            assert!(!schema.accepts(value));
        }
        for value in [
            json!({"type":"array","minItems":2,"maxItems":1}),
            json!({"type":"array","minItems":-1}),
            json!({"type":"array","maxItems":1.5}),
            json!({"type":"object","minItems":1}),
        ] {
            assert!(ResponseSchema::compile(&value).is_err());
        }
    }

    fn fixed_product_examples() -> ([&'static str; 6], [Value; 6]) {
        let sources = [
            include_str!("../tests/fixtures/response-schema/change-batch-proposal.schema.json"),
            include_str!("../tests/fixtures/response-schema/planner-solution.schema.json"),
            include_str!("../tests/fixtures/response-schema/verification-result.schema.json"),
            include_str!(
                "../tests/fixtures/response-schema/fusion-verification-result.schema.json"
            ),
            include_str!("../tests/fixtures/response-schema/fusion-default-claims.schema.json"),
            include_str!("../tests/fixtures/response-schema/observation-response.schema.json"),
        ];
        let node = json!({"id":"component","kind":"component","label":"Component","description":"Public fixture","trustBoundary":null,"unresolved":false});
        let verification = json!({"protocol":"winwincode.independent-verification-result.v1","delivery_spec_id":"spec","delivery_spec_revision":1,"candidate_ref":"candidate","findings":[{"finding_id":"finding","criterion_id":"criterion","verdict":"pass","explanation":"Public fixture","evidence_sources":[{"source_id":"source"}]}]});
        let mut fusion_verification = verification.clone();
        fusion_verification["fusion_investigations"] = json!([{"claim_key":"claim","status":"investigated","evidence_sources":[{"source_id":"source"}]}]);
        let outputs = [
            json!({"schemaVersion":1,"disposition":"final","patch":"public fixture","acceptanceCriteriaIds":["criterion-1"],"validationProfile":"profile/default"}),
            json!({"schemaVersion":1,"protocol":"winwincode.planner-solution.v1","solution":{"id":"solution","summary":"Public fixture","approach":["inspect"],"components":[{"id":"component","kind":"component","label":"Component","responsibility":"Public fixture","repositoryPathPrefixes":["src/"],"trustBoundary":null,"unresolved":false}],"connections":[{"id":"connection","from":"component","to":"component","label":"Local"}]},"architectureDiagram":{"id":"architecture","kind":"system-architecture","title":"Architecture","nodes":[node.clone()],"edges":[]},"processDiagram":{"id":"process","kind":"process-flow","title":"Process","nodes":[node],"edges":[]},"taskProposals":[{"id":"task","title":"Task","goal":"Public fixture","acceptanceCriterionIds":["criterion"],"blockedByTaskIds":[]}],"unresolvedItems":[],"risks":[]}),
            verification,
            fusion_verification,
            json!({"claims":[{"claimKey":"claim","summary":"Public fixture","position":"supports","evidence":[{"evidenceType":"test","sourceRef":"source"}],"requiredEvidence":["test"]}]}),
            json!({"schemaVersion":1,"observationId":"observation","decision":"accept","reasonCode":"criteria_satisfied","summary":"Public fixture","rootCauses":[],"repairClass":null,"confidenceBps":10000}),
        ];
        (sources, outputs)
    }

    #[test]
    fn fixed_product_schemas_compile_and_enforce_their_existing_contracts() {
        let (sources, outputs) = fixed_product_examples();
        for (index, (source, output)) in sources.iter().zip(outputs.iter()).enumerate() {
            let raw: Value = serde_json::from_str(source).unwrap();
            let schema = ResponseSchema::compile(&raw).unwrap();
            assert!(schema.is_object());
            assert!(schema.accepts(&output.to_string()), "fixed schema {index}");
            let mut missing = output.clone();
            missing
                .as_object_mut()
                .unwrap()
                .remove(raw["required"][0].as_str().unwrap());
            assert!(
                !schema.accepts(&missing.to_string()),
                "missing required {index}"
            );
            let mut extra = output.clone();
            extra["unexpected"] = json!(true);
            assert!(
                !schema.accepts(&extra.to_string()),
                "additional property {index}"
            );
        }
        let schemas = sources
            .iter()
            .map(|source| ResponseSchema::compile(&serde_json::from_str(source).unwrap()).unwrap())
            .collect::<Vec<_>>();
        for (pointer, invalid) in [
            ("/schemaVersion", json!(2)),
            ("/acceptanceCriteriaIds", json!([])),
            ("/validationProfile", json!("bad token")),
            ("/acceptanceCriteriaIds", json!(vec!["criterion"; 257])),
        ] {
            let mut output = outputs[0].clone();
            *output.pointer_mut(pointer).unwrap() = invalid;
            assert!(!schemas[0].accepts(&output.to_string()));
        }
        let mut output = outputs[1].clone();
        *output
            .pointer_mut("/solution/components/0/trustBoundary")
            .unwrap() = json!(false);
        assert!(!schemas[1].accepts(&output.to_string()));
        *output
            .pointer_mut("/solution/components/0/trustBoundary")
            .unwrap() = json!("local");
        assert!(schemas[1].accepts(&output.to_string()));
        let mut output = outputs[2].clone();
        *output.pointer_mut("/findings/0/verdict").unwrap() = json!("unknown");
        assert!(!schemas[2].accepts(&output.to_string()));
        let mut output = outputs[3].clone();
        *output
            .pointer_mut("/fusion_investigations/0/evidence_sources")
            .unwrap() = json!([]);
        assert!(!schemas[3].accepts(&output.to_string()));
        let mut output = outputs[4].clone();
        *output
            .pointer_mut("/claims/0/evidence/0/evidenceType")
            .unwrap() = json!("unknown");
        assert!(!schemas[4].accepts(&output.to_string()));
        let mut output = outputs[5].clone();
        output["repairClass"] = json!("targeted_patch");
        assert!(schemas[5].accepts(&output.to_string()));
        output["repairClass"] = json!("unknown");
        assert!(!schemas[5].accepts(&output.to_string()));
    }

    #[test]
    fn schema_subset_bounds_schema_and_final_output() {
        let mut schema = json!({"type":"string"});
        for _ in 0..=MAX_SCHEMA_DEPTH {
            schema = json!({"type":"array","items":schema});
        }
        assert!(ResponseSchema::compile(&schema).is_err());
        let schema = ResponseSchema::compile(&json!({"type":"object"})).unwrap();
        assert!(!schema.accepts(&format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_OUTPUT_BYTES))));
        let mut value = "null".to_owned();
        for _ in 0..=MAX_OUTPUT_DEPTH {
            value = format!("[{value}]");
        }
        assert!(
            !ResponseSchema::compile(&json!({"type":"array"}))
                .unwrap()
                .accepts(&value)
        );
    }
}
