//! The lead: how far past the fastest game's clock an action is stamped, so that every game has it before it reaches the stamp.
//! It grows with the measured delay and with the speed, floors at three steps, and shrinks only after staying lower for a while.

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
    let want = Lockstep::lead_for(1, 600.0);
    assert!(want > before);
    assert_eq!(want, n.host.state().lead, "the slowest player decides");
    for c in [a, b] {
        let session: SessionState = n.peer(c).last(MessageKind::Session);
        assert_eq!(want, session.lead, "every game was told");
    }
}

#[test]
fn the_lead_shrinks_only_after_staying_lower_for_eight_seconds() {
    let mut n = Net::new();
    let a = n.connect("A");
    started(&mut n, 1);
    n.set_latency(a, 0.600);
    n.run_for(6.0, 0.25);
    let long = n.host.state().lead;
    assert_eq!(Lockstep::lead_for(1, 600.0), long);

    n.set_latency(a, 0.020);
    n.run_for(5.0, 0.25); // the smoothed round trip is already low, but not for long enough
    assert!(
        n.host.state().lead > Lockstep::lead_for(1, 100.0),
        "still long after a few seconds: {}",
        n.host.state().lead
    );
    n.run_for(30.0, 0.25);
    assert_eq!(Lockstep::lead_for(1, 20.0), n.host.state().lead, "short again once it stayed low");
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
    assert_eq!(Lockstep::lead_for(3, 100.0), at_3);
}
