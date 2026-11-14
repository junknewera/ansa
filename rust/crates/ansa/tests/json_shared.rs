use ansa::protocol::json::Json;

#[test]
fn clones_and_replacements_preserve_owned_values_and_number_tokens() {
    let original = Json::parse(r#"{"opaque":"\u001f\"\\é😀","tokens":[1,1.0,1E+0,-0]}"#).unwrap();
    let clone = original.clone();
    let changed = clone
        .with("opaque", Json::string("replacement").unwrap())
        .unwrap();
    assert_eq!(original.text_field("opaque").unwrap(), "\u{1f}\"\\é😀");
    assert_eq!(changed.text_field("opaque").unwrap(), "replacement");
    assert_eq!(original.field("tokens").unwrap().dump(), "[1,1.0,1E+0,-0]");
    drop(original);
    assert_eq!(
        clone.field("tokens").unwrap(),
        changed.field("tokens").unwrap()
    );
    assert_eq!(Json::parse(&clone.dump()).unwrap(), clone);
}

#[test]
fn escaped_byte_bounds_and_constructor_duplicate_keys_stay_strict() {
    let text = "\u{1f}\"\n😀";
    let serialized = "\"\\u001F\\\"\\n😀\"";
    assert_eq!(
        Json::string_with_limits(text, serialized.len())
            .unwrap()
            .dump(),
        serialized
    );
    assert_eq!(
        Json::string_with_limits(text, serialized.len() - 1).unwrap_err(),
        "json_build_limit"
    );
    assert_eq!(
        Json::object_with_limits(vec![], 1).unwrap_err(),
        "json_build_limit"
    );
    assert_eq!(
        Json::object(vec![
            ("a", Json::parse("0").unwrap()),
            ("\u{61}", Json::parse("1").unwrap())
        ])
        .unwrap_err(),
        "json_duplicate_key"
    );
}

#[test]
fn shared_subtrees_count_logical_nodes_and_depth_without_budget_escape() {
    let mut value = Json::parse("0").unwrap();
    for _ in 0..16 {
        value = Json::array(vec![value.clone(), value]).unwrap();
    }
    let exact = Json::object(vec![("payload", value.clone())]).unwrap(); // 131072 serialized nodes.
    assert_eq!(
        Json::array(vec![exact]).unwrap_err(),
        "json_structure_limit"
    );
    assert_eq!(
        Json::array(vec![value.clone(), value]).unwrap_err(),
        "json_structure_limit"
    );
    let mut nested = Json::parse("null").unwrap();
    for _ in 0..63 {
        nested = Json::array(vec![nested]).unwrap();
    }
    assert_eq!(
        Json::array(vec![nested.clone()]).unwrap_err(),
        "json_depth_limit"
    );
    assert_eq!(
        Json::object(vec![("a", nested.clone()), ("a", nested)]).unwrap_err(),
        "json_depth_limit"
    );
}
