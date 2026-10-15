//! Several orders in one step (open bug 7, 2026-10-15): two players building at once, a chain, the interface half's pause applies.
//! Every order waits in the queue in the order the mod wrote it and is used by its own command only, matched by its identity (the
//! place of its text, or of its held command for a `complete_next`), never by "the next build_proposal"; a command that is no order's
//! passes untouched; what the apply of a command needs (its timing, the held command that takes its result) is the order's own,
//! never a slot keyed by a payload address.

use super::*;

fn settings() -> portable::Settings {
    portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap()
}

/// The originator's text of a road with one node at `x` (what the mod writes as `native_inject_<key>.txt`).
fn road_text(x: f32) -> String {
    format!("proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\n", x.to_bits())
}

/// A road placeholder at `x` (one added node; its payload's first words are the node vector, so it carries no tag byte).
fn road(x: f32) -> (Call, Box<[u8; 0x40]>) {
    road_placeholder(x)
}

/// The payload the command `c` points to now (the deferred completion exchanges the applied command's payload with the held one's).
fn payload_now(c: &Call) -> usize {
    usize::from_ne_bytes(c.c.command[..8].try_into().unwrap())
}

fn apply(rt: &Runtime, c: &mut Call) {
    let mut sim = [0u8; 8];
    unsafe { rt.on_apply(sim.as_mut_ptr(), c.c.command.as_mut_ptr()) };
}

fn emptied(c: &Call) -> bool {
    c.c.payload[..crate::native_copy::PROPOSAL_SIZE].iter().all(|&b| b == 0)
}

/// The `(key, held)` of every `inject_refused` event, in order.
fn refusals(dir: &Path) -> Vec<(u64, u64)> {
    let num = |w: &str, name: &str| -> u64 {
        let tag = format!("[\"{name}\"]=");
        w.split(&tag).nth(1).and_then(|r| r.split([',', '}']).next()).and_then(|n| n.parse().ok()).unwrap_or(u64::MAX)
    };
    events(dir)
        .into_iter()
        .filter(|(k, _)| k == "inject_refused")
        .map(|(_, w)| (num(&w, "key"), num(&w, "held")))
        .collect()
}

/// The `(id, ok, reason)` of every `completed` event, in order.
fn completions(dir: &Path) -> Vec<(u64, bool, String)> {
    events(dir)
        .into_iter()
        .filter(|(k, _)| k == "completed")
        .map(|(_, w)| {
            let id = w.split("[\"id\"]=").nth(1).and_then(|r| r.split([',', '}']).next()).and_then(|n| n.parse().ok());
            let reason = w.split("[\"reason\"]=\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("");
            (id.unwrap_or(0), w.contains("[\"ok\"]=true"), reason.to_string())
        })
        .collect()
}

fn stand_ins() {
    inject::TEST_FNS.with(|f| {
        f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
    });
}

#[test]
fn three_orders_of_one_step_are_each_used_by_their_own_command_in_any_order_and_a_foreign_one_passes_between_them() {
    let dir = TempDir::new("rt-queue-three");
    let rt = runtime(Some(dir.path()));
    rt.enable_native_inject(settings());
    stand_ins();
    // three builds due in the same step: the mod writes the three texts and the three orders before any command is applied
    for (key, x) in [(5u64, 100f32), (6, 200.0), (7, 300.0)] {
        std::fs::write(dir.path().join(crate::inject::inject_file_name(key)), road_text(x)).unwrap();
    }
    ctl(
        dir.path(),
        "1\tinject_next\t1\t{[\"held\"]=11,[\"key\"]=5}\n2\tinject_next\t1\t{[\"held\"]=12,[\"key\"]=6}\n3\tinject_next\t1\t{[\"held\"]=13,[\"key\"]=7}\n",
    );
    rt.pump();
    assert_eq!(rt.inject.waiting_count(), 3, "no order replaced another");
    assert!(refusals(dir.path()).is_empty());
    // the commands come in another order than the orders, with a town's road between them
    let (mut b, _nb) = road(200.0);
    let (mut town, _nt) = road(400.0);
    let (mut c, _nc) = road(300.0);
    let (mut a, _na) = road(100.0);
    let town_before = town.c.payload.to_vec();
    apply(&rt, &mut b);
    assert!(
        emptied(&b),
        "the second order's command took the second order (refused here: this test's layout is not the game's)"
    );
    assert_eq!(rt.inject.waiting_count(), 2);
    apply(&rt, &mut town);
    assert_eq!(town.c.payload.to_vec(), town_before, "a command that is no order's passes untouched");
    assert_eq!(rt.inject.waiting_count(), 2, "and uses up no order");
    apply(&rt, &mut c);
    apply(&rt, &mut a);
    assert!(emptied(&c) && emptied(&a));
    assert!(!rt.inject.pending());
    rt.pump();
    // each refusal names its own order and its own held command, in the order the commands came
    assert_eq!(refusals(dir.path()), [(6, 12), (7, 13), (5, 11)]);
    let ev = events(dir.path());
    assert!(
        ev.iter().filter(|(k, w)| k == "inject_refused" && w.contains("[\"outcome\"]=\"nothing\"")).count() == 3,
        "{ev:?}"
    );
    inject::TEST_FNS.with(|f| f.set(None));
}

#[test]
fn two_orders_of_the_same_place_are_used_in_the_order_the_mod_wrote_them() {
    let dir = TempDir::new("rt-queue-same-place");
    let rt = runtime(Some(dir.path()));
    rt.enable_native_inject(settings());
    stand_ins();
    // a chain: two builds touch the same node, both due in one step
    for key in [8u64, 9] {
        std::fs::write(dir.path().join(crate::inject::inject_file_name(key)), road_text(500.0)).unwrap();
    }
    ctl(dir.path(), "1\tinject_next\t1\t{[\"held\"]=21,[\"key\"]=8}\n2\tinject_next\t1\t{[\"held\"]=22,[\"key\"]=9}\n");
    let (mut first, _n1) = road(500.0);
    let (mut second, _n2) = road(500.0);
    // the command's own apply reads the file: both orders are armed before the first command is matched
    apply(&rt, &mut first);
    assert_eq!(rt.inject.waiting_count(), 1);
    apply(&rt, &mut second);
    rt.pump();
    assert_eq!(refusals(dir.path()), [(8, 21), (9, 22)]);
    inject::TEST_FNS.with(|f| f.set(None));
}

/// Two of this game's tool commands held (builds at x = 100 and 200) and deferred, and the mod's `complete_next` for each, written in
/// the opposite order. Returns the held calls (their payloads stay alive) and the nodes.
fn two_deferred(rt: &Runtime, dir: &Path) -> Vec<(Call, Box<[u8; 0x40]>)> {
    ctl(dir, HOLD_ON);
    rt.pump();
    let mut held = vec![road(100.0), road(200.0)];
    for (c, _) in &mut held {
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
    }
    ctl(
        dir,
        "2\tdefer\t1\t{[\"id\"]=1}\n3\tdefer\t1\t{[\"id\"]=2}\n4\tcomplete_next\t1\t{[\"held\"]=2}\n5\tcomplete_next\t1\t{[\"held\"]=1}\n",
    );
    rt.pump();
    held
}

/// The mod's release of both deferred commands (the interface world has the apply), then any Add: the callbacks fire.
fn release_both(rt: &Runtime, dir: &Path) {
    ctl(dir, "6\trelease\t1\t{[\"at\"]=0,[\"id\"]=1}\n7\trelease\t1\t{[\"at\"]=0,[\"id\"]=2}\n");
    rt.pump();
    let mut nop = Call::new(27, 50);
    unsafe { rt.on_add(nop.args(), at(0x999)) };
    rt.pump();
}

#[test]
fn each_held_command_takes_the_result_of_its_own_builds_apply_when_two_are_due_in_one_step() {
    let dir = TempDir::new("rt-queue-link");
    let rt = runtime(Some(dir.path()));
    rt.enable_native_inject(settings());
    let held = two_deferred(&rt, dir.path());
    assert_eq!(rt.inject.waiting_count(), 2);
    // the script's rebuilds at the stamp: build 1 first, a town's road between, then build 2
    let (mut r1, _m1) = road(100.0);
    let (mut town, _t) = road(400.0);
    let (mut r2, _m2) = road(200.0);
    let town_payload = payload_now(&town);
    apply(&rt, &mut r1);
    apply(&rt, &mut town);
    assert_eq!(rt.inject.waiting_count(), 1, "the town's road is neither held command's build");
    assert_eq!(payload_now(&town), town_payload, "it gives its result to nobody");
    apply(&rt, &mut r2);
    assert!(!rt.inject.pending());
    // the right result to the right held command: each held command took the payload (the result) of its own build's apply, and the
    // applied command was left the held one's. Before, the second order replaced the first: held 2 took build 1's result, held 1
    // waited for the age limit.
    assert_eq!(payload_now(&r1), held[0].0.payload(), "build 1's apply went to held command 1");
    assert_eq!(payload_now(&r2), held[1].0.payload(), "build 2's apply went to held command 2");
    release_both(&rt, dir.path());
    let done: Vec<_> = completions(dir.path()).into_iter().map(|(id, _, why)| (id, why)).collect();
    assert_eq!(done, [(1, "stamp_apply".to_string()), (2, "stamp_apply".to_string())], "{:?}", events(dir.path()));
    assert!(refusals(dir.path()).is_empty());
}

#[test]
fn commands_the_interface_adds_keep_their_order_until_their_apply_and_a_reused_address_with_another_build_is_not_theirs(
) {
    let dir = TempDir::new("rt-queue-added");
    let rt = runtime(Some(dir.path()));
    rt.enable_native_inject(settings());
    let held = two_deferred(&rt, dir.path());
    // the interface half's applies during a pause: both commands reach the command list's Add before the simulation applies either
    let (mut r2, _m2) = road(200.0);
    let (mut r1, mut m1) = road(100.0);
    unsafe { rt.on_add(r2.args(), at(0x777)) };
    unsafe { rt.on_add(r1.args(), at(0x777)) };
    assert!(!rt.inject.pending(), "each took its own order at its Add");
    assert_eq!(lock(&rt.added).len(), 2);
    // build 2 is applied: it completes held 2
    apply(&rt, &mut r2);
    assert_eq!(lock(&rt.added).len(), 1);
    assert_eq!(payload_now(&r2), held[1].0.payload(), "build 2's apply went to held command 2");
    // build 1's command was destroyed unapplied and its payload address given to another build (a town's road elsewhere): it is not
    // build 1's command, whatever its address
    let r1_payload = payload_now(&r1);
    m1[4..8].copy_from_slice(&900f32.to_bits().to_le_bytes());
    apply(&rt, &mut r1);
    assert_eq!(lock(&rt.added).len(), 1, "the record stays: nothing completes held 1 with the other build's result");
    assert_eq!(payload_now(&r1), r1_payload, "the other build's result went to nobody");
    // the record expires: held 1 fails (its tool callback fired with a failed result), not left busy
    rt.advance(crate::inject::MAX_TTL_MS + 1);
    let (mut later, _l) = road(700.0);
    apply(&rt, &mut later);
    assert!(lock(&rt.added).is_empty());
    release_both(&rt, dir.path());
    let done = completions(dir.path());
    assert!(done.iter().any(|(id, _, why)| *id == 2 && why == "stamp_apply"), "{done:?}");
    assert!(done.contains(&(1, false, "fail".to_string())), "a failed result: {done:?}");
}
