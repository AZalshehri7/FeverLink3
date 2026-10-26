//! Random sessions: players joining, dropping silently or loudly, coming back (also while the host still believes in
//! their old connection), leaving, being removed, time passing, the host starting and syncing, junk in between.
//! After every step the invariants that matter must hold: slots never over the limit, ids never given twice, a
//! returner that is let in is the player it claimed to be, names unique, and no (sender, counter) nonce ever sealed
//! twice. Seeded, so a failure repeats.

mod net;

use std::collections::{BTreeMap, BTreeSet};

use feverlink3_core::session::*;
use net::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len() as u64) as usize])
    }
}

/// A player the model keeps track of: its connection, the name it asked for, and the ticket its welcome gave.
struct Tracked {
    conn: ConnectionId,
    name: String,
    ticket: Option<ResumeTicket>,
}

struct Model {
    n: Net,
    rng: Rng,
    peers: Vec<Tracked>,
    /// Connections the transport was told about, or that the host closed: no longer bound to a player.
    gone: BTreeSet<ConnectionId>,
    tokens: BTreeMap<u16, Vec<u8>>,
    max_id: u16,
    /// The highest counter each sender has sealed so far, the host (sender 0) included.
    last_counter: BTreeMap<u16, u64>,
    uplink_seen: usize,
    events_seen: usize,
    steps: BTreeMap<&'static str, u32>,
}

const MAX: usize = 8;

impl Model {
    fn new(seed: u64) -> Model {
        let options = HostOptions { max_players: MAX, ..HostOptions::default() };
        Model {
            n: Net::with(options),
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            peers: Vec::new(),
            gone: BTreeSet::new(),
            tokens: BTreeMap::new(),
            max_id: 0,
            last_counter: BTreeMap::new(),
            uplink_seen: 0,
            events_seen: 0,
            steps: BTreeMap::new(),
        }
    }

    fn count(&mut self, what: &'static str) {
        *self.steps.entry(what).or_default() += 1;
    }

    fn live(&self) -> Vec<ConnectionId> {
        self.peers
            .iter()
            .map(|p| p.conn)
            .filter(|c| self.n.reachable.contains(c) && self.n.peer(*c).session.state() == PlayerState::Joined)
            .collect()
    }

    fn name(&mut self) -> String {
        let pool = ["Ann", "Bob", "Cy", "Dee"];
        format!("{}{}", pool[self.rng.below(4) as usize], self.rng.below(3))
    }

    /// A connection came up; look at how the host answered.
    fn welcomed(&mut self, conn: ConnectionId, name: String, claimed: Option<ResumeTicket>) {
        let peer = self.n.peer(conn);
        let Some(w) = peer.welcomes.first() else {
            let reason = peer.rejected.as_ref().map(|r| r.reason.clone());
            assert_eq!(Some("full".to_owned()), reason, "a join is refused only because the session is full");
            let roster = self.n.host.roster();
            assert!(roster.len() >= MAX, "refused as full with only {} slots held", roster.len());
            self.gone.insert(conn);
            return;
        };
        let (id, resumed, grant) = (w.you, w.resumed, w.resume.clone().expect("a grant"));
        match (&claimed, resumed) {
            (Some(t), true) => {
                assert_eq!(t.id, id, "a returner gets the id it claimed");
                assert_eq!(Some(&grant.token), self.tokens.get(&id), "and the token of that slot");
            }
            (_, false) => {
                assert!(id > self.max_id, "a new player gets an id nobody had: {id} after {}", self.max_id);
                self.max_id = id;
                self.tokens.insert(id, grant.token.clone());
            }
            (None, true) => panic!("a join with no claim cannot be a return"),
        }
        let ticket = self.n.peer(conn).session.ticket().cloned();
        self.peers.push(Tracked { conn, name, ticket });
    }

    fn step(&mut self) {
        match self.rng.below(100) {
            0..=14 => {
                self.count("join");
                let name = self.name();
                let c = self.n.connect(&name);
                self.welcomed(c, name, None);
            }
            15..=24 => {
                self.count("lose");
                if let Some(c) = self.rng.pick(&self.live()) {
                    self.n.lose(c);
                    self.gone.insert(c);
                }
            }
            25..=29 => {
                self.count("cut");
                if let Some(c) = self.rng.pick(&self.live()) {
                    self.n.cut(c);
                }
            }
            30..=47 => {
                self.count("return");
                let withticket: Vec<usize> =
                    (0..self.peers.len()).filter(|&i| self.peers[i].ticket.is_some()).collect();
                if let Some(i) = self.rng.pick(&withticket) {
                    let (name, ticket) = (self.peers[i].name.clone(), self.peers[i].ticket.clone().unwrap());
                    let c = self.n.connect_resuming(&name, ticket.clone());
                    self.welcomed(c, name, Some(ticket));
                }
            }
            48..=53 => {
                self.count("leave");
                if let Some(c) = self.rng.pick(&self.live()) {
                    self.n.leave(c);
                    self.gone.insert(c);
                    self.peers.iter_mut().filter(|p| p.conn == c).for_each(|p| p.ticket = None);
                }
            }
            54..=58 => {
                self.count("kick");
                let id = self.rng.below(u64::from(self.max_id) + 3) as u16;
                self.n.host_call(|h| h.kick(id));
                for p in &mut self.peers {
                    if p.ticket.as_ref().is_some_and(|t| t.id == id) {
                        p.ticket = None;
                    }
                }
            }
            59..=78 => {
                self.count("time");
                let secs = [0.2, 1.0, 1.0, 5.0, 31.0, 120.0, 400.0][self.rng.below(7) as usize];
                self.n.run_for(secs, if secs > 30.0 { 4.0 } else { 1.0 });
            }
            79..=86 => {
                self.count("talk");
                if let Some(c) = self.rng.pick(&self.live()) {
                    match self.rng.below(3) {
                        0 => self.n.send(c, MessageKind::Chat, &json(&ChatMessage::new("hi"))),
                        1 => self.n.hello(c, 20_000, 1),
                        _ => self.n.send(
                            c,
                            MessageKind::Clock,
                            &json(&ClockMessage {
                                t: 21_000,
                                speed: 1,
                                ahead: 3,
                                need: None,
                                safe: None,
                                pause: false,
                                stop: None,
                                hop: None,
                            }),
                        ),
                    }
                }
            }
            _ => {
                self.count("host");
                match self.rng.below(5) {
                    0 => self.n.host_call(HostSession::start),
                    1 => self.n.host_call(HostSession::pause),
                    2 => self.n.host_call(|h| h.set_speed(2)),
                    3 => self.n.host_call(|h| h.start_resync("model")),
                    _ => self.n.host_hello(20_000, 1),
                }
            }
        }
    }

    fn check(&mut self, seed: u64, step: usize) {
        let ctx = format!("seed {seed}, step {step}");
        let roster = self.n.host.roster();
        let ids: Vec<u16> = roster.iter().map(|e| e.id).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]) && ids.first() == Some(&0), "{ctx}: roster ids {ids:?}");
        assert!(roster.len() <= MAX, "{ctx}: {} slots held", roster.len());
        let connected: Vec<u16> =
            roster.iter().filter(|e| e.state == ConnectionState::Connected).map(|e| e.id).collect();
        let players: Vec<u16> = self.n.host.players().keys().copied().collect();
        assert_eq!(connected, players, "{ctx}: the connected roster entries are the players");
        let mut names: Vec<String> = roster.iter().map(|e| e.name.to_uppercase()).collect();
        names.sort();
        names.dedup();
        assert_eq!(roster.len(), names.len(), "{ctx}: names are unique: {roster:?}");

        // honest players are never refused: no replay-window trouble after a return, no frame from a stale sender
        assert_eq!(0, self.n.host.dropped(), "{ctx}: the host refused a frame: {:?}", self.n.host_log.last());

        // a player the host has not heard of leaving or timing out is a player of the session
        for p in &self.peers {
            let peer = self.n.peer(p.conn);
            let bound = self.n.reachable.contains(&p.conn)
                && peer.session.state() == PlayerState::Joined
                && !self.gone.contains(&p.conn)
                && !self.n.closed.contains(&p.conn);
            if bound {
                let id = peer.id();
                assert!(players.contains(&id), "{ctx}: {} (#{id}) is connected but not a player of the host", p.name);
            }
        }
        // and the other way: every connected player of the host is some live connection's
        for &id in players.iter().filter(|&&id| id != 0) {
            let owner = self.peers.iter().any(|p| {
                self.n.peer(p.conn).session.id() == Some(id)
                    && !self.gone.contains(&p.conn)
                    && !self.n.closed.contains(&p.conn)
            });
            assert!(owner, "{ctx}: player #{id} has no connection behind it");
        }

        // no nonce twice: what each player sealed only grows, across reconnections too, and so does what the host sealed
        for &(sender, counter) in &self.n.uplink[self.uplink_seen..] {
            let last = self.last_counter.entry(sender).or_insert(0);
            assert!(counter > *last, "{ctx}: sender {sender} sealed counter {counter} after {last}");
            *last = counter;
        }
        self.uplink_seen = self.n.uplink.len();
        for e in &self.n.events[self.events_seen..] {
            if let HostOutput::SendToConnection { frame, .. } = e {
                if frame.header.sender == HostSession::HOST_ID {
                    let last = self.last_counter.entry(HostSession::HOST_ID).or_insert(0);
                    assert!(
                        frame.header.counter > *last,
                        "{ctx}: the host sealed counter {} twice",
                        frame.header.counter
                    );
                    *last = frame.header.counter;
                }
            }
        }
        self.events_seen = self.n.events.len();
    }
}

fn run(seed: u64, steps: usize) -> BTreeMap<&'static str, u32> {
    let mut m = Model::new(seed);
    for step in 0..steps {
        m.step();
        m.check(seed, step);
    }
    m.steps
}

/// 20 sessions of 200 steps in the normal run; `FEVERLINK3_MODEL_SEEDS=500 cargo test -p feverlink3_core --test all
/// model_tests` runs more of them, for a deeper search. The sessions are independent (each its own seed and model), so
/// they run on a few threads at once: 7 s on one thread was the slowest test of the core (2026-10-26).
#[test]
fn random_sessions_keep_their_invariants() {
    let seeds = std::env::var("FEVERLINK3_MODEL_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(20u64);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8) as u64;
    let mut totals: BTreeMap<&str, u32> = BTreeMap::new();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    let mut part: BTreeMap<&'static str, u32> = BTreeMap::new();
                    for seed in (1..=seeds).filter(|seed| seed % threads == t) {
                        for (what, n) in run(seed, 200) {
                            *part.entry(what).or_default() += n;
                        }
                    }
                    part
                })
            })
            .collect();
        for w in workers {
            // a failed invariant panics in its worker: join() hands that panic on, with the seed and step in its text
            for (what, n) in w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)) {
                *totals.entry(what).or_default() += n;
            }
        }
    });
    // the run really did exercise everything
    for what in ["join", "lose", "cut", "return", "leave", "kick", "time", "talk", "host"] {
        assert!(totals.get(what).copied().unwrap_or(0) > 100, "{what}: {totals:?}");
    }
}
