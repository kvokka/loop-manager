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
