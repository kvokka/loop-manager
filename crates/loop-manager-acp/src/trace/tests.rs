use super::*;

fn secrets(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn a_secret_is_replaced_in_every_string_of_the_message_and_nothing_else_is() {
    let line = r#"{"jsonrpc":"2.0","id":1,"params":{"env":[{"name":"KEY","value":"s3cret"}],"text":"a s3cret b s3cret","n":7}}"#;
    let msg = redact(line, &secrets(&["s3cret"]));
    assert_eq!(
        msg,
        json!({ "jsonrpc": "2.0", "id": 1, "params": { "env": [{ "name": "KEY", "value": "[redacted]" }],
            "text": "a [redacted] b [redacted]", "n": 7 } })
    );
}

#[test]
fn a_secret_escaped_in_the_json_line_is_still_found() {
    let line = json!({ "value": "say \"hi\"\n" }).to_string();
    let msg = redact(&line, &secrets(&["\"hi\"\n"]));
    assert_eq!(msg, json!({ "value": "say [redacted]" }));
}

#[test]
fn a_line_that_is_no_json_is_kept_as_a_string_redacted() {
    let msg = redact("not json: tok-1", &secrets(&["tok-1"]));
    assert_eq!(msg, json!("not json: [redacted]"));
}

#[test]
fn the_longest_secret_wins_and_an_empty_one_redacts_nothing() {
    let msg = redact(r#""abcdef red""#, &secrets(&["", "abc", "abcdef", "red"]));
    // `red` is not looked for inside the `[redacted]` that replaced `abcdef`.
    assert_eq!(msg, json!("[redacted] [redacted]"));
    let msg = redact(r#""plain""#, &secrets(&[""]));
    assert_eq!(msg, json!("plain"));
}

#[test]
fn the_values_of_mcp_env_and_headers_are_secrets() {
    let part = json!({ "mcpServers": [
        { "type": "stdio", "name": "a", "command": "/bin/a", "args": [], "env": [{ "name": "K", "value": "v1" }] },
        { "type": "http", "name": "b", "url": "http://x", "headers": [{ "name": "Authorization", "value": "Bearer t" }] },
    ] });
    assert_eq!(mcp_secrets(&part), secrets(&["v1", "Bearer t"]));
}
