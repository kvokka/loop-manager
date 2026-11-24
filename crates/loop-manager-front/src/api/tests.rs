use super::{Param, runs::ListQuery};
use axum::{extract::FromRequestParts, extract::Query, http::Request, response::IntoResponse};

#[tokio::test]
async fn a_malformed_query_is_an_error_reply() {
    let (mut parts, ()) = Request::get("/v1/runs?limit=many").body(()).unwrap().into_parts();
    let Err(e) = Param::<Query<ListQuery>>::from_request_parts(&mut parts, &()).await else {
        panic!("`limit=many` is accepted");
    };
    let reply = e.into_response();
    assert_eq!(reply.status(), 400);
    let body = axum::body::to_bytes(reply.into_body(), 1 << 16).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "bad_request", "{body}");
}

/// `explain --formula` names the layer of `rounds` and of each input value the config gives, and the `if` of a step
/// (docs/design/config.md).
#[test]
fn a_formula_is_explained_with_the_layers_of_its_rounds_and_input_values() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        (
            "config.toml",
            "projects = [\"shop\"]\nrounds = 2\n[inputs]\nfocus = \"top\"\nn = 1\n",
        ),
        ("shop/loop-manager.toml", "[inputs]\nn = \"4\"\n"),
        (
            "shop/formulas/flow.toml",
            "name = \"flow\"\n[inputs]\nfocus = { type = \"string\" }\nn = { type = \"integer\" }\n\
             [[step]]\nname = \"work\"\nkind = \"exec\"\ncommand = [\"true\"]\n\
             [[step]]\nname = \"review\"\nkind = \"exec\"\ncommand = [\"true\"]\nback_to = \"work\"\n\
             if = { \"inputs.n\" = [4, 5], back_from = \"review\" }\n",
        ),
    ];
    for (path, text) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let config = loop_manager_core::config::load(&dir.path().join("config.toml")).unwrap();
    let of = super::config::Explain::Formula("flow".into());
    let reply = super::config::explained(&config, "shop", &of, None).unwrap();
    let text = reply["text"].as_str().unwrap();
    let head = "rounds = 2  # orchestrator\ninputs.focus = \"top\"  # orchestrator\ninputs.n = 4  # project\n";
    assert!(text.starts_with(head), "{text}");
    let check =
        "# runs only if { \"back_from\" = \"review\", \"inputs.n\" = [4,5] }\n# a check: on fail, back to `work`";
    assert!(text.contains(check), "{text}");
    assert_eq!(text.matches("# runs only if").count(), 1, "{text}");
}

/// `explain --formula` on a rig that sets `max_writers` names it after `rounds`; on the root there is none.
#[test]
fn a_formula_on_a_rig_is_explained_with_its_max_writers() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("config.toml", "projects = [\"shop\"]\n"),
        (
            "shop/loop-manager.toml",
            "[rigs.web]\npath = \"web\"\nmax_writers = 1\n",
        ),
        ("shop/web/.keep", ""),
        (
            "shop/formulas/flow.toml",
            "name = \"flow\"\n[[step]]\nname = \"work\"\nkind = \"exec\"\ncommand = [\"true\"]\n",
        ),
    ];
    for (path, text) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let config = loop_manager_core::config::load(&dir.path().join("config.toml")).unwrap();
    let of = super::config::Explain::Formula("flow".into());
    let on = |rig| super::config::explained(&config, "shop", &of, rig).unwrap()["text"].clone();
    let web = on(Some("web"));
    let web = web.as_str().unwrap();
    assert!(
        web.starts_with("rounds = 3  # default\nmax_writers = 1  # rig\n"),
        "{web}"
    );
    let root = on(None);
    assert!(!root.as_str().unwrap().contains("max_writers"), "{root}");
}

/// `issue show` reads an issue with its comments from the project's tracker; a project the config does not have, one
/// without a tracker, an issue the tracker does not have and a tracker that is down each have their error reply.
#[tokio::test]
async fn an_issue_is_read_from_the_tracker_of_its_project() {
    use loop_manager_core::tracker::Tracker;
    use std::{collections::BTreeMap, sync::Arc};
    let tracker = Arc::new(loop_manager_testkit::MemoryTracker::new());
    let id = tracker.add(&["review"], None);
    tracker.comment(&id, "alice", "first");
    tracker.comment(&id, "bob", "second");
    let trackers = BTreeMap::from([("shop".to_owned(), tracker.clone() as Arc<dyn Tracker>)]);
    let Ok(axum::Json(issue)) = super::issues::read(&trackers, true, "shop", &id).await else {
        panic!("`{id}` is not read");
    };
    assert_eq!(
        (&issue["id"], &issue["status"], &issue["labels"]),
        (&id.clone().into(), &"open".into(), &serde_json::json!(["review"]))
    );
    let comments: Vec<_> = issue["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["author"].clone(), c["text"].clone()))
        .collect();
    assert_eq!(
        comments,
        [("alice".into(), "first".into()), ("bob".into(), "second".into())],
        "{issue}"
    );
    let status = async |project: &str, issue: &str, known: bool| {
        let Err(e) = super::issues::read(&trackers, known, project, issue).await else {
            panic!("`{project}` `{issue}` is read");
        };
        let reply = e.into_response();
        let status = reply.status().as_u16();
        let body = axum::body::to_bytes(reply.into_body(), 1 << 16).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (status, body["error"]["code"].as_str().unwrap().to_owned())
    };
    assert_eq!(status("mall", &id, false).await, (404, "not_found".into()));
    assert_eq!(status("mall", &id, true).await, (409, "rejected".into()));
    assert_eq!(status("shop", "b-none", true).await, (404, "not_found".into()));
    tracker.set_down(true);
    assert_eq!(status("shop", &id, true).await, (503, "tracker_unavailable".into()));
}

/// `issue create`, `issue list` and `issue close` call the project's tracker as the actor they are given: a filed issue
/// takes its labels, parent and rig prefix, the list holds what is under the root, a close takes any holder's issue;
/// a parent or an issue the tracker does not have is `not_found`, a refused close `tracker`.
#[tokio::test]
async fn issues_are_filed_listed_and_closed_through_the_tracker_of_their_project() {
    use loop_manager_core::tracker::{NewIssue, Tracker};
    use std::{collections::BTreeMap, sync::Arc};
    let tracker = Arc::new(loop_manager_testkit::MemoryTracker::new());
    let root = tracker.add(&[], None);
    let trackers = BTreeMap::from([("shop".to_owned(), tracker.clone() as Arc<dyn Tracker>)]);
    let new = |parent: &str| NewIssue {
        title: "Fix the cart".to_owned(),
        description: "The total is wrong".to_owned(),
        labels: vec!["agent:coder".to_owned()],
        parent: Some(parent.to_owned()),
        prefix: Some("w".to_owned()),
    };
    let actor = "loop-manager:run_1:1";
    let Ok(filed) = super::issues::file(&trackers, true, "shop", actor, None, &new(&root)).await else {
        panic!("not filed");
    };
    let id = filed["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("w-"), "{filed}");
    assert_eq!(
        (&filed["epic"], &filed["labels"]),
        (&root.clone().into(), &serde_json::json!(["agent:coder"]))
    );
    let code = |e: super::ApiError| e.into_response().status().as_u16();
    let lost = super::issues::file(&trackers, true, "shop", actor, None, &new("b-none")).await;
    assert_eq!(lost.map_err(code), Err(404));
    let Ok(axum::Json(listed)) = super::issues::under(&trackers, true, "shop", &root).await else {
        panic!("`{root}` is not listed");
    };
    assert_eq!(
        (&listed[0]["id"], &listed[0]["status"]),
        (&id.clone().into(), &"open".into()),
        "{listed}"
    );
    assert_eq!(
        super::issues::under(&trackers, true, "shop", "b-none")
            .await
            .map_err(code)
            .err(),
        Some(404)
    );
    tracker.assign(&id, "loop-manager:run_9");
    // A parent with open children is refused and stays open; its child closes whoever holds it.
    let shut = async |issue: &str| super::issues::shut(&trackers, true, "shop", "loop-manager", issue, "dropped").await;
    assert_eq!(shut(&root).await.map_err(code), Err(502));
    assert_eq!(shut(&id).await.map_err(code), Ok(serde_json::Value::Null));
    assert_eq!(tracker.issue(&id).unwrap().status, "closed");
    assert_eq!(shut(&id).await.map_err(code), Ok(serde_json::Value::Null));
    assert_eq!(shut("b-none").await.map_err(code), Err(404));
}
