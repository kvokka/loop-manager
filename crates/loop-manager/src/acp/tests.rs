use super::project_of;
use serde_json::json;
use std::path::Path;

#[test]
fn the_project_of_a_cwd_is_the_one_whose_place_holds_it_most_closely() {
    let projects = json!([
        { "name": "outer", "dir": "/w", "root": { "path": "." }, "rigs": [] },
        { "name": "inner", "dir": "/v", "root": { "path": "." }, "rigs": [{ "path": "/w/inner" }] },
    ]);
    let of = |cwd: &str| project_of(&projects, Path::new(cwd));
    assert_eq!(of("/w/inner/src").as_deref(), Some("inner"));
    assert_eq!(of("/w/other").as_deref(), Some("outer"));
    assert_eq!(of("/x"), None);
}

#[test]
fn a_command_is_its_operation_and_body_and_any_other_text_an_instruction() {
    use super::parse;
    assert_eq!(parse("/lm-takeover"), Ok(Some(("takeover", json!({})))));
    assert_eq!(
        parse("/lm-continue withdraw"),
        Ok(Some(("continue", json!({ "unsent": "withdraw" }))))
    );
    assert_eq!(
        parse(" /lm-delivered "),
        Ok(Some(("delivery", json!({ "choice": "delivered" }))))
    );
    assert_eq!(
        parse("/lm-resend"),
        Ok(Some(("delivery", json!({ "choice": "resend" }))))
    );
    assert_eq!(parse("resend the tests"), Ok(None));
    assert!(
        parse("/lm-resend twice")
            .unwrap_err()
            .contains("`/lm-resend` takes no `twice`")
    );
}
