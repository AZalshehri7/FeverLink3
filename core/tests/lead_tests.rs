//! The lead: how far past the fastest game's clock an action is stamped, so that every game has it before it reaches the stamp.
//! It grows with the measured delay and with the speed, floors at three steps (one engine batch and the hand-off), and is a jitter
//! buffer: it covers the latest arrival seen recently plus a safety margin, at once, and shrinks slowly (JitterBuffer).

mod net;

use feverlink3_core::session::*;
use net::*;

const STEP: i64 = Lockstep::STEP;

#[test]
fn a_connection_with_no_delay_gets_the_floor_of_three_steps_at_normal_speed() {
    assert_eq!(3 * STEP, Lockstep::lead_for(1, 0.0));
    assert_eq!(Lockstep::lead_for(1, 0.0), Lockstep::lead_units(1));
}

#[test]
fn the_lead_never_shrinks_when_the_delay_or_the_speed_grows_and_stays_in_bounds() {
    let mut last = 0;
    for rtt in [0.0, 10.0, 40.0, 100.0, 200.0, 400.0, 800.0, 3000.0] {
        let lead = Lockstep::lead_for(1, rtt);
        assert!(lead >= last, "rtt {rtt}: {lead} < {last}");
        last = lead;
    }
    for rtt in [0.0, 100.0, 400.0] {
        let mut last = 0;
        for speed in 1..=4 {
            let lead = Lockstep::lead_for(speed, rtt);
            assert!(lead >= last, "speed {speed}: {lead} < {last}");
            last = lead;
        }
    }
    for speed in [i32::MIN, -1, 0, 1, 4, 99, i32::MAX] {
        for rtt in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0, 0.0, 1e9] {
            let lead = Lockstep::lead_for(speed, rtt);
            assert!((3 * STEP..=24 * STEP).contains(&lead), "speed {speed}, rtt {rtt}: {lead}");
            assert_eq!(0, lead % STEP, "a lead is whole steps");
        }
    }
}

#[test]
fn half_a_second_at_normal_speed_is_enough_on_a_good_connection_and_a_slow_one_is_longer() {
    let good = Lockstep::lead_for(1, 20.0);
    let slow = Lockstep::lead_for(1, 400.0);
    assert!(good <= 3 * STEP + STEP, "{good}");
    assert!(slow > good, "{slow} vs {good}");
    // in real time at normal speed (6 steps a second): well under a second for a good link
    assert!((good as f64 / STEP as f64) / Lockstep::STEPS_PER_SECOND < 0.8);
}

fn started(net: &mut Net, speed: i32) {
    net.hello_all(10_000, 0);
    net.host_call(HostSession::start);
    net.host_call(|h| h.set_speed(speed));
}

#[test]
fn the_host_lengthens_the_lead_at_once_when_a_player_turns_out_to_be_slow_and_tells_everyone() {
    let mut n = Net::new();
    let a = n.connect("A");
    let b = n.connect("B");
    started(&mut n, 1);
    n.set_latency(a, 0.020);
    n.set_latency(b, 0.600);
    let before = n.host.state().lead;
    n.run_for(6.0, 0.25);
    let want = Lockstep::lead_for_cover(1, 300.0 + JitterBuffer::SAFETY_MS);
    assert!(want > before);
    assert_eq!(want, n.host.state().lead, "the slowest player decides");
    for c in [a, b] {
        let session: SessionState = n.peer(c).last(MessageKind::Session);
        assert_eq!(want, session.lead, "every game was told");
    }
}

#[test]
fn the_lead_shrinks_slowly_a_step_at_a_time_after_the_latest_slow_round_trip_has_left_the_window() {
    let mut n = Net::new();
    let a = n.connect("A");
    started(&mut n, 4);
    n.set_latency(a, 0.600);
    n.run_for(6.0, 0.25);
    let long = n.host.state().lead;
    assert_eq!(Lockstep::lead_for_cover(4, 300.0 + JitterBuffer::SAFETY_MS), long);

    n.set_latency(a, 0.020);
    n.run_for(5.0, 0.25); // the latest slow round trip is still one of the last eight
    assert_eq!(long, n.host.state().lead, "still long a few seconds later: {}", n.host.state().lead);
    // then the buffer shrinks by 50 ms a second: at x4 a step is 42 ms, so about a step a second, never a jump
    let mut leads = vec![n.host.state().lead];
    for _ in 0..40 {
        n.run_for(0.5, 0.25);
        leads.push(n.host.state().lead);
    }
    for w in leads.windows(2) {
        assert!(w[1] <= w[0] && w[0] - w[1] <= 2 * STEP, "shrinks gradually: {leads:?}");
    }
    assert_eq!(
        Lockstep::lead_for_cover(4, 10.0 + JitterBuffer::SAFETY_MS),
        n.host.state().lead,
        "short again once the slow round trips are gone: {leads:?}"
    );
}

#[test]
fn a_late_clock_grows_the_buffer_at_once_and_it_shrinks_back_over_seconds() {
    let mut b = JitterBuffer::default();
    assert_eq!(15.0 + JitterBuffer::SAFETY_MS, b.update(15.0, 0.0), "the one-way time of the friend test's 30 ms link");
    // a clock 300 ms late (the friend test's peer clock gaps: 140 ms at the median, up to 825): covered at once
    assert_eq!(315.0 + JitterBuffer::SAFETY_MS, b.update(315.0, 1.0));
    // gone: 50 ms less every second, never below the arrival of now
    assert_eq!(285.0, b.update(15.0, 2.0));
    assert_eq!(135.0, b.update(15.0, 5.0));
    assert_eq!(15.0 + JitterBuffer::SAFETY_MS, b.update(15.0, 30.0));
    assert_eq!(JitterBuffer::SAFETY_MS, b.update(f64::NAN, 31.0), "not a number counts as nothing measured");
    // the lead of the buffer never drops below one engine batch and the hand-off (907af4e)
    for speed in 1..=4 {
        assert_eq!(Lockstep::lead_for(speed, 0.0), Lockstep::lead_for_cover(speed, 0.0));
        assert!(Lockstep::lead_for_cover(speed, 0.0) / STEP > i64::from(speed));
    }
    assert_eq!(
        Lockstep::lead_for_measured(4, 30.0, 400.0),
        Lockstep::lead_for_cover(4, Lockstep::cover_ms(30.0, 400.0))
    );
}

// ---------------------------------------------------------------- the measured clock cadence (bugs 4 and 6, 2026-10-18)

#[test]
fn the_lead_holds_one_engine_batch_and_the_hand_off_on_top_of_the_one_way_time() {
    // friend test 2026-10-18, x4 over 30 ms: the lead was 4 steps, one batch, so every game had at most one batch of room before its
    // barrier; now one batch (4 steps at x4) and the hand-off step come on top of the one-way time and the handling (2 steps at x4)
    assert_eq!(7 * STEP, Lockstep::lead_for(4, 30.0));
    assert_eq!(4 * STEP, Lockstep::lead_for(2, 30.0));
    assert_eq!(3 * STEP, Lockstep::lead_for(1, 30.0), "x1 as before: a batch is one step");
    for speed in 1..=4 {
        let batches = Lockstep::lead_for(speed, 0.0) / STEP / i64::from(speed);
        assert!(batches >= 1, "x{speed}: at least one batch of room");
    }
}

#[test]
fn a_clock_gap_of_one_batch_adds_nothing_and_the_excess_is_bounded() {
    for speed in 1..=4 {
        let batch_ms = Lockstep::BATCH_SECONDS * 1000.0;
        let step_ms = batch_ms;
        for rtt in [0.0, 30.0, 150.0, 300.0] {
            assert_eq!(
                Lockstep::lead_for(speed, rtt),
                Lockstep::lead_for_measured(speed, rtt, batch_ms),
                "x{speed} rtt {rtt}"
            );
            assert_eq!(Lockstep::lead_for(speed, rtt), Lockstep::lead_for_measured(speed, rtt, 0.0));
            let jitter = Lockstep::lead_for_measured(speed, rtt, step_ms + 100.0);
            assert!(jitter >= Lockstep::lead_for(speed, rtt));
            if speed >= 2 {
                assert!(
                    jitter > Lockstep::lead_for(speed, rtt),
                    "x{speed} rtt {rtt}: 100 ms of excess is a step at least"
                );
            }
            // a stall of seconds is not hidden behind the lead: at most GAP_EXCESS_MAX_MS more
            assert_eq!(
                Lockstep::lead_for_measured(speed, rtt, step_ms + Lockstep::GAP_EXCESS_MAX_MS),
                Lockstep::lead_for_measured(speed, rtt, 60_000.0)
            );
        }
    }
    for gap in [f64::NAN, f64::INFINITY, -5.0, 1e12] {
        let lead = Lockstep::lead_for_measured(4, 100.0, gap);
        assert!((3 * STEP..=24 * STEP).contains(&lead), "gap {gap}: {lead}");
    }
}

#[test]
fn the_meter_counts_only_the_gaps_of_a_running_game_and_ignores_rare_stalls() {
    let mut m = LeadMeter::default();
    let all = |_: u16| true;
    // x1: a clock line per step, 167 ms apart
    let mut at = 0.0;
    let mut t = 10_000;
    for _ in 0..40 {
        at += 0.167;
        t += STEP;
        m.clock(1, t, 1, at);
    }
    let base = m.gap_ms(at, all);
    assert!((160.0..175.0).contains(&base), "{base}");
    // standing at the barrier: keepalives every 250 ms with the same time and engine speed 0, for 5 s, then running again
    for _ in 0..20 {
        at += 0.25;
        m.clock(1, t, 0, at);
    }
    for _ in 0..3 {
        at += 0.167;
        t += STEP;
        m.clock(1, t, 1, at);
    }
    assert!((160.0..175.0).contains(&m.gap_ms(at, all)), "a hold is no gap of a running game: {}", m.gap_ms(at, all));
    // one late clock of 2 s among many lines: the latest arrival is covered (the jitter buffer), up to GAP_EXCESS_MAX_MS of lead
    at += 2.0;
    t += STEP;
    m.clock(1, t, 1, at);
    for _ in 0..30 {
        at += 0.167;
        t += STEP;
        m.clock(1, t, 1, at);
    }
    assert!((1990.0..2010.0).contains(&m.gap_ms(at, all)), "{}", m.gap_ms(at, all));
    assert_eq!(
        Lockstep::lead_for_measured(1, 0.0, m.gap_ms(at, all)),
        Lockstep::lead_for_measured(1, 0.0, 60_000.0),
        "bounded: a stall is not hidden behind the lead"
    );
    // gaps age out of the window; a player that left is forgotten
    assert_eq!(0.0, m.gap_ms(at + LeadMeter::GAP_WINDOW_SECONDS + 1.0, all));
    m.forget(1);
    assert_eq!(0.0, m.gap_ms(at, all));
    // round trips: the largest of the last eight (the latest arrival), of the slowest player that is here
    for ms in [100.0, 110.0, 105.0, 400.0, 102.0, 98.0, 101.0, 103.0] {
        m.rtt(2, ms);
    }
    m.rtt(3, 500.0);
    assert_eq!(400.0, m.rtt_ms(|id| id == 2), "the spike is covered while it is one of the last eight");
    for ms in [100.0, 101.0, 102.0, 103.0] {
        m.rtt(2, ms);
    }
    assert_eq!(103.0, m.rtt_ms(|id| id == 2), "and goes when it is older than that");
    assert_eq!(500.0, m.rtt_ms(all));
}

#[test]
fn keepalives_of_a_running_engine_between_two_batches_do_not_hide_the_gaps() {
    // friend test 2026-10-18 (x4, the guest's from_game.log): a moved line every batch, between two of them a line with the same time
    // (the 30-frame and real-time cadences) while the engine runs; the meter took the line before a move to have moved, so it kept
    // almost no gap (link.lead gap_ms=0) and the lead fell to its floor
    let mut m = LeadMeter::default();
    let all = |_: u16| true;
    let (mut at, mut t) = (0.0, 10_000);
    for i in 0..60 {
        at += 0.15;
        m.clock(1, t, 4, at); // keepalive, engine running
        at += 0.05 + if i % 10 == 9 { 0.3 } else { 0.0 };
        t += 4 * STEP;
        m.clock(1, t, 4, at);
    }
    let gap = m.gap_ms(at, all);
    assert!((480.0..520.0).contains(&gap), "the gaps between moves, a late one in ten among them: {gap}");
    assert!(Lockstep::lead_for_measured(4, 30.0, gap) > Lockstep::lead_for(4, 30.0));
}

/// A tiny deterministic generator for the jitter (no dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn clock_at(t: i64, speed: i32) -> Vec<u8> {
    json(&ClockMessage {
        t,
        speed,
        ahead: 3,
        need: None,
        safe: Some(t + 6 * STEP),
        pause: false,
        stop: None,
        hop: None,
    })
}

/// Two games at `speed` over a link of `rtt_ms` round trip with jitter, each sending a clock line per engine batch (`speed` steps)
/// with its frames late by up to `frame_jitter_ms` now and then; the host's lead after `seconds`.
fn run_link(rtt_ms: f64, frame_jitter_ms: f64, speed: i32, seconds: f64, seed: u64) -> (Net, i64) {
    let mut n = Net::new();
    let a = n.connect("A");
    started(&mut n, speed);
    let (mut rng, mut frames) = (Lcg(seed), Lcg(seed + 100));
    let step_s = Lockstep::BATCH_SECONDS;
    let batch = i64::from(speed) * STEP;
    let (mut t_host, mut t_a) = (10_000, 10_000);
    let (mut next_host, mut next_a) = (n.time(), n.time());
    let late = |r: &mut Lcg| if r.next() < 0.2 { frame_jitter_ms * r.next() / 1000.0 } else { 0.0 };
    for ms in 0..(seconds * 1000.0) as i64 {
        // the round trip: rtt_ms plus up to 30 % of jitter
        n.set_latency(a, (rtt_ms * (1.0 + 0.3 * rng.next())) / 1000.0);
        n.advance(0.001);
        let now = n.time();
        if now >= next_host {
            t_host += batch;
            n.host_send(MessageKind::Clock, &clock_at(t_host, speed));
            next_host = now + step_s + late(&mut frames);
        }
        if now >= next_a {
            t_a += batch;
            n.send(a, MessageKind::Clock, &clock_at(t_a, speed));
            next_a = now + step_s + late(&mut frames);
        }
        if ms % 250 == 0 {
            n.tick();
        }
    }
    let lead = n.host.state().lead;
    (n, lead)
}

#[test]
fn the_lead_converges_to_the_measured_round_trip_and_clock_jitter_and_stays_in_bounds() {
    for (rtt, seed) in [(30.0, 1), (150.0, 2), (300.0, 3)] {
        for speed in [1, 4] {
            let (n, steady) = run_link(rtt, 0.0, speed, 40.0, seed);
            // no jitter in the frames: the round trip alone (its jitter at most 30 %)
            assert!(steady >= Lockstep::lead_for(speed, rtt), "rtt {rtt} x{speed}: {steady}");
            // (one step of slack: the harness' clock lines are a millisecond apart at best, a hair over one step's real time)
            let most = Lockstep::lead_for_cover(speed, rtt * 1.3 / 2.0 + JitterBuffer::SAFETY_MS) + STEP;
            assert!(steady <= most, "rtt {rtt} x{speed}: {steady}");
            let (n2, jittery) = run_link(rtt, 300.0, speed, 40.0, seed);
            assert!(jittery >= steady, "rtt {rtt} x{speed}: frames late by up to 300 ms never shorten the lead");
            assert!(
                jittery
                    <= Lockstep::lead_for_cover(
                        speed,
                        Lockstep::cover_ms(rtt * 1.3, Lockstep::BATCH_SECONDS * 1000.0 + 302.0)
                            + JitterBuffer::SAFETY_MS
                    ),
                "rtt {rtt} x{speed}: {jittery} covers at most the largest gap"
            );
            assert!((3 * STEP..=24 * STEP).contains(&jittery));
            if speed == 4 {
                assert!(jittery > steady, "rtt {rtt}: at x4 a step is 42 ms, frames late by 300 ms are steps of lead");
                assert!(n2.host_log.iter().any(|l| l.starts_with("link.lead ")), "the change is logged");
            }
            let _ = n;
        }
    }
}

#[test]
fn a_higher_speed_gets_a_lead_that_covers_the_same_real_time() {
    let mut n = Net::new();
    let a = n.connect("A");
    started(&mut n, 1);
    n.set_latency(a, 0.100);
    n.run_for(6.0, 0.25);
    let at_1 = n.host.state().lead;
    n.host_call(|h| h.set_speed(3));
    let at_3 = n.host.state().lead;
    assert!(at_3 > at_1, "{at_3} vs {at_1}");
    assert_eq!(Lockstep::lead_for_cover(3, 50.0 + JitterBuffer::SAFETY_MS), at_3);
}

// ---------------------------------------------------------------- measured rates and batch boundaries (build timing, 2026-10-22)

#[test]
fn the_lead_counts_real_time_at_the_measured_batch_interval_and_the_nominal_one_changes_nothing() {
    // the nominal batch gives exactly the old lead, at every speed and cover
    let nominal = Lockstep::BATCH_SECONDS * 1000.0;
    for speed in [i32::MIN, 0, 1, 2, 3, 4, 7] {
        for cover in [0.0, 15.0, 125.0, 333.3, 600.0, f64::NAN] {
            assert_eq!(
                Lockstep::lead_for_cover(speed, cover),
                Lockstep::lead_for_cover_at(speed, cover, nominal),
                "x{speed} {cover}"
            );
            assert_eq!(Lockstep::lead_for_cover(speed, cover), Lockstep::lead_for_cover_at(speed, cover, f64::NAN));
        }
    }
    assert_eq!(nominal, Lockstep::batch_ms_or_nominal(None));
    assert_eq!(Lockstep::BATCH_MS_MIN, Lockstep::batch_ms_or_nominal(Some(5.0)), "a burst is no engine that fast");
    assert_eq!(
        Lockstep::BATCH_MS_MAX,
        Lockstep::batch_ms_or_nominal(Some(900.0)),
        "stretched batches do not shrink the lead for ever"
    );
    assert!((Lockstep::steps_per_second(4, 200.0) - 20.0).abs() < 1e-9);
    assert!((Lockstep::steps_per_second(4, nominal) - 24.0).abs() < 1e-9);
    // the real game of 2026-10-22 at x4: batches 200 ms apart, the largest clock gap 300 ms, a 10 ms round trip. Counted at 200 ms
    // the gap's excess is 100 ms (not 133) and the real time goes into steps at 20 a second (not 24): 9 steps instead of 11
    let measured = Lockstep::cover_ms_at(10.0, 300.0, 200.0) + JitterBuffer::SAFETY_MS;
    let old = Lockstep::cover_ms(10.0, 300.0) + JitterBuffer::SAFETY_MS;
    assert_eq!(
        (9 * STEP, 11 * STEP),
        (Lockstep::lead_for_cover_at(4, measured, 200.0), Lockstep::lead_for_cover(4, old))
    );
    // the floor stays one engine batch and the hand-off step, whatever the measure
    for speed in 1..=4 {
        for batch in [100.0, 200.0, 300.0] {
            assert!(Lockstep::lead_for_cover_at(speed, 0.0, batch) / STEP > i64::from(speed));
        }
    }

    // the meter: x4 lines of one full batch (4 steps) 200 ms apart, every fifth batch stretched to 400 ms by the smooth pacing, and
    // batches shortened at a stop (1 step) that say nothing about the interval
    let mut m = LeadMeter::default();
    let all = |_: u16| true;
    let (mut at, mut t) = (0.0, 10_000);
    for i in 0..60 {
        at += if i % 5 == 4 { 0.4 } else { 0.2 };
        t += if i % 7 == 6 { STEP } else { 4 * STEP };
        m.clock(1, t, 4, at);
    }
    let batch = m.batch_ms(at, 4, all).expect("measured");
    assert!((195.0..205.0).contains(&batch), "{batch}");
    assert_eq!(None, m.batch_ms(at, 2, all), "no full batch of x2 was seen");
    assert_eq!(None, m.batch_ms(at + LeadMeter::GAP_WINDOW_SECONDS + 1.0, 4, all), "aged out");
    // a second game with a faster engine: the fastest counts (the longer lead in steps)
    let (mut at2, mut t2) = (at - 40.0 * 0.17, 10_000);
    for _ in 0..40 {
        at2 += 0.17;
        t2 += 4 * STEP;
        m.clock(2, t2, 4, at2);
    }
    let both = m.batch_ms(at, 4, all).expect("measured");
    assert!((165.0..175.0).contains(&both), "{both}");
    assert!((195.0..205.0).contains(&m.batch_ms(at, 4, |id| id == 1).unwrap()), "a game that left does not count");
}

#[test]
fn the_host_counts_its_lead_with_the_batch_interval_the_clock_lines_show() {
    // two games at x4 over a 100 ms round trip, a clock line per full batch, about 200 ms apart (the real game of 2026-10-22)
    let mut n = Net::new();
    let a = n.connect("A");
    started(&mut n, 4);
    n.set_latency(a, 0.100);
    let (mut t_host, mut t_a, mut next) = (10_000, 10_000, n.time());
    for ms in 0..8000 {
        n.advance(0.001);
        if n.time() >= next {
            t_host += 4 * STEP;
            t_a += 4 * STEP;
            n.host_send(MessageKind::Clock, &clock_at(t_host, 4));
            n.send(a, MessageKind::Clock, &clock_at(t_a, 4));
            next = n.time() + 0.2;
        }
        if ms % 250 == 0 {
            n.tick();
        }
    }
    let lead = n.host.state().lead;
    let line =
        n.host_log.iter().rev().find(|l| l.starts_with("link.lead ")).expect("the lead's change is logged").clone();
    assert!(line.contains(" batch_ms=20") && line.contains(" lead_steps=8 "), "{line}");
    // the one-way 50 ms and the safety, at 20 steps a second: ceil(0.13 s * 20) + 1 + 4 = 8 steps; counted at the nominal 167 ms
    // batch (24 steps a second, and 33 ms of every 200 ms gap taken for jitter) it was 9
    assert_eq!(8 * STEP, lead, "{line}");
    assert_eq!(9 * STEP, Lockstep::lead_for_cover(4, Lockstep::cover_ms(100.0, 201.0) + JitterBuffer::SAFETY_MS));
}

#[test]
fn a_running_checksum_round_is_stamped_on_a_batch_boundary_with_its_marks_a_batch_apart() {
    for (t0, speed) in [(10_000, 4), (10_200, 4), (10_600, 3), (10_200, 2), (10_200, 1)] {
        let mut n = Net::new();
        let a = n.connect("A");
        n.hello_all(t0, 0);
        n.host_call(HostSession::start);
        n.host_call(|h| h.set_speed(speed));
        n.host_send(MessageKind::Clock, &clock_at(t0 + 3 * STEP, speed));
        n.host_call(HostSession::request_checksum);
        let r = n.peer(a).last::<ChecksumRequestMessage>(MessageKind::ChecksumRequest);
        let batch = STEP * i64::from(speed);
        assert!(!r.paused);
        assert_eq!(0, r.at % batch, "x{speed} from {t0}: stamp {} on a batch boundary", r.at);
        assert_eq!(
            u32::try_from(speed).unwrap(),
            r.spread,
            "the marks a batch apart: every one where a full batch ends"
        );
        assert!(r.at >= n.host.future_stamp(), "never before a promise or the lead");
        assert!(r.at < n.host.future_stamp() + batch, "at most one batch later");
    }
    assert_eq!(Lockstep::round_up_to_batch(10_000, 4), 10_400);
    assert_eq!(Lockstep::round_up_to_batch(10_400, 4), 10_400);
    assert_eq!(Lockstep::round_up_to_batch(10_401, 4), 11_200);
    assert_eq!(Lockstep::round_up_to_batch(10_200, 1), 10_200);
    assert_eq!(Lockstep::round_up_to_batch(10_200, 9), Lockstep::round_up_to_batch(10_200, 4));
}
