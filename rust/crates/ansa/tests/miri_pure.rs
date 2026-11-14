//! Deliberately pure subset: no processes, network, locale, crypto or filesystem.
use ansa::protocol::json::Json;

#[test]
fn owned_json_survives_source_drop_and_preserves_number_lexemes() {
    let value = {
        let source = String::from(r#"{"opaque":"é😀","items":[1,1.0,1e0]}"#);
        Json::parse(&source).unwrap()
    };
    assert_eq!(value.text_field("opaque").unwrap(), "é😀");
    let values = value.field("items").unwrap().elements().unwrap();
    assert_eq!(values.iter().map(Json::dump).collect::<Vec<_>>(), ["1", "1.0", "1e0"]);
}

#[test]
fn frontend_editor_and_controls_remain_owned_and_bounded() {
    use ansa::frontend::input::Editor;
    let mut editor = Editor::default();
    for byte in "Ж😀".as_bytes() {
        editor.feed(*byte, true);
    }
    assert_eq!(editor.text().unwrap(), "Ж😀");
    editor.feed(127, true);
    assert_eq!(editor.text().unwrap(), "Ж");
    assert_eq!(editor.cursor, "Ж".len());
    editor.feed(127, true);
    assert_eq!(editor.text().unwrap(), "");
}
