use super::*;

#[tokio::test]
async fn a_key_creates_one_issue_whose_parent_is_released_while_a_child_is_open() {
    let t = MemoryTracker::new();
    let root = t.create("loop-manager:r1", "k1", "Ship it", "text").await.unwrap();
    assert_eq!(t.create("loop-manager:r1", "k1", "Other", "").await.unwrap(), root);
    assert_ne!(t.create("loop-manager:r1", "k2", "Two", "").await.unwrap().id, root.id);
    let child = t.add(&[], Some(&root.id));
    t.claim("loop-manager:r1", &root.id).await.unwrap();
    assert!(t.ready().await.unwrap().iter().any(|b| b.id == child));
    t.close("loop-manager:r1", &root.id, "passed").await.unwrap();
    let root = t.issue(&root.id).unwrap();
    assert_eq!((root.status.as_str(), root.assignee), ("open", None));
    assert!(t.ready().await.unwrap().iter().all(|b| b.id != root.id));
}

#[tokio::test]
async fn under_gives_the_ready_issues_at_any_depth_below_the_root() {
    let t = MemoryTracker::new();
    let (root, other) = (t.add(&[], None), t.add(&[], None));
    let epic = t.add(&[], Some(&root));
    let (kid, outside) = (t.add(&[], Some(&epic)), t.add(&[], Some(&other)));
    assert_eq!(t.under(&root).await.unwrap(), [kid]);
    assert_eq!(t.under(&other).await.unwrap(), [outside]);
    assert_eq!(t.under("b-404").await, Err(TrackerError::NotFound));
}

#[tokio::test]
async fn waiting_gives_the_open_issues_under_the_root_held_by_a_blocker_of_their_line() {
    let t = MemoryTracker::new();
    let (above, outside) = (t.add(&[], None), t.add(&[], None));
    let root = t.add(&[], Some(&above));
    let (epic, sibling) = (t.add(&[], Some(&root)), t.add(&[], Some(&root)));
    let (kid, done) = (t.add(&[], Some(&epic)), t.add(&[], Some(&epic)));
    t.depend(&epic, &outside, "blocks");
    t.depend(&sibling, &kid, "waits-for");
    t.set_status(&done, "closed");
    let wait = |issue: &str, blocker: &str, kind: &str, under: bool| Wait {
        issue: issue.to_owned(),
        blocker: blocker.to_owned(),
        kind: kind.to_owned(),
        status: "open".to_owned(),
        under,
    };
    let expected = [
        wait(&epic, &outside, "blocks", false),
        wait(&sibling, &kid, "waits-for", true),
        wait(&kid, &outside, "blocks", false),
    ];
    assert_eq!(t.waiting(&root).await.unwrap(), expected);
    assert!(t.ready().await.unwrap().iter().all(|b| b.id != kid && b.id != sibling));
    t.set_status(&above, "blocked");
    let held = t.waiting(&root).await.unwrap();
    let by_status: Vec<(&str, &str)> = (held.iter().filter(|w| w.kind == "status"))
        .map(|w| (w.issue.as_str(), w.status.as_str()))
        .collect();
    assert_eq!(
        by_status,
        [
            (epic.as_str(), "blocked"),
            (sibling.as_str(), "blocked"),
            (kid.as_str(), "blocked")
        ]
    );
    t.set_status(&outside, "closed");
    assert!(t.waiting(&root).await.unwrap().iter().all(|w| w.blocker != outside));
    assert_eq!(t.waiting("b-404").await, Err(TrackerError::NotFound));
}

#[tokio::test]
async fn releasing_an_issue_closed_by_someone_else_clears_its_owner_and_keeps_it_closed() {
    let t = MemoryTracker::new();
    let id = t.add(&[], None);
    t.claim("loop-manager:r1", &id).await.unwrap();
    t.close_as(&id, "human");
    t.release("loop-manager:r1", &id).await.unwrap();
    let issue = t.issue(&id).unwrap();
    assert_eq!((issue.status.as_str(), issue.assignee), ("closed", None));
    assert!(t.ready().await.unwrap().is_empty());
}

#[tokio::test]
async fn file_takes_its_fields_once_per_key_and_needs_its_parent() {
    let t = MemoryTracker::new();
    let root = t.add(&[], None);
    let new = NewIssue {
        title: "Rig work".into(),
        description: "text".into(),
        labels: vec!["x".into()],
        parent: Some(root.clone()),
        prefix: Some("api".into()),
    };
    let filed = t.file("loop-manager:r1", Some("k1"), &new).await.unwrap();
    assert!(filed.id.starts_with("api-"), "{}", filed.id);
    assert_eq!(
        (filed.epic.as_ref(), filed.labels.as_slice()),
        (Some(&root), ["x".to_owned()].as_slice())
    );
    let created = t.changes(0).await.unwrap().pop().unwrap();
    assert_eq!(
        (created.kind.as_str(), created.actor.as_str()),
        ("created", "loop-manager:r1")
    );
    assert_eq!(t.file("loop-manager:r2", Some("k1"), &new).await.unwrap(), filed);
    assert_eq!(t.subtree(&root).await.unwrap(), [filed]);
    let plain = NewIssue {
        parent: None,
        prefix: None,
        ..new.clone()
    };
    assert!(
        t.file("loop-manager:r1", None, &plain)
            .await
            .unwrap()
            .id
            .starts_with("b-")
    );
    let orphan = NewIssue {
        parent: Some("b-404".into()),
        ..new
    };
    assert_eq!(
        t.file("loop-manager:r1", None, &orphan).await,
        Err(TrackerError::NotFound)
    );
}

#[tokio::test]
async fn subtree_gives_every_issue_below_the_root_closed_ones_too() {
    let t = MemoryTracker::new();
    let (root, other) = (t.add(&[], None), t.add(&[], None));
    let epic = t.add(&[], Some(&root));
    let (kid, _) = (t.add(&[], Some(&epic)), t.add(&[], Some(&other)));
    t.set_status(&epic, "closed");
    let ids: Vec<String> = t.subtree(&root).await.unwrap().into_iter().map(|b| b.id).collect();
    assert_eq!(ids, [epic, kid]);
    assert_eq!(t.subtree("b-404").await, Err(TrackerError::NotFound));
    t.set_down(true);
    assert!(matches!(t.subtree(&root).await, Err(TrackerError::Unavailable { .. })));
}

#[tokio::test]
async fn close_by_closes_whoever_holds_it_once_and_not_a_parent_with_open_children() {
    let t = MemoryTracker::new();
    let parent = t.add(&[], None);
    let (held, open) = (t.add(&[], Some(&parent)), t.add(&[], Some(&parent)));
    t.claim("loop-manager:r1", &held).await.unwrap();
    t.close_by("human", &held, "done").await.unwrap();
    let last = t.changes(0).await.unwrap().pop().unwrap();
    assert_eq!((last.kind.as_str(), last.actor.as_str()), ("closed", "human"));
    assert_eq!(last.issue.assignee.as_deref(), Some("loop-manager:r1"));
    t.close_by("human", &held, "again").await.unwrap();
    assert_eq!(t.changes(last.seq).await.unwrap(), [], "a repeat writes nothing");
    assert!(matches!(
        t.close_by("human", &parent, "early").await,
        Err(TrackerError::Invalid { .. })
    ));
    assert_eq!(t.issue(&parent).unwrap().status, "open");
    assert_eq!(t.issue(&open).unwrap().status, "open");
    assert_eq!(t.close_by("human", "b-404", "x").await, Err(TrackerError::NotFound));
}
