//! The launcher's part of the latency logs (owner list 2026-10-17, items 4 and 6; `docs/research/TODO.md` "Logging for 4 and 6").
//! Observation only: nothing here changes what a line says to the session, only the stamps it carries ([`Hops`]).
//!
//! A clock or action line from this game's mod carries `hop.g` (the mod's write); [`LinkWatch::stamp_read`] adds `hop.l` (this
//! launcher read it). A peer clock or an action on its way to this game carries what the sender and the host added;
//! [`LinkWatch::stamp_write`] adds `hop.w` (written to `to_game.log`), this machine's clock offset to the host's (`ro`) and its round
//! trip (`rr`), and the mod adds its read. Each hop is measured where both its ends are known on one clock: the mod-to-launcher hop
//! on the sending machine, the launcher-to-host and host-to-launcher hops with the measured offsets (on one PC they are 0).
//!
//! Once a minute two kinds of line go to `session.log` (format `name=min/median/max`, ms, with `name_n=count`):
//!   `link.launcher wall=<ms> secs=<s> role=<host|player> own_clock_n own_clock_gap_ms g2l_ms own_actions act_g2l_ms rtt_ms off_ms`
//!   `link.launcher.peer wall=<ms> peer=<id> peerclock_n peerclock_gap_ms l2h_ms h2w_ms e2e_ms actions act_e2e_ms`
//! `own_clock_gap_ms` is the cadence at which this launcher reads its own mod's clock lines: a gap there is the game (its frame or
//! its file write), not the network. `peerclock_gap_ms` is the cadence at which a peer's clocks reach this game's `to_game.log`.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use feverlink3_core::session::linkstats::{Arrivals, Samples};
use feverlink3_core::session::{Hops, MessageKind};
use serde_json::Value;

/// This machine's wall clock, ms since 1970 (0 if the clock is before 1970).
pub fn wall_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Debug, Default)]
struct PeerWatch {
    clocks: Arrivals,
    l2h: Samples,
    h2w: Samples,
    e2e: Samples,
    actions: usize,
    act_e2e: Samples,
}

/// What one launcher measures of the link, per minute.
#[derive(Debug)]
pub struct LinkWatch {
    role: &'static str,
    /// Where arrival times count from (fixed, so a gap across the minute's end is measured right).
    origin: Instant,
    /// The start of the current minute.
    started: Instant,
    own_clocks: Arrivals,
    g2l: Samples,
    own_actions: usize,
    act_g2l: Samples,
    rtt: Samples,
    peers: BTreeMap<u16, PeerWatch>,
    /// This machine's wall clock minus the host's (ms): 0 on the host, a player's as the host's pings report it.
    pub offset: Option<i64>,
    /// This machine's round trip to the host (ms): 0 on the host.
    pub rtt_ms: Option<f64>,
}

impl LinkWatch {
    pub const MINUTE_SECONDS: u64 = 60;

    pub fn new(role: &'static str) -> LinkWatch {
        let host = role == "host";
        let now = Instant::now();
        LinkWatch {
            role,
            origin: now,
            started: now,
            own_clocks: Arrivals::default(),
            g2l: Samples::default(),
            own_actions: 0,
            act_g2l: Samples::default(),
            rtt: Samples::default(),
            peers: BTreeMap::new(),
            offset: host.then_some(0),
            rtt_ms: host.then_some(0.0),
        }
    }

    /// A player's round trip as the host last reported it (once per ping).
    pub fn rtt_sample(&mut self, ms: f64) {
        self.rtt.push(ms);
    }

    /// A clock or action line this launcher just read from its game: stamps `hop.l` when the mod started stamps (`hop.g`), and
    /// measures the mod-to-launcher hop (one machine, one clock). Any other line, or one without stamps, is returned as it was.
    pub fn stamp_read(&mut self, kind: MessageKind, payload: Vec<u8>, now_wall: i64, now: Instant) -> Vec<u8> {
        if kind == MessageKind::Clock {
            self.own_clocks.arrived(self.seconds(now));
        } else if kind == MessageKind::Action {
            self.own_actions += 1;
        } else {
            return payload;
        }
        let Some((mut v, mut hop)) = with_hops(&payload) else { return payload };
        hop.l = Some(now_wall);
        if let Some(g) = hop.g.filter(|g| Hops::plausible_ms(*g)) {
            let d = (now_wall - g) as f64;
            if kind == MessageKind::Clock {
                self.g2l.push(d)
            } else {
                self.act_g2l.push(d)
            }
        }
        put_hops(&mut v, &hop).unwrap_or(payload)
    }

    /// A peer clock or an action this launcher is about to write to its game: stamps `hop.w`, `ro` and `rr` when the line carries
    /// stamps, and measures the hops whose ends are known on one clock. Any other line is returned as it was.
    pub fn stamp_write(&mut self, kind: MessageKind, payload: Vec<u8>, now_wall: i64, now: Instant) -> Vec<u8> {
        if kind != MessageKind::PeerClock && kind != MessageKind::Action {
            return payload;
        }
        let seconds = self.seconds(now);
        let Ok(mut v) = serde_json::from_slice::<Value>(&payload) else { return payload };
        let from = v.get("from").and_then(Value::as_u64).and_then(|f| u16::try_from(f).ok()).unwrap_or(0);
        let peer = self.peers.entry(from).or_default();
        if kind == MessageKind::PeerClock {
            peer.clocks.arrived(seconds);
        } else {
            peer.actions += 1;
        }
        let Some(mut hop) = v.get("hop").and_then(|h| serde_json::from_value::<Hops>(h.clone()).ok()) else {
            return payload;
        };
        hop.w = Some(now_wall);
        hop.ro = self.offset;
        hop.rr = self.rtt_ms.filter(|r| r.is_finite()).map(|r| r.round() as i64);
        // on the host's clock: a stamp x taken on a machine with offset o is x - o there
        let on_host = |x: Option<i64>, o: Option<i64>| Some(x? - o?);
        let l = on_host(hop.l, hop.so);
        let g = on_host(hop.g, hop.so);
        let w = on_host(hop.w, hop.ro);
        let gap = |a: Option<i64>, b: Option<i64>| Some((b? - a?) as f64);
        if kind == MessageKind::PeerClock {
            if let Some(d) = gap(l, hop.h) {
                peer.l2h.push(d);
            }
            if let Some(d) = gap(hop.h, w) {
                peer.h2w.push(d);
            }
            if let Some(d) = gap(g, w) {
                peer.e2e.push(d);
            }
        } else if let Some(d) = gap(g, w) {
            peer.act_e2e.push(d);
        }
        put_hops(&mut v, &hop).unwrap_or(payload)
    }

    /// The minute's lines, once a minute (nothing before that); then the minute starts over.
    pub fn minute(&mut self, now: Instant) -> Vec<String> {
        let secs = now.saturating_duration_since(self.started).as_secs();
        if secs < Self::MINUTE_SECONDS {
            return Vec::new();
        }
        let wall = wall_ms();
        let mut own = format!("link.launcher wall={wall} secs={secs} role={}", self.role);
        own.push_str(&self.own_clocks.gaps.describe("own_clock_gap_ms"));
        own.push_str(&self.g2l.describe("g2l_ms"));
        let _ = write!(own, " own_actions={}", self.own_actions);
        own.push_str(&self.act_g2l.describe("act_g2l_ms"));
        own.push_str(&self.rtt.describe("rtt_ms"));
        match self.offset {
            Some(o) => {
                let _ = write!(own, " off_ms={o}");
            }
            None => own.push_str(" off_ms=?"),
        }
        let mut out = vec![own];
        for (id, p) in &self.peers {
            let mut line = format!("link.launcher.peer wall={wall} peer={id}");
            line.push_str(&p.clocks.gaps.describe("peerclock_gap_ms"));
            line.push_str(&p.l2h.describe("l2h_ms"));
            line.push_str(&p.h2w.describe("h2w_ms"));
            line.push_str(&p.e2e.describe("e2e_ms"));
            let _ = write!(line, " actions={}", p.actions);
            line.push_str(&p.act_e2e.describe("act_e2e_ms"));
            out.push(line);
        }
        self.started = now;
        self.own_clocks.gaps.clear();
        self.g2l.clear();
        self.own_actions = 0;
        self.act_g2l.clear();
        self.rtt.clear();
        for p in self.peers.values_mut() {
            p.clocks.gaps.clear();
            p.l2h.clear();
            p.h2w.clear();
            p.e2e.clear();
            p.actions = 0;
            p.act_e2e.clear();
        }
        out
    }

    /// A peer left: its next clock does not count the outage as a gap.
    pub fn forget(&mut self, peer: u16) {
        self.peers.remove(&peer);
    }

    fn seconds(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.origin).as_secs_f64()
    }
}

/// The payload as JSON and its `hop` object; `None` when it has none (an older mod, or not JSON).
fn with_hops(payload: &[u8]) -> Option<(Value, Hops)> {
    let v = serde_json::from_slice::<Value>(payload).ok()?;
    let hop = serde_json::from_value::<Hops>(v.get("hop")?.clone()).ok()?;
    Some((v, hop))
}

fn put_hops(v: &mut Value, hop: &Hops) -> Option<Vec<u8>> {
    let obj = v.as_object_mut()?;
    obj.insert("hop".to_owned(), serde_json::to_value(hop).ok()?);
    serde_json::to_vec(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const W: i64 = 1_760_000_000_000;

    #[test]
    fn a_clock_from_the_game_gets_the_launchers_stamp_and_the_mods_hop_is_measured() {
        let mut lw = LinkWatch::new("player");
        let t0 = lw.started;
        let line = br#"{"t":200,"speed":1,"ahead":3,"hop":{"g":1760000000000}}"#.to_vec();
        let out = lw.stamp_read(MessageKind::Clock, line, W + 7, t0);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["hop"]["l"], W + 7);
        assert_eq!(v["hop"]["g"], W);
        assert_eq!(v["t"], 200);
        // a line without stamps (an older mod) and any other kind go through unchanged
        let plain = br#"{"t":400,"speed":1,"ahead":3}"#.to_vec();
        assert_eq!(lw.stamp_read(MessageKind::Clock, plain.clone(), W + 9, t0 + Duration::from_millis(250)), plain);
        let hello = br#"{"time":5,"hop":{"g":1}}"#.to_vec();
        assert_eq!(lw.stamp_read(MessageKind::Hello, hello.clone(), W, t0), hello);
        let lines = lw.minute(t0 + Duration::from_secs(61));
        assert!(lines[0].starts_with("link.launcher wall="), "{lines:?}");
        assert!(
            lines[0].contains(" role=player own_clock_gap_ms_n=1 own_clock_gap_ms=250/250/250 g2l_ms_n=1 g2l_ms=7/7/7"),
            "{lines:?}"
        );
        assert!(lines[0].ends_with(" off_ms=?"), "{lines:?}");
    }

    #[test]
    fn a_peer_clock_to_the_game_gets_the_receivers_stamps_and_every_hop_on_the_hosts_clock() {
        let mut lw = LinkWatch::new("player");
        lw.offset = Some(-300);
        lw.rtt_ms = Some(41.6);
        let t0 = lw.started;
        // the sender runs 500 ms ahead of the host: g and l are on its clock, h on the host's, w on this one's (300 behind)
        let pc = format!(
            r#"{{"from":0,"t":200,"ahead":3,"hop":{{"g":{},"l":{},"h":{},"so":500,"sr":0}}}}"#,
            W + 500,
            W + 510,
            W + 40
        );
        let out = lw.stamp_write(MessageKind::PeerClock, pc.into_bytes(), W - 300 + 90, t0);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["hop"]["w"], W - 210);
        assert_eq!(v["hop"]["ro"], -300);
        assert_eq!(v["hop"]["rr"], 42);
        let lines = lw.minute(t0 + Duration::from_secs(60));
        let peer = lines.iter().find(|l| l.starts_with("link.launcher.peer ")).expect("a peer line");
        // l2h: 40 - 10 = 30; h2w: 90 - 40 = 50; e2e: 90 - 0 = 90
        assert!(peer.contains(" peer=0 peerclock_gap_ms_n=0 l2h_ms_n=1 l2h_ms=30/30/30 h2w_ms_n=1 h2w_ms=50/50/50 e2e_ms_n=1 e2e_ms=90/90/90"), "{peer}");
        assert!(lw.minute(t0 + Duration::from_secs(90)).is_empty(), "the next minute has not passed");
    }

    #[test]
    fn an_action_is_measured_end_to_end_and_keeps_its_body() {
        let mut lw = LinkWatch::new("host");
        let t0 = lw.started;
        let a = format!(
            r#"{{"stamp":2000,"seq":1,"body":"{{x}}","from":1,"hop":{{"g":{},"l":{},"h":{},"so":0}}}}"#,
            W,
            W + 1,
            W + 30
        );
        let out = lw.stamp_write(MessageKind::Action, a.into_bytes(), W + 31, t0);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["body"], "{x}");
        assert_eq!(v["hop"]["ro"], 0);
        let lines = lw.minute(t0 + Duration::from_secs(60));
        assert!(
            lines.iter().any(|l| l.contains(" peer=1 ") && l.contains(" actions=1 act_e2e_ms_n=1 act_e2e_ms=31/31/31")),
            "{lines:?}"
        );
    }
}
