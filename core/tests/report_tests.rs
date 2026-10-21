//! Bug reports over the session (`docs/PROTOCOL.md`, "Bug reports"): the request, the ask to every game, the parts coming back
//! as chunks, the timeout, and the bundle going to the player who asked.

mod common;

use common::*;
use feverlink3_core::session::*;

fn chunks(id: i32, bytes: &[u8]) -> Vec<ResyncChunkMessage> {
    let count = ResyncLimits::chunk_count(bytes.len() as u64).unwrap();
    let sha256 = sha256_hex(bytes);
    bytes
        .chunks(ResyncLimits::CHUNK_SIZE)
        .enumerate()
        .map(|(i, d)| ResyncChunkMessage {
            id,
            index: i as i32,
            count: count as i32,
            sha256: sha256.clone(),
            data: d.to_vec(),
        })
        .collect()
}

fn asked(p: &Player) -> Option<ReportAskMessage> {
    p.reports.iter().rev().find_map(|e| match e {
        PlayerReportEvent::Asked(a) => Some(a.clone()),
        _ => None,
    })
}

fn host_events(h: &Harness) -> Vec<HostReportEvent> {
    h.events
        .iter()
        .filter_map(|e| match e {
            HostOutput::Report(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn collected(h: &Harness) -> Option<CollectedReport> {
    host_events(h).into_iter().find_map(|e| match e {
        HostReportEvent::Collected(c) => Some(c),
        _ => None,
    })
}

fn notices(p: &Player) -> Vec<String> {
    p.game
        .iter()
        .filter(|g| g.0 == MessageKind::Chat)
        .filter_map(|g| codec::try_decode::<ChatMessage>(&g.1))
        .filter(|c| c.level.is_some())
        .map(|c| c.text)
        .collect()
}

fn send_part(h: &mut Harness, c: ConnectionId, id: i32, bytes: &[u8]) {
    for m in chunks(id, bytes) {
        let outs = h.players.get_mut(&c).unwrap().session.local_game_message(MessageKind::ReportPart, &json(&m));
        h.pump_player(c, outs);
    }
}

#[test]
fn a_players_request_collects_every_games_part_and_the_bundle_goes_back_to_that_player() {
    let mut h = Harness::new();
    let a = h.connect("Ann");
    let b = h.connect("Ben");
    let outs = h.players.get_mut(&a).unwrap().session.request_report("the bridge vanished");
    h.pump_player(a, outs);

    // every game is asked, the host's own through its launcher
    let ask = asked(h.p(a)).expect("Ann's launcher is asked for its part too");
    assert_eq!(Some(ask.clone()), asked(h.p(b)));
    assert_eq!(vec![HostReportEvent::BuildPart(ask.clone())], host_events(&h));
    assert_eq!("Ann", ask.by_name);
    assert_eq!(ReportLimits::part_budget(3), ask.max_bytes);
    assert_eq!(6, ask.report.len());
    // nothing of it reaches a game but the notices of the players who did not ask
    assert_eq!(0, h.p(a).count(MessageKind::ReportAsk));
    assert_eq!(vec!["Ann made a bug report; your logs were sent to the host.".to_owned()], notices(h.p(b)));
    assert!(notices(h.p(a)).is_empty(), "the one who asked gets no notice");
    let host_notice: ChatMessage = h.host_game_last(MessageKind::Chat);
    assert_eq!("Ann made a bug report; your logs are in it.", host_notice.text);

    // the parts come back as chunks, checked against their SHA-256 like a repair's save
    let big = blob(6 << 20);
    send_part(&mut h, a, ask.id, &big);
    assert!(h.routable.contains(&a), "Ann was not cut off for flooding");
    h.host_call(|host| host.report_part(ask.id, b"host logs".to_vec()));
    assert!(collected(&h).is_none(), "Ben's part is awaited");
    send_part(&mut h, b, ask.id, b"ben logs");
    let report = collected(&h).expect("every part is in");
    assert_eq!("the bridge vanished", report.note);
    let parts: Vec<(u16, &str, Option<usize>)> =
        report.parts.iter().map(|p| (p.player, p.name.as_str(), p.bytes.as_ref().map(Vec::len))).collect();
    assert_eq!(vec![(0, "Hote", Some(9)), (1, "Ann", Some(big.len())), (2, "Ben", Some(8))], parts);

    // the host's launcher sends the bundle to Ann; only she gets it, whole and named after the report
    let bundle = blob(300_000);
    for m in chunks(ask.id, &bundle) {
        h.host_call(|host| host.send_report_chunk(1, &m));
    }
    let got = h.p(a).reports.iter().find_map(|e| match e {
        PlayerReportEvent::Bundle { id, report, bytes } => Some((*id, report.clone(), bytes.len())),
        _ => None,
    });
    assert_eq!(Some((ask.id, ask.report.clone(), bundle.len())), got);
    assert!(h.p(b).reports.iter().all(|e| matches!(e, PlayerReportEvent::Asked(_))));
}

#[test]
fn a_game_that_does_not_answer_is_missing_after_a_minute_and_one_that_leaves_at_once() {
    let mut h = Harness::new();
    let a = h.connect("Ann");
    let b = h.connect("Ben");
    let c = h.connect("Cid");
    h.host_call(|host| host.request_report(""));
    let ask = asked(h.p(a)).unwrap();
    assert!(notices(h.p(a)).iter().any(|n| n.starts_with("Hote made a bug report")));
    h.host_call(|host| host.report_part(ask.id, b"host".to_vec()));
    send_part(&mut h, a, ask.id, b"ann");
    h.disconnect(c);
    assert!(collected(&h).is_none(), "Ben may still answer");
    h.advance(ReportLimits::ANSWER_SECONDS + 0.5);
    h.host_call(|host| host.tick());
    let report = collected(&h).expect("the report completes without Ben");
    let missing: Vec<(&str, bool)> = report.parts.iter().map(|p| (p.name.as_str(), p.missing.is_some())).collect();
    assert_eq!(vec![("Hote", false), ("Ann", false), ("Ben", true), ("Cid", true)], missing);
    assert!(report.parts[3].missing.as_deref().unwrap().contains("left the session"));
    // a late part of a finished report is ignored, and a new report can start
    send_part(&mut h, b, ask.id, b"late");
    h.host_call(|host| host.request_report("again"));
    assert_eq!(ask.id + 1, asked(h.p(b)).unwrap().id);
}

#[test]
fn a_second_request_while_one_runs_is_refused_at_once_naming_whose_report_runs() {
    let mut h = Harness::new();
    let a = h.connect("Ann");
    let b = h.connect("Ben");
    let outs = h.players.get_mut(&a).unwrap().session.request_report("first");
    h.pump_player(a, outs);
    let ask = asked(h.p(b)).unwrap();
    let outs = h.players.get_mut(&b).unwrap().session.request_report("second");
    h.pump_player(b, outs);
    assert_eq!(1, host_events(&h).len(), "one report at a time");
    // Ben's launcher hears at once whose report runs; nobody's game is told, Ann gets nothing
    let busy = ReportBusyMessage { id: ask.id, by_name: "Ann".into() };
    assert_eq!(Some(&PlayerReportEvent::Busy(busy.clone())), h.p(b).reports.last());
    assert_eq!(0, h.p(b).count(MessageKind::ReportBusy));
    assert!(!notices(h.p(b)).iter().any(|n| n.contains("already")));
    assert!(h.p(a).reports.iter().all(|e| !matches!(e, PlayerReportEvent::Busy(_))));
    assert_eq!("A bug report is already being made; it will reach Ann.", notices::report_busy(&busy.by_name));
    // the host's own window asking now is refused the same way, through its launcher
    h.host_call(|host| host.request_report("third"));
    assert_eq!(Some(HostReportEvent::Busy(busy)), host_events(&h).last().cloned());
    // chunks nobody asked for are not let past the flood limit
    let unasked = h.connect("Dan");
    for m in chunks(99, &blob(12 << 20)) {
        let outs = h.players.get_mut(&unasked).unwrap().session.local_game_message(MessageKind::ReportPart, &json(&m));
        h.pump_player(unasked, outs);
    }
    assert!(!h.routable.contains(&unasked), "a player that floods with parts nobody asked for is cut off");
}
