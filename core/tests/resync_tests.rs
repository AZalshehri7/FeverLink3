mod common;

use common::*;
use feverlink3_core::crypto::Sealer;
use feverlink3_core::session::*;

fn quiet() -> HostOptions {
    HostOptions::default()
}

fn report_all(h: &mut Harness, ids: &[ConnectionId], n: i32, at: i64, host: &[(&str, &str)], player: &[(&str, &str)]) {
    h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, at, parts(host))));
    for &c in ids {
        h.player_game(c, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, at, parts(player))));
    }
}

/// Runs one ordinary checksum round in which the last player disagrees about `part` (or nothing).
fn checksum_round(h: &mut Harness, ids: &[ConnectionId], part: Option<&str>) {
    h.host_call(HostSession::request_checksum);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    let base = [("roads", "r"), ("lines", "l"), ("money", "1")];
    h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, parts(&base))));
    for (i, &c) in ids.iter().enumerate() {
        let mut p = parts(&base);
        if i == ids.len() - 1 {
            if let Some(part) = part {
                p.insert(part.to_owned(), "different".to_owned());
            }
        }
        h.player_game(c, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, p)));
    }
}

// ------------------------------------------------------------------ the round

#[test]
fn full_round_pauses_saves_transfers_loads_verifies_and_resumes_the_previous_speed() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    h.host_call(|host| host.start_resync("test"));
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    assert_eq!(0, pause_at % Lockstep::STEP);
    assert!(ids.iter().all(|&c| h.p(c).session.session().unwrap().pause_at == Some(pause_at)));

    let id = h.reach_transferring();
    assert_eq!(1, h.host_game_count(MessageKind::ResyncSave));
    assert_eq!(id, h.host_game_last::<ResyncSaveMessage>(MessageKind::ResyncSave).id);

    let bytes = blob(600 * 1024); // three chunks
    h.send_file(id, &bytes);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    for &c in &ids {
        assert_eq!(vec![(id, sha256_hex(&bytes), bytes.clone())], h.p(c).saves);
        assert_eq!(0, h.p(c).count(MessageKind::ResyncChunk), "chunks go to the launcher, not the game");
        assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
    }
    assert_eq!(1, h.host_game_count(MessageKind::ResyncLoad));
    assert_eq!(0, h.ready_count(), "everyone must say hello again");

    h.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    assert!(req.paused);
    assert_eq!(pause_at, req.at);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "r")], &[("roads", "r")]);

    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(3, h.host.state().speed);
    assert_eq!(None, h.host.state().pause_at);
    let mut states = h.progress_states();
    states.dedup();
    use ResyncState::*;
    assert_eq!(vec![Pausing, Saving, Transferring, Loading, Verifying, Idle], states);
    assert!(h.failures().is_empty());
}

#[test]
fn pausing_waits_for_every_ready_game_to_stop_and_ignores_unready_ones() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    let late = h.connect("Late"); // never says hello
    h.host_call(|host| host.start_resync("t"));
    let pause_at = h.host.state().pause_at.unwrap();
    let c = |t: i64, speed: i32| json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false });
    h.host_game(MessageKind::Clock, &c(pause_at, 0));
    h.player_game(ids[0], MessageKind::Clock, &c(pause_at, 0));
    assert_eq!(ResyncState::Pausing, h.host.resync_state()); // second player still moving
    h.player_game(ids[1], MessageKind::Clock, &c(pause_at - 200, 0));
    assert_eq!(ResyncState::Pausing, h.host.resync_state()); // stopped, but before the stamp
    h.player_game(ids[1], MessageKind::Clock, &c(pause_at, 1));
    assert_eq!(ResyncState::Pausing, h.host.resync_state()); // at the stamp but still running
    h.player_game(ids[1], MessageKind::Clock, &c(pause_at, 0));
    assert_eq!(ResyncState::Saving, h.host.resync_state()); // `late` was never waited for
    assert_eq!(0, h.p(late).count(MessageKind::Hello));
}

#[test]
fn pausing_gives_up_after_120_seconds_and_stays_paused() {
    let (mut h, _) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    h.advance(119.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
    h.advance(2.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(1, h.failures().len());
    assert!(h.failures()[0].contains("did not stop"), "{:?}", h.failures());
    assert!(h.host.state().pause_at.is_some(), "the session stays paused");
}

#[test]
fn saving_fails_when_the_host_game_reports_failure() {
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let pause_at = h.host.state().pause_at.unwrap();
    h.stop_all_at(pause_at);
    let id = h.host.resync_id().unwrap();
    let done = SaveDoneMessage { id, ok: false, detail: Some("disk full".to_owned()) };
    h.host_game(MessageKind::SaveDone, &json(&done));
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("disk full"));
}

#[test]
fn saving_times_out_after_180_seconds() {
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let pause_at = h.host.state().pause_at.unwrap();
    h.stop_all_at(pause_at);
    h.advance(179.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Saving, h.host.resync_state());
    h.advance(2.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("saving"));
}

#[test]
fn save_done_from_a_player_or_with_another_id_is_ignored() {
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let pause_at = h.host.state().pause_at.unwrap();
    h.stop_all_at(pause_at);
    let id = h.host.resync_id().unwrap();
    h.player_game(ids[0], MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    assert_eq!(ResyncState::Saving, h.host.resync_state());
    h.host_game(MessageKind::SaveDone, &json(&SaveDoneMessage { id: id + 7, ok: true, detail: None }));
    assert_eq!(ResyncState::Saving, h.host.resync_state());
    h.host_game(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    assert_eq!(ResyncState::Transferring, h.host.resync_state());
}

#[test]
fn announced_files_outside_the_bounds_fail_the_round() {
    let sha = sha256_hex(b"x");
    let too_big = ResyncLimits::CHUNK_SIZE as u64 * u64::from(ResyncLimits::MAX_CHUNKS) + 1;
    for (sha, len) in [(sha.as_str(), 0), (sha.as_str(), too_big), ("zz", 10)] {
        let (mut h, _) = Harness::running(1, 1, quiet());
        h.host_call(|host| host.start_resync("t"));
        let id = h.reach_transferring();
        h.host_call(|host| host.resync_file_ready(id, sha, len));
        assert_eq!(ResyncState::Idle, h.host.resync_state(), "len {len}");
        assert_eq!(1, h.failures().len());
    }
}

#[test]
fn the_largest_allowed_file_is_accepted() {
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    let max = ResyncLimits::CHUNK_SIZE as u64 * u64::from(ResyncLimits::MAX_CHUNKS);
    h.host_call(|host| host.resync_file_ready(id, &sha256_hex(b"x"), max));
    assert_eq!(ResyncState::Transferring, h.host.resync_state());
    assert!(h.failures().is_empty());
}

#[test]
fn file_announced_out_of_turn_is_ignored() {
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.resync_file_ready(1, &sha256_hex(b"x"), 10));
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures().is_empty());
}

#[test]
fn chunks_out_of_order_or_of_the_wrong_size_are_refused_and_never_sent() {
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    let bytes = blob(ResyncLimits::CHUNK_SIZE + 100);
    h.host_call(|host| host.resync_file_ready(id, &sha256_hex(&bytes), bytes.len() as u64));
    let before = h.host_log.len();
    h.host_call(|host| host.resync_chunk(id, 1, &bytes[ResyncLimits::CHUNK_SIZE..])); // not the next index
    h.host_call(|host| host.resync_chunk(id, 0, &bytes[..10])); // wrong size
    h.host_call(|host| host.resync_chunk(id, 0, &vec![0u8; ResyncLimits::CHUNK_SIZE + 1])); // over 256 KiB
    h.host_call(|host| host.resync_chunk(id + 1, 0, &bytes[..ResyncLimits::CHUNK_SIZE])); // wrong round
    assert_eq!(before + 4, h.host_log.len());
    assert_eq!(ResyncState::Transferring, h.host.resync_state());
    assert!(h.p(ids[0]).saves.is_empty());
    h.host_call(|host| host.resync_chunk(id, 0, &bytes[..ResyncLimits::CHUNK_SIZE]));
    h.host_call(|host| host.resync_chunk(id, 1, &bytes[ResyncLimits::CHUNK_SIZE..]));
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(1, h.p(ids[0]).saves.len());
}

#[test]
fn transfer_gives_up_after_600_seconds() {
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    h.reach_transferring();
    h.advance(601.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("transfer"));
}

#[test]
fn a_host_alone_skips_the_transfer() {
    let (mut h, _) = Harness::running(0, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.host_call(|host| host.resync_file_ready(id, &sha256_hex(b"save"), 4));
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(1, h.host_game_count(MessageKind::ResyncLoad));
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("a", "1")]))));
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(1, h.host.state().speed);
}

#[test]
fn loading_logs_who_is_missing_after_600_seconds_and_continues() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(pause_at)));
    h.hello_player(ids[0], pause_at, 0);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    h.advance(599.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    h.advance(2.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    assert!(h.host_log.iter().any(|l| l.contains("P1") && l.contains("continuing")), "{:?}", h.host_log);
    // the two games that came back are enough to finish the round
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids[..1], req.n, req.at, &[("a", "1")], &[("a", "1")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
}

#[test]
fn a_player_leaving_during_loading_is_no_longer_waited_for() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(pause_at)));
    h.hello_player(ids[0], pause_at, 0);
    h.disconnect(ids[1]);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
}

#[test]
fn verification_that_differs_fails_and_stays_paused() {
    let (mut h, ids) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(5000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "a")], &[("roads", "b")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("roads"));
    assert_eq!(Some(pause_at), h.host.state().pause_at);
    assert_eq!(1, h.events.iter().filter(|e| matches!(e, HostOutput::ChecksumCompleted(_))).count());
}

#[test]
fn a_verification_round_at_different_sim_times_is_asked_again_and_a_later_match_finishes() {
    let (mut h, ids) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(5000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    for round in 0..3 {
        let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
        h.host_game(
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "a")]))),
        );
        h.player_game(
            ids[0],
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at + 800, parts(&[("roads", "a")]))),
        );
        assert_eq!(ResyncState::Verifying, h.host.resync_state(), "skewed round {round}: asked again, not failed");
    }
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "a")], &[("roads", "a")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures().is_empty());
}

#[test]
fn a_verification_that_stays_at_different_sim_times_fails_after_four_skewed_rounds() {
    let (mut h, ids) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(5000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    for _ in 0..4 {
        let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
        h.host_game(
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "a")]))),
        );
        h.player_game(
            ids[0],
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at + 800, parts(&[("roads", "a")]))),
        );
    }
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("different times"));
    assert_eq!(Some(pause_at), h.host.state().pause_at);
}

#[test]
fn verification_that_never_completes_fails_after_120_seconds() {
    let (mut h, _) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(5000));
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    h.advance(121.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert!(h.failures()[0].contains("verification"));
}

#[test]
fn speed_changes_are_ignored_while_a_round_runs() {
    let (mut h, ids) = Harness::running(1, 2, quiet());
    h.host_call(|host| host.start_resync("t"));
    h.player_game(ids[0], MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 4 }));
    h.host_call(|host| host.set_speed(3));
    h.host_call(HostSession::pause);
    assert!(h.host.state().pause_at.is_some());
    assert_eq!(2, h.host.state().speed);
}

#[test]
fn start_resync_needs_a_started_session_and_no_round_running() {
    let mut h = Harness::new();
    h.host_call(|host| host.start_resync("t"));
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    let (mut h, _) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("first"));
    let id = h.host.resync_id();
    h.host_call(|host| host.start_resync("second"));
    assert_eq!(id, h.host.resync_id());
}

#[test]
fn an_already_paused_session_keeps_its_pause_stamp() {
    let (mut h, _) = Harness::running(1, 2, quiet());
    h.host_call(HostSession::pause);
    let stamp = h.host.state().pause_at;
    h.host_call(|host| host.start_resync("t"));
    assert_eq!(stamp, h.host.state().pause_at);
}

// ------------------------------------------------------------------ hot join

#[test]
fn a_player_joining_a_started_session_starts_a_round_and_gets_the_save() {
    let (mut h, ids) = Harness::running(1, 2, quiet());
    let joiner = h.connect("Joiner");
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
    assert!(h.host_log.iter().any(|l| l.contains("new player")));
    let id = h.reach_transferring();
    let bytes = blob(300_000);
    h.send_file(id, &bytes);
    assert_eq!(bytes, h.p(joiner).saves[0].2);
    assert_eq!(bytes, h.p(ids[0]).saves[0].2);
    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    let mut all = ids.clone();
    all.push(joiner);
    report_all(&mut h, &all, req.n, req.at, &[("a", "1")], &[("a", "1")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(2, h.host.state().speed);
}

#[test]
fn a_player_joining_before_the_start_triggers_nothing() {
    let mut h = Harness::new();
    h.connect("A");
    assert_eq!(ResyncState::Idle, h.host.resync_state());
}

#[test]
fn a_player_joining_while_files_are_moving_waits_for_the_next_round() {
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    let late = h.connect("Late");
    assert_eq!(Some(id), h.host.resync_id());
    assert_eq!(0, h.p(late).saves.len());
    assert_eq!(0, h.p(late).count(MessageKind::ResyncLoad));
    let pause_at = h.host.state().pause_at.unwrap();
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(pause_at)));
    h.hello_player(ids[0], pause_at, 0);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("a", "1")], &[("a", "1")]);
    assert_eq!(Some(id + 1), h.host.resync_id());
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
}

// ------------------------------------------------------------------ policy in the host

#[test]
fn two_differing_checks_in_a_row_request_and_start_a_resync() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    checksum_round(&mut h, &ids, Some("roads"));
    assert!(h.requests().is_empty());
    checksum_round(&mut h, &ids, Some("roads"));
    assert_eq!(1, h.requests().len());
    assert!(h.requests()[0].contains("roads"));
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
}

#[test]
fn without_auto_resync_the_request_is_emitted_and_nothing_starts() {
    let options = HostOptions { auto_resync: false, ..HostOptions::default() };
    let (mut h, ids) = Harness::running(2, 1, options);
    checksum_round(&mut h, &ids, Some("roads"));
    checksum_round(&mut h, &ids, Some("roads"));
    assert_eq!(1, h.requests().len());
    assert_eq!(ResyncState::Idle, h.host.resync_state());
}

#[test]
fn money_differences_never_request_a_resync() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    for _ in 0..6 {
        checksum_round(&mut h, &ids, Some("money"));
    }
    assert!(h.requests().is_empty());
    assert_eq!(ResyncState::Idle, h.host.resync_state());
}

#[test]
fn drift_parts_request_only_after_five_minutes_of_continuous_drift() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    checksum_round(&mut h, &ids, Some("vehicles"));
    h.advance(299.0);
    checksum_round(&mut h, &ids, Some("vehicles"));
    assert!(h.requests().is_empty());
    h.advance(2.0);
    checksum_round(&mut h, &ids, Some("vehicles"));
    assert_eq!(1, h.requests().len());
    assert!(h.requests()[0].contains("vehicles"));
}

#[test]
fn a_replay_failure_requests_a_resync_at_once_and_the_cooldown_holds_the_next() {
    let (mut h, ids) = Harness::running(2, 1, quiet());
    h.player_game(ids[0], MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "no node".to_owned() }));
    assert_eq!(vec!["replay failed".to_owned()], h.requests());
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
    // let the round fail so a cooldown starts
    h.advance(121.0);
    h.host_call(HostSession::tick);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    h.advance(10.0);
    h.player_game(ids[1], MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "again".to_owned() }));
    assert_eq!(1, h.requests().len(), "inside the 120 s cooldown");
    h.advance(120.0);
    h.host_game(MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "host".to_owned() }));
    assert_eq!(2, h.requests().len());
}

#[test]
fn no_policy_requests_while_a_round_is_running_or_before_the_start() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.hello_all(0, 0);
    h.player_game(a, MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "x".to_owned() }));
    assert!(h.requests().is_empty());
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    h.player_game(ids[0], MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "x".to_owned() }));
    assert!(h.requests().is_empty());
}

// ------------------------------------------------------------------ the player side

struct Crafter {
    sealer: Sealer,
}

impl Crafter {
    fn new(h: &Harness) -> Self {
        let mut sealer = Sealer::new(&h.key, 0);
        for _ in 0..10_000 {
            sealer.seal(0, b"");
        }
        Self { sealer }
    }

    fn send(&mut self, h: &mut Harness, c: ConnectionId, kind: MessageKind, payload: &[u8]) {
        let frame = self.sealer.seal(kind.into(), payload);
        let outs = h.players.get_mut(&c).unwrap().session.frame_received(&frame);
        h.pump_player(c, outs);
    }

    fn chunk(&mut self, h: &mut Harness, c: ConnectionId, m: &ResyncChunkMessage) {
        self.send(h, c, MessageKind::ResyncChunk, &json(m));
    }
}

fn chunk_msg(id: i32, index: i32, count: i32, sha: &str, data: &[u8]) -> ResyncChunkMessage {
    ResyncChunkMessage { id, index, count, sha256: sha.to_owned(), data: data.to_vec() }
}

fn joined_player() -> (Harness, ConnectionId, Crafter) {
    let mut h = Harness::new();
    let c = h.connect("A");
    let crafter = Crafter::new(&h);
    (h, c, crafter)
}

#[test]
fn player_assembles_chunks_in_any_order_and_emits_the_save_after_the_hash_matches() {
    let (mut h, c, mut host) = joined_player();
    let bytes = blob(10);
    let sha = sha256_hex(&bytes);
    host.chunk(&mut h, c, &chunk_msg(5, 1, 2, &sha, &bytes[5..]));
    assert!(h.p(c).saves.is_empty());
    host.chunk(&mut h, c, &chunk_msg(5, 1, 2, &sha, &bytes[5..])); // a duplicate changes nothing
    host.chunk(&mut h, c, &chunk_msg(5, 0, 2, &sha, &bytes[..5]));
    assert_eq!(vec![(5, sha, bytes)], h.p(c).saves);
}

#[test]
fn player_refuses_a_save_whose_hash_does_not_match() {
    let (mut h, c, mut host) = joined_player();
    let wrong = sha256_hex(b"something else");
    host.chunk(&mut h, c, &chunk_msg(1, 0, 1, &wrong, &blob(40)));
    assert!(h.p(c).saves.is_empty());
    assert_eq!(1, h.p(c).session.dropped());
}

#[test]
fn player_refuses_chunks_outside_the_bounds() {
    let (mut h, c, mut host) = joined_player();
    let sha = sha256_hex(b"x");
    let data = blob(10);
    let big = vec![1u8; ResyncLimits::CHUNK_SIZE + 1];
    let cases = [
        chunk_msg(1, 0, 2049, &sha, &data), // too many chunks
        chunk_msg(1, 0, 0, &sha, &data),    // no chunks
        chunk_msg(1, 2, 2, &sha, &data),    // index == count
        chunk_msg(1, -1, 2, &sha, &data),   // negative index
        chunk_msg(1, 0, 2, &sha, &[]),      // empty
        chunk_msg(1, 0, 2, &sha, &big),     // over 256 KiB
        chunk_msg(1, 0, 2, "not hex", &data),
    ];
    for (i, m) in cases.iter().enumerate() {
        host.chunk(&mut h, c, m);
        assert_eq!(i as u64 + 1, h.p(c).session.dropped(), "case {i}");
    }
    assert!(h.p(c).saves.is_empty());
}

#[test]
fn player_refuses_chunks_that_disagree_with_the_transfer_in_progress() {
    let (mut h, c, mut host) = joined_player();
    let a = blob(10);
    let sha = sha256_hex(&a);
    host.chunk(&mut h, c, &chunk_msg(2, 0, 2, &sha, &a[..5]));
    host.chunk(&mut h, c, &chunk_msg(2, 1, 3, &sha, &a[5..])); // other count
    host.chunk(&mut h, c, &chunk_msg(2, 1, 2, &sha256_hex(b"y"), &a[5..])); // other hash
    assert_eq!(2, h.p(c).session.dropped());
    host.chunk(&mut h, c, &chunk_msg(2, 1, 2, &sha, &a[5..]));
    assert_eq!(1, h.p(c).saves.len());
}

#[test]
fn player_forgets_an_unfinished_transfer_after_ten_minutes() {
    let (mut h, c, mut host) = joined_player();
    let a = blob(10);
    let sha = sha256_hex(&a);
    host.chunk(&mut h, c, &chunk_msg(3, 0, 2, &sha, &a[..5]));
    h.advance(599.0);
    let outs = h.players.get_mut(&c).unwrap().session.tick();
    h.pump_player(c, outs);
    assert!(h.p(c).log.iter().all(|l| !l.contains("forgotten")));
    h.advance(2.0);
    let outs = h.players.get_mut(&c).unwrap().session.tick();
    h.pump_player(c, outs);
    assert!(h.p(c).log.iter().any(|l| l.contains("forgotten")));
    host.chunk(&mut h, c, &chunk_msg(3, 1, 2, &sha, &a[5..])); // the old half is gone: no save
    assert!(h.p(c).saves.is_empty());
}

#[test]
fn player_delivers_a_load_only_for_a_save_it_received_intact() {
    let (mut h, c, mut host) = joined_player();
    let bytes = blob(20);
    let sha = sha256_hex(&bytes);
    let load = json(&ResyncLoadMessage { id: 9, name: "n".to_owned(), sha256: sha.clone() });
    host.send(&mut h, c, MessageKind::ResyncLoad, &load);
    assert_eq!(0, h.p(c).count(MessageKind::ResyncLoad));
    host.chunk(&mut h, c, &chunk_msg(9, 0, 1, &sha, &bytes));
    host.send(&mut h, c, MessageKind::ResyncLoad, &load);
    assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
    let other = json(&ResyncLoadMessage { id: 9, name: "n".to_owned(), sha256: sha256_hex(b"z") });
    host.send(&mut h, c, MessageKind::ResyncLoad, &other);
    assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
}

// ------------------------------------------------------------------ helpers

#[test]
fn chunk_arithmetic_matches_the_bounds() {
    let cs = ResyncLimits::CHUNK_SIZE as u64;
    assert_eq!(None, ResyncLimits::chunk_count(0));
    assert_eq!(Some(1), ResyncLimits::chunk_count(1));
    assert_eq!(Some(1), ResyncLimits::chunk_count(cs));
    assert_eq!(Some(2), ResyncLimits::chunk_count(cs + 1));
    assert_eq!(Some(2048), ResyncLimits::chunk_count(cs * 2048));
    assert_eq!(None, ResyncLimits::chunk_count(cs * 2048 + 1));
    assert_eq!(Some(ResyncLimits::CHUNK_SIZE), ResyncLimits::chunk_len(cs + 5, 0));
    assert_eq!(Some(5), ResyncLimits::chunk_len(cs + 5, 1));
    assert_eq!(None, ResyncLimits::chunk_len(cs + 5, 2));
}

#[test]
fn sha256_helpers_are_lower_case_hex() {
    assert_eq!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", sha256_hex(b"abc"));
    assert_eq!(Some("ab".repeat(32)), normalize_sha256(&"AB".repeat(32)));
    assert_eq!(None, normalize_sha256("abc"));
}

#[test]
fn permissions_is_kind_20_and_only_the_host_may_send_it() {
    assert_eq!(20, u8::from(MessageKind::Permissions));
    assert_eq!(Some(MessageKind::Permissions), MessageKind::from_u8(20));
    assert!(!MessageKind::Permissions.allowed_from_player());
}

// ------------------------------------------------------------------ starting with games at different times

/// Two games that loaded different saves (the host's clock is behind the player's) and a host that presses Start.
fn started_at_different_times() -> (Harness, ConnectionId) {
    let mut h = Harness::with(quiet());
    let p = h.connect("Guest");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(87_000)));
    h.host_game(
        MessageKind::Clock,
        &json(&ClockMessage { t: 87_000, speed: 0, ahead: 3, need: None, safe: None, pause: false }),
    );
    h.hello_player(p, 183_600, 0);
    h.host_call(HostSession::start);
    (h, p)
}

#[test]
fn starting_with_games_at_different_times_brings_everyone_into_the_hosts_world_at_once() {
    let (mut h, p) = started_at_different_times();
    assert_eq!(ResyncState::Pausing, h.host.resync_state());
    let state = h.host.state().clone();
    let pause_at = state.pause_at.expect("a pause stamp");
    assert!(state.speed > 0, "a stopped game never reaches a stamp ahead of it, so the games may run");
    assert!(
        pause_at > 87_000 && pause_at < 183_600,
        "counted from the host's clock, not the furthest game's: {pause_at}"
    );
    assert_eq!(0, pause_at % Lockstep::STEP);
    assert_eq!(Some(pause_at), h.p(p).session.session().unwrap().pause_at);

    // the player's game was already past the stamp and holds; the host's game runs up to the stamp and stops there
    assert_eq!(ResyncState::Pausing, h.host.resync_state(), "the host's game is still behind the stamp");
    h.host_game(
        MessageKind::Clock,
        &json(&ClockMessage { t: pause_at, speed: 1, ahead: 3, need: None, safe: None, pause: false }),
    );
    assert_eq!(ResyncState::Pausing, h.host.resync_state(), "still running");
    h.host_game(
        MessageKind::Clock,
        &json(&ClockMessage { t: pause_at, speed: 0, ahead: 3, need: None, safe: None, pause: false }),
    );
    assert_eq!(ResyncState::Saving, h.host.resync_state());

    let id = h.host.resync_id().unwrap();
    h.host_game(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    let bytes = blob(1000);
    h.send_file(id, &bytes);
    assert_eq!(vec![(id, sha256_hex(&bytes), bytes.clone())], h.p(p).saves);
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    assert_eq!(pause_at, req.at);
    report_all(&mut h, &[p], req.n, req.at, &[("roads", "r")], &[("roads", "r")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(1, h.host.state().speed, "the session runs afterwards");
    assert_eq!(None, h.host.state().pause_at);
    assert!(h.failures().is_empty(), "{:?}", h.failures());
}

#[test]
fn starting_with_one_game_does_not_start_a_round() {
    let mut h = Harness::with(quiet());
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(87_000)));
    h.host_game(
        MessageKind::Clock,
        &json(&ClockMessage { t: 87_000, speed: 0, ahead: 3, need: None, safe: None, pause: false }),
    );
    h.host_call(HostSession::start);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(Some(87_000), h.host.state().pause_at);
}

#[test]
fn starting_with_games_at_the_same_time_does_not_start_a_round() {
    let mut h = Harness::with(quiet());
    let p = h.connect("Guest");
    h.hello_all(10_000, 0);
    let _ = p;
    h.host_call(HostSession::start);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(Some(10_000), h.host.state().pause_at);
}

// ------------------------------------------------------------------ a reloading host loads while the players download

#[test]
fn a_reloading_host_is_told_to_load_when_the_save_is_announced_and_may_say_hello_before_the_players_do() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    h.host_call(|host| host.start_resync("test"));
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    let id = h.reach_transferring();
    let bytes = blob(600 * 1024);
    h.send_file(id, &bytes);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(1, h.host_game_count(MessageKind::ResyncLoad), "the host's game is told exactly once");
    for &c in &ids {
        assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
    }
    h.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    assert_eq!(1, h.host_game_count(MessageKind::ResyncLoad));
    assert!(h.failures().is_empty(), "{:?}", h.failures());
}

#[test]
fn with_host_loads_early_off_the_host_is_told_to_load_together_with_the_players_after_the_transfer() {
    let options = HostOptions { host_loads_early: false, ..HostOptions::default() };
    let (mut h, ids) = Harness::running(2, 3, options);
    h.host_call(|host| host.start_resync("test"));
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    let id = h.reach_transferring();
    assert_eq!(0, h.host_game_count(MessageKind::ResyncLoad), "not yet: the players still download");
    let bytes = blob(600 * 1024);
    h.send_file(id, &bytes);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(1, h.host_game_count(MessageKind::ResyncLoad), "the host's game is told exactly once, with the players");
    for &c in &ids {
        assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
    }
    h.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    assert!(h.failures().is_empty(), "{:?}", h.failures());
}

// ------------------------------------------------------------------ a host whose game does not reload

#[test]
fn a_host_that_does_not_reload_keeps_its_world_and_only_the_players_load() {
    let options = HostOptions { host_reloads: false, ..quiet() };
    let (mut h, ids) = Harness::running(2, 3, options);
    h.host_call(|host| host.start_resync("test"));
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    let id = h.reach_transferring();
    let bytes = blob(1000);
    h.send_file(id, &bytes);
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(0, h.host_game_count(MessageKind::ResyncLoad), "the host's own game is not told to load");
    for &c in &ids {
        assert_eq!(1, h.p(c).count(MessageKind::ResyncLoad));
    }
    assert_eq!(1, h.ready_count(), "only the host's game is still ready; the players owe a hello");
    assert!(h.host.players().get(&0).is_some_and(|p| p.ready));

    // the players load and say hello; the host's game never does
    for &c in &ids {
        h.hello_player(c, pause_at, 0);
    }
    assert_eq!(ResyncState::Verifying, h.host.resync_state());
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    assert!(req.paused);
    assert_eq!(pause_at, req.at);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "r")], &[("roads", "r")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(3, h.host.state().speed);
    assert!(h.failures().is_empty(), "{:?}", h.failures());
}

#[test]
fn a_host_that_does_not_reload_still_fails_the_round_when_the_worlds_differ() {
    let options = HostOptions { host_reloads: false, ..quiet() };
    let (mut h, ids) = Harness::running(1, 1, options);
    h.host_call(|host| host.start_resync("test"));
    let pause_at = h.host.state().pause_at.unwrap();
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    h.hello_player(ids[0], pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "r")], &[("roads", "other")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    let failures = h.failures();
    assert_eq!(1, failures.len(), "{failures:?}");
    assert!(failures[0].contains("roads"), "{failures:?}");
    assert!(h.host.state().pause_at.is_some(), "the session stays paused");
}

// ------------------------------------------------------------------ what a round says about itself

#[test]
fn the_session_state_names_the_phase_of_the_round_and_every_player_is_told() {
    let (mut h, ids) = Harness::running(2, 2, quiet());
    let phase = |h: &Harness| h.host.state().resync.clone();
    let told = |h: &Harness, c| h.p(c).session.session().unwrap().resync.clone();
    assert_eq!(None, phase(&h));

    h.host_call(|host| host.start_resync("test"));
    assert_eq!(Some("pausing".to_owned()), phase(&h));
    assert!(
        ids.iter().all(|&c| told(&h, c) == Some("pausing".to_owned())),
        "players' windows can say the host is preparing"
    );

    let id = h.reach_transferring();
    assert_eq!(Some("sending".to_owned()), phase(&h));
    assert!(ids.iter().all(|&c| told(&h, c) == Some("sending".to_owned())));

    let bytes = blob(10);
    h.send_file(id, &bytes);
    assert_eq!(Some("loading".to_owned()), phase(&h));
    assert!(ids.iter().all(|&c| told(&h, c) == Some("loading".to_owned())));

    let pause_at = h.host.state().pause_at.unwrap();
    h.hello_all(pause_at, 0);
    assert_eq!(Some("verifying".to_owned()), phase(&h));
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "r")], &[("roads", "r")]);
    assert_eq!(None, phase(&h), "no round, no phase");
    assert!(ids.iter().all(|&c| told(&h, c).is_none()), "and the players are told it is over");
}

#[test]
fn a_round_that_fails_clears_the_phase_too() {
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("t"));
    h.advance(121.0);
    h.host_call(HostSession::tick);
    assert_eq!(None, h.host.state().resync);
    assert!(h.p(ids[0]).session.session().unwrap().resync.is_none());
}

#[test]
fn a_player_counts_the_chunks_of_the_save_it_is_receiving() {
    let (mut h, ids) = Harness::running(1, 1, quiet());
    h.host_call(|host| host.start_resync("test"));
    let id = h.reach_transferring();
    let bytes = blob(600 * 1024); // three chunks
    let sha = sha256_hex(&bytes);
    h.host_call(|host| host.resync_file_ready(id, &sha, bytes.len() as u64));
    assert_eq!(None, h.p(ids[0]).session.transfer_progress(), "nothing yet");
    let chunks: Vec<&[u8]> = bytes.chunks(ResyncLimits::CHUNK_SIZE).collect();
    assert_eq!(3, chunks.len());
    h.host_call(|host| host.resync_chunk(id, 0, chunks[0]));
    assert_eq!(Some((1, 3)), h.p(ids[0]).session.transfer_progress());
    h.host_call(|host| host.resync_chunk(id, 1, chunks[1]));
    assert_eq!(Some((2, 3)), h.p(ids[0]).session.transfer_progress());
    h.host_call(|host| host.resync_chunk(id, 2, chunks[2]));
    assert_eq!(
        None,
        h.p(ids[0]).session.transfer_progress(),
        "complete: the save is handed over and the count is gone"
    );
}

// ------------------------------------------------------------------ the save over bulk datagrams

fn bulk_round(h: &mut Harness) -> i32 {
    h.host_call(|host| host.start_resync("test"));
    h.reach_transferring()
}

#[test]
fn a_players_capabilities_reach_the_host_and_unknown_ones_do_no_harm() {
    let mut h = Harness::with(quiet());
    let a = h.connect_with_caps("A", &[CAP_BULK_DGRAM, "something-from-the-future"]);
    let b = h.connect("B");
    let (ia, ib) = (h.p(a).session.id().unwrap(), h.p(b).session.id().unwrap());
    assert!(h.host.player_supports(ia, CAP_BULK_DGRAM));
    assert!(h.host.player_supports(ia, "something-from-the-future"));
    assert!(!h.host.player_supports(ib, CAP_BULK_DGRAM));
    assert!(!h.host.player_supports(99, CAP_BULK_DGRAM));
}

#[test]
fn a_join_message_without_capabilities_is_what_older_players_send() {
    let join = JoinMessage::new("Old", "0.1", vec![1, 2, 3]);
    let text = String::from_utf8(json(&join)).unwrap();
    assert!(!text.contains("caps"), "{text}");
    let back: JoinMessage = codec::try_decode(text.as_bytes()).unwrap();
    assert!(back.caps.is_empty());
    let with: JoinMessage =
        codec::try_decode(br#"{"name":"N","version":"0.1","proof":"AQID","caps":["bulk-dgram-1"]}"#).unwrap();
    assert_eq!(with.caps, vec![CAP_BULK_DGRAM.to_owned()]);
}

#[test]
fn chunks_skip_the_players_whose_save_goes_by_bulk_and_the_load_still_reaches_everyone() {
    let (mut h, ids) = Harness::running_with_caps(&[vec![CAP_BULK_DGRAM], vec![]], 1, quiet());
    let (a, b) = (ids[0], ids[1]);
    let id = bulk_round(&mut h);
    let bytes = blob(600 * 1024);
    let sha = sha256_hex(&bytes);
    h.host_call(|host| host.resync_file_ready(id, &sha, bytes.len() as u64));
    let ia = h.p(a).session.id().unwrap();
    assert!(h.host.resync_receivers().contains(&ia));
    h.host.resync_set_bulk(id, &[ia, 0, 77]);
    let chunks: Vec<&[u8]> = bytes.chunks(ResyncLimits::CHUNK_SIZE).collect();
    for (i, chunk) in chunks.iter().enumerate().take(chunks.len() - 1) {
        h.host_call(|host| host.resync_chunk(id, i as u32, chunk));
    }
    assert!(h.p(a).saves.is_empty(), "A got no chunk");
    // A's launcher received the save as bulk datagrams; only then does the host send the last chunk and so the load
    let outs = h.players.get_mut(&a).unwrap().session.bulk_save_received(id, &sha, bytes.clone());
    h.pump_player(a, outs);
    assert_eq!(vec![(id, sha.clone(), bytes.clone())], h.p(a).saves);
    let last = chunks.len() as u32 - 1;
    h.host_call(|host| host.resync_chunk(id, last, chunks[last as usize]));
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(vec![(id, sha, bytes)], h.p(b).saves, "B got its chunks");
    assert_eq!(1, h.p(a).count(MessageKind::ResyncLoad));
    assert_eq!(1, h.p(b).count(MessageKind::ResyncLoad));
}

#[test]
fn a_failed_bulk_transfer_falls_back_to_chunks_for_that_player_alone() {
    let (mut h, ids) = Harness::running_with_caps(&[vec![CAP_BULK_DGRAM], vec![]], 1, quiet());
    let (a, b) = (ids[0], ids[1]);
    let id = bulk_round(&mut h);
    let bytes = blob(600 * 1024);
    let sha = sha256_hex(&bytes);
    h.host_call(|host| host.resync_file_ready(id, &sha, bytes.len() as u64));
    let ia = h.p(a).session.id().unwrap();
    h.host.resync_set_bulk(id, &[ia]);
    let chunks: Vec<&[u8]> = bytes.chunks(ResyncLimits::CHUNK_SIZE).collect();
    // all but the last chunk go the usual way; then the bulk transfer to A is declared failed and A gets every chunk
    for (i, chunk) in chunks.iter().enumerate().take(chunks.len() - 1) {
        h.host_call(|host| host.resync_chunk(id, i as u32, chunk));
    }
    assert_eq!(ResyncState::Transferring, h.host.resync_state());
    for (i, chunk) in chunks.iter().enumerate() {
        h.host_call(|host| host.resync_chunk_to(id, ia, i as u32, chunk));
    }
    assert_eq!(ResyncState::Transferring, h.host.resync_state(), "the fallback does not move the round on");
    assert_eq!(vec![(id, sha.clone(), bytes.clone())], h.p(a).saves);
    assert!(h.p(b).saves.is_empty(), "B is not sent anything twice");
    let last = chunks.len() as u32 - 1;
    h.host_call(|host| host.resync_chunk(id, last, chunks[last as usize]));
    assert_eq!(ResyncState::Loading, h.host.resync_state());
    assert_eq!(vec![(id, sha, bytes)], h.p(b).saves);
    // a chunk for someone who is not a receiver, or of the wrong size, is refused
    h.host_call(|host| host.resync_chunk_to(id, 99, 0, &[0; 4]));
}

#[test]
fn a_bulk_save_with_the_wrong_hash_is_refused_and_a_load_without_the_save_drops_the_player() {
    let (mut h, ids) = Harness::running_with_caps(&[vec![CAP_BULK_DGRAM]], 1, quiet());
    let a = ids[0];
    let bytes = blob(5000);
    let sha = sha256_hex(&bytes);
    let outs = h.players.get_mut(&a).unwrap().session.bulk_save_received(4, &sha256_hex(b"other"), bytes.clone());
    h.pump_player(a, outs);
    assert!(h.p(a).saves.is_empty());
    assert!(h.p(a).log.iter().any(|l| l.contains("bulk save 4 refused")), "{:?}", h.p(a).log);
    let outs = h.players.get_mut(&a).unwrap().session.bulk_save_received(4, "nothex", bytes.clone());
    h.pump_player(a, outs);
    assert!(h.p(a).saves.is_empty());
    let outs = h.players.get_mut(&a).unwrap().session.bulk_save_received(4, &sha, Vec::new());
    h.pump_player(a, outs);
    assert!(h.p(a).saves.is_empty());
    // the right one is taken, once for the load that names it
    let outs = h.players.get_mut(&a).unwrap().session.bulk_save_received(4, &sha, bytes.clone());
    h.pump_player(a, outs);
    assert_eq!(1, h.p(a).saves.len());
}

// ------------------------------------------------------------------ light and full checksum rounds

/// One ordinary round in which the last player disagrees about `part`; says whether the host asked for a full round.
fn round_is_full(h: &mut Harness, ids: &[ConnectionId], part: Option<&str>) -> bool {
    checksum_round(h, ids, part);
    h.host_game_last::<ChecksumRequestMessage>(MessageKind::ChecksumRequest).full
}

#[test]
fn only_every_sixth_checksum_round_is_full_and_the_verification_round_always_is() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    let kinds: Vec<bool> = (0..13).map(|_| round_is_full(&mut h, &ids, None)).collect();
    let first = kinds.iter().position(|&f| f).expect("a full round");
    assert!(first < 6);
    for (i, &full) in kinds.iter().enumerate().skip(first) {
        assert_eq!(full, (i - first) % 6 == 0, "round {i}: {kinds:?}");
    }
    h.host_call(|host| host.start_resync("test"));
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    assert!(req.paused && req.full, "the check after a reload is a full one");
}

#[test]
fn a_difference_in_a_heavy_lane_is_judged_over_full_rounds_only() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    while !round_is_full(&mut h, &ids, Some("edges")) {} // rounds before the first full one: the heavy lane is not compared
    assert_eq!(ResyncState::Idle, h.host.resync_state(), "one full round is not enough");
    for _ in 0..5 {
        assert!(!round_is_full(&mut h, &ids, Some("edges")));
        assert_eq!(ResyncState::Idle, h.host.resync_state(), "a light round cannot count a heavy lane");
    }
    // the next full round differs again: two full checks in a row
    assert!(round_is_full(&mut h, &ids, Some("edges")));
    assert_ne!(
        ResyncState::Idle,
        h.host.resync_state(),
        "two full rounds in a row with the same heavy difference reload"
    );
}

#[test]
fn a_light_round_with_a_light_difference_still_counts_and_an_old_game_that_sends_heavy_lanes_is_not_a_difference() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    checksum_round(&mut h, &ids, None); // round 1, full
                                        // round 2 is light: the host game sends no heavy lane, a game that does not know the flag sends one: still identical
    h.host_call(HostSession::request_checksum);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    assert!(!req.full);
    h.host_game(
        MessageKind::ChecksumReport,
        &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "r")]))),
    );
    for &c in &ids {
        h.player_game(
            c,
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "r"), ("edges", "e")]))),
        );
    }
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    // a light lane that differs in two light rounds in a row reloads as before
    checksum_round(&mut h, &ids, Some("lines")); // light
    checksum_round(&mut h, &ids, Some("lines")); // light
    assert_ne!(ResyncState::Idle, h.host.resync_state());
}

// ------------------------------------------------------------------ the chat lines around a repair

#[test]
fn a_repair_sync_is_announced_before_it_starts_and_its_end_after_it() {
    let (mut h, ids) = Harness::running(2, 3, quiet());
    let before = h.p(ids[0]).count(MessageKind::Chat);
    checksum_round(&mut h, &ids, Some("lines"));
    checksum_round(&mut h, &ids, Some("lines")); // two rounds in a row: the safety net starts a resync
    assert_ne!(ResyncState::Idle, h.host.resync_state());
    assert_eq!(before + 1, h.p(ids[0]).count(MessageKind::Chat), "one chat line to every player");
    let chat: ChatMessage = h.p(ids[0]).last(MessageKind::Chat);
    assert_eq!(Some("FeverLink3"), chat.name.as_deref());
    assert!(chat.text.contains("repair sync starts") && chat.text.contains("will be lost"), "{}", chat.text);
    assert!(h.host_game_last::<ChatMessage>(MessageKind::Chat).text.contains("repair sync"), "the host's own game too");
    let pause_at = h.host.state().pause_at.expect("pause stamp");
    let id = h.reach_transferring();
    h.send_file(id, &blob(1000));
    h.hello_all(pause_at, 0);
    let req: ChecksumRequestMessage = h.host_game_last(MessageKind::ChecksumRequest);
    report_all(&mut h, &ids, req.n, req.at, &[("roads", "r")], &[("roads", "r")]);
    assert_eq!(ResyncState::Idle, h.host.resync_state());
    let chat: ChatMessage = h.p(ids[0]).last(MessageKind::Chat);
    assert!(chat.text.contains("sync is done"), "{}", chat.text);
}
