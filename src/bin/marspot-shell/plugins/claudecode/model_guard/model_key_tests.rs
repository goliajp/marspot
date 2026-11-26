use super::*;

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

const LAID_OUT: &str = "{\n  \"env\": {\"A\": \"}{,\"},\n  \"model\": \"fable\",\n  \"hooks\": [1, {\"x\": [2]}],\n  \"theme\": \"dark\"\n}\n";

#[test]
fn a_model_is_changed_in_place_and_every_other_byte_stays() {
    let back = with_model(LAID_OUT, Some(&s("opus"))).unwrap();
    assert_eq!(back, LAID_OUT.replace("\"fable\"", "\"opus\""));
}

#[test]
fn a_model_is_taken_out_with_its_line_and_one_comma() {
    assert_eq!(
        with_model(LAID_OUT, None).unwrap(),
        "{\n  \"env\": {\"A\": \"}{,\"},\n  \"hooks\": [1, {\"x\": [2]}],\n  \"theme\": \"dark\"\n}\n"
    );
    assert_eq!(with_model("{\n  \"a\": 1,\n  \"model\": \"x\"\n}\n", None).unwrap(), "{\n  \"a\": 1\n}\n");
    assert_eq!(with_model("{\n  \"model\": \"x\"\n}\n", None).unwrap(), "{\n}\n");
    assert_eq!(with_model("{\"model\":\"x\", \"a\":1}", None).unwrap(), "{\"a\":1}");
    assert_eq!(with_model("{\"a\":1,\"model\":\"x\"}", None).unwrap(), "{\"a\":1}");
    assert_eq!(with_model("{\"model\":\"x\"}", None).unwrap(), "{}");
}

#[test]
fn a_model_is_put_in_first_laid_out_as_the_next_member() {
    assert_eq!(with_model("{\n  \"theme\": \"dark\"\n}\n", Some(&s("opus"))).unwrap(), "{\n  \"model\": \"opus\",\n  \"theme\": \"dark\"\n}\n");
    assert_eq!(with_model("{\"theme\":1}", Some(&s("opus"))).unwrap(), "{\"model\": \"opus\",\"theme\":1}");
    assert_eq!(with_model("{}\n", Some(&s("opus"))).unwrap(), "{\n  \"model\": \"opus\"\n}\n");
}

#[test]
fn nothing_to_change_is_the_same_text_and_what_cannot_be_edited_is_refused() {
    assert_eq!(with_model("{\"a\": 1}", None).unwrap(), "{\"a\": 1}");
    for bad in ["", "{\"model\": \"op", "[1]", "{\"a\" 1}"] {
        assert_eq!(with_model(bad, Some(&s("opus"))), None, "{bad:?}");
    }
    // a key spelled with an escape is model to a parser but not to the cut
    assert_eq!(with_model("{\"mod\\u0065l\": \"x\", \"a\": 1}", None), None);
}

#[test]
fn a_model_that_is_not_a_string_goes_back_as_it_was() {
    let v = Value::Obj(vec![("id".into(), s("opus"))]);
    assert_eq!(with_model("{\"model\": \"x\"}", Some(&v)).unwrap(), "{\"model\": {\"id\":\"opus\"}}");
}

#[test]
fn a_text_with_two_model_keys_is_never_edited() {
    for two in ["{\"model\": \"a\", \"model\": \"b\"}", "{\"model\": \"a\", \"mod\\u0065l\": \"b\", \"x\": 1}"] {
        assert!(has_two_models(two), "{two:?}");
        assert_eq!(with_model(two, Some(&s("opus"))), None, "{two:?}");
        assert_eq!(with_model(two, None), None, "{two:?}");
    }
    assert!(!has_two_models(LAID_OUT));
    assert!(!has_two_models("{\"models\": 1, \"model\": 2}"));
}
