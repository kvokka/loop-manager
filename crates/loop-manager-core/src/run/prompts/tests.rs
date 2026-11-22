//! The rules of the delivery check (contracts 6.4), one case each.

use super::{Verdict, verdict};
use crate::driver::{Replay, Replayed};

const MARKER: &str = "[Request interrupted by user]";

fn replay(complete: bool, messages: &[(&str, &str)]) -> Replay {
    let messages = messages.iter().map(|(id, text)| Replayed {
        id: (*id).to_owned(),
        text: (*text).to_owned(),
    });
    Replay {
        complete,
        messages: messages.collect(),
    }
}

fn unknown(why: &'static str) -> Verdict {
    Verdict::Unknown { why }
}

#[test]
fn each_rule_of_the_delivery_check_gives_its_verdict() {
    let shown = replay(true, &[("m1", "Build web."), ("m2", "Go on.")]);
    let check = |replay: &Replay, anchor, retains, text| verdict(replay, anchor, retains, &[], text);
    // 0: a local command may be kept nowhere.
    assert_eq!(check(&shown, Some("m1"), true, " /compact"), unknown("command"));
    // 1: a replay not ended.
    assert_eq!(
        check(&replay(false, &[]), None, true, "Go on."),
        unknown("replay_incomplete")
    );
    // 2: the anchor is not in the replay.
    assert_eq!(check(&shown, Some("m0"), true, "Go on."), unknown("anchor_missing"));
    // 3: no anchor, and the harness vouches for no message it holds.
    assert_eq!(check(&shown, None, false, "Go on."), unknown("no_anchor"));
    // 4: nothing after the anchor; with no anchor, nothing at all from a harness that keeps its messages.
    assert_eq!(check(&shown, Some("m2"), false, "Go on."), Verdict::NotDelivered);
    assert_eq!(check(&replay(true, &[]), None, true, "Go on."), Verdict::NotDelivered);
    // 5: the one message after the anchor is the prompt.
    let delivered = Verdict::Delivered {
        message_id: "m2".to_owned(),
    };
    assert_eq!(check(&shown, Some("m1"), true, " Go on.\n"), delivered);
    // 6: anything else.
    assert_eq!(check(&shown, Some("m1"), true, "Stop."), unknown("unexpected_messages"));
    assert_eq!(check(&shown, None, true, "Go on."), unknown("unexpected_messages"));
}

/// A message the harness adds by itself is passed over after the anchor only when its text is exactly a marker of the
/// harness: anything else still disputes.
#[test]
fn a_marker_of_the_harness_after_the_anchor_is_no_prompt() {
    let markers = [MARKER];
    let check = |replay: &Replay, markers: &[&str], text| verdict(replay, Some("m1"), true, markers, text);
    let cancelled = replay(true, &[("m1", "Build web."), ("u7", MARKER)]);
    assert_eq!(check(&cancelled, &markers, "Go on."), Verdict::NotDelivered);
    assert_eq!(check(&cancelled, &[], "Go on."), unknown("unexpected_messages"));
    let sent = replay(true, &[("m1", "Build web."), ("u7", MARKER), ("m3", "Go on.")]);
    let delivered = Verdict::Delivered {
        message_id: "m3".to_owned(),
    };
    assert_eq!(check(&sent, &markers, "Go on."), delivered);
    let near = replay(true, &[("m1", "Build web."), ("u7", " [Request interrupted by user]")]);
    assert_eq!(check(&near, &markers, "Go on."), unknown("unexpected_messages"));
    // The anchor still has to be there.
    let lost = replay(true, &[("u7", MARKER)]);
    assert_eq!(check(&lost, &markers, "Go on."), unknown("anchor_missing"));
}
