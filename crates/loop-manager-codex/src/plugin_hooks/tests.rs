use super::*;

#[test]
fn preference_replaces_the_default_including_with_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.toml");
    assert_eq!(selection(&path).unwrap(), ["t3-run-card@loop-manager"]);
    for ids in [vec![], vec!["other@catalog"], vec!["*"]] {
        std::fs::write(&path, format!("[codex.plugin_hooks]\nauto_trust_updates = {ids:?}")).unwrap();
        assert_eq!(selection(&path).unwrap(), ids);
    }
}

#[test]
fn renews_only_modified_enabled_hooks_of_selected_plugins() {
    let hook = json!({"pluginId":"card@lm","source":"plugin","trustStatus":"modified","enabled":true,
        "isManaged":false,"key":"card@lm:hooks/hooks.json:post_tool_use:0:0","currentHash":"a/b"});
    for id in ["card@lm", "*"] {
        let selected = [id.to_owned()];
        assert_eq!(approval(&hook, &selected).unwrap()["value"], "a/b");
        for (key, value) in [
            ("source", json!("sessionFlags")),
            ("enabled", json!(false)),
            ("trustStatus", json!("untrusted")),
            ("trustStatus", json!("trusted")),
            ("isManaged", json!(true)),
        ] {
            let mut skipped = hook.clone();
            skipped[key] = value;
            assert!(approval(&skipped, &selected).is_none());
        }
    }
    assert!(approval(&hook, &["other@lm".into()]).is_none());
}
