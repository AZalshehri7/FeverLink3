mod common;

use common::*;
use feverlink3_core::session::*;

fn action(h: &mut Harness, c: ConnectionId, category: &str) {
    let m = ActionMessage::new(20_000, 1, "{}").with_category(category);
    h.player_game(c, MessageKind::Action, &json(&m));
}

fn last_action(p: &Player) -> ActionMessage {
    p.last(MessageKind::Action)
}

#[test]
fn everything_is_allowed_by_default_and_the_category_defaults_to_other() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(a, MessageKind::Action, br#"{"stamp":200,"seq":1,"body":"x"}"#);
    let m = last_action(h.p(b));
    assert_eq!("other", m.category);
    assert!(m.allowed);
    assert_eq!(a_id(&h, a), m.from);
}

fn a_id(h: &Harness, c: ConnectionId) -> u16 {
    h.p(c).session.id().unwrap()
}

#[test]
fn set_permissions_reaches_every_game_and_players_remember_it() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let perms = Permissions { build: PlayerSet::HostOnly, speed: PlayerSet::Only(vec![1]), ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms.clone()));
    assert_eq!(&perms, h.host.permissions());
    for c in [a, b] {
        assert_eq!(&perms, h.p(c).session.permissions());
        assert_eq!(perms, h.p(c).last::<Permissions>(MessageKind::Permissions));
    }
    assert_eq!(perms, h.host_game_last::<Permissions>(MessageKind::Permissions));
}

#[test]
fn a_joiner_is_told_the_current_permissions() {
    let mut h = Harness::new();
    let perms = Permissions { spend: PlayerSet::HostOnly, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms.clone()));
    let a = h.connect("A");
    assert_eq!(&perms, h.p(a).session.permissions());
}

#[test]
fn a_forbidden_action_is_relayed_tagged_not_allowed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let perms = Permissions { build: PlayerSet::Only(vec![a_id(&h, a)]), ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    action(&mut h, a, "build");
    assert!(last_action(h.p(b)).allowed);
    action(&mut h, b, "build");
    let seen_by_a = last_action(h.p(a));
    assert!(!seen_by_a.allowed, "relayed, not dropped");
    assert_eq!("build", seen_by_a.category);
    assert_eq!(a_id(&h, b), seen_by_a.from);
    assert!(!h.host_game_last::<ActionMessage>(MessageKind::Action).allowed, "the host's game sees the verdict too");
}

#[test]
fn the_host_game_is_always_allowed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let perms = Permissions {
        build: PlayerSet::Only(vec![]),
        bulldoze: PlayerSet::HostOnly,
        spend: PlayerSet::HostOnly,
        speed: PlayerSet::HostOnly,
    };
    h.host_call(|host| host.set_permissions(perms));
    let m = ActionMessage::new(200, 1, "{}").with_category("bulldoze");
    h.host_game(MessageKind::Action, &json(&m));
    assert!(last_action(h.p(a)).allowed);
}

#[test]
fn categories_are_independent_and_other_is_never_restricted() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let perms = Permissions {
        build: PlayerSet::HostOnly,
        bulldoze: PlayerSet::HostOnly,
        spend: PlayerSet::HostOnly,
        speed: PlayerSet::HostOnly,
    };
    h.host_call(|host| host.set_permissions(perms));
    for (category, allowed) in [("build", false), ("bulldoze", false), ("spend", false), ("other", true)] {
        action(&mut h, a, category);
        assert_eq!(allowed, last_action(h.p(b)).allowed, "{category}");
    }
    let only_spend = Permissions { spend: PlayerSet::Everyone, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(only_spend));
    action(&mut h, a, "spend");
    assert!(last_action(h.p(b)).allowed);
}

#[test]
fn an_unknown_category_counts_as_other() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let perms = Permissions { build: PlayerSet::HostOnly, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    action(&mut h, a, "teleport");
    let m = last_action(h.p(b));
    assert_eq!("other", m.category);
    assert!(m.allowed);
}

#[test]
fn a_player_cannot_claim_its_own_action_allowed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let perms = Permissions { build: PlayerSet::HostOnly, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    h.player_game(a, MessageKind::Action, br#"{"stamp":1,"seq":1,"body":"x","category":"build","allowed":true}"#);
    assert!(!last_action(h.p(b)).allowed);
}

#[test]
fn speed_requests_obey_the_speed_permission_and_the_refusal_is_logged_once() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.hello_all(0, 0);
    h.host_call(HostSession::start);
    let perms = Permissions { speed: PlayerSet::HostOnly, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    let before = h.host.dropped();
    for speed in [3, 2, 0] {
        h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed }));
    }
    assert_eq!(0, h.host.state().speed);
    assert_eq!(before + 3, h.host.dropped());
    assert_eq!(1, h.host_log.iter().filter(|l| l.contains("speed request") && l.contains("dropped")).count());
    h.host_game(MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 2 }));
    assert_eq!(2, h.host.state().speed);
}

#[test]
fn a_listed_player_may_change_the_speed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.hello_all(0, 0);
    h.host_call(HostSession::start);
    let perms = Permissions { speed: PlayerSet::Only(vec![a_id(&h, b)]), ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 3 }));
    assert_eq!(0, h.host.state().speed);
    h.player_game(b, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 3 }));
    assert_eq!(3, h.host.state().speed);
}

#[test]
fn a_player_cannot_send_permissions() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let before = h.host.dropped();
    h.player_game(a, MessageKind::Permissions, &json(&Permissions::default()));
    assert_eq!(before + 1, h.host.dropped());
    assert_eq!(&Permissions::default(), h.host.permissions());
}

#[test]
fn a_later_permissions_message_replaces_the_earlier_one() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let perms = Permissions { build: PlayerSet::HostOnly, ..Permissions::default() };
    h.host_call(|host| host.set_permissions(perms));
    assert_ne!(&Permissions::default(), h.p(a).session.permissions());
    h.host_call(|host| host.set_permissions(Permissions::default()));
    assert_eq!(&Permissions::default(), h.p(a).session.permissions());
}

#[test]
fn player_sets_have_the_documented_json_shapes() {
    let p = Permissions {
        build: PlayerSet::Everyone,
        bulldoze: PlayerSet::HostOnly,
        spend: PlayerSet::Only(vec![1, 3]),
        speed: PlayerSet::Everyone,
    };
    let text = String::from_utf8(codec::encode(&p)).unwrap();
    assert_eq!(r#"{"build":"everyone","bulldoze":"hostOnly","spend":{"only":[1,3]},"speed":"everyone"}"#, text);
    assert_eq!(Some(p), codec::try_decode::<Permissions>(text.as_bytes()));
    assert_eq!(Some(Permissions::default()), codec::try_decode::<Permissions>(b"{}"));
    assert_eq!(None, codec::try_decode::<Permissions>(br#"{"build":"nobody"}"#));
}

#[test]
fn player_set_membership() {
    assert!(PlayerSet::Everyone.allows(7));
    assert!(!PlayerSet::HostOnly.allows(7));
    assert!(PlayerSet::HostOnly.allows(0));
    assert!(PlayerSet::Only(vec![7]).allows(7));
    assert!(!PlayerSet::Only(vec![7]).allows(8));
    assert!(PlayerSet::Only(vec![]).allows(0));
}

#[test]
fn action_json_without_category_or_allowed_decodes_with_defaults() {
    let m: ActionMessage = codec::try_decode(br#"{"stamp":1,"seq":2,"body":"b"}"#).unwrap();
    assert_eq!("other", m.category);
    assert!(m.allowed);
    assert_eq!(0, m.from);
}
