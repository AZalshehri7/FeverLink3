//! The admission exchange of Steam invites over Steam P2P (`docs/PROTOCOL.md`, "Steam invites"): a friend who
//! clicked "Join game" or accepted an invite connects to the host's Steam ID on [`ADMIT_PORT`] and asks to be let in;
//! the host's [`Gate`] decides, and only an admitted account receives the session key, on that connection. Steam's
//! networking authenticates the friend's account (the identity the connection reports is Steam's, not anything the
//! friend wrote) and encrypts the connection; the session itself then runs on virtual port 0 as for a pasted code.
//!
//! Nothing here writes the key or the ticket to a log line.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use feverlink3_core::admission::{
    is_individual_account, AdmitMessage, ConnectToken, Decision, Gate, Pending, RefuseReason, REQUEST_TIMEOUT,
};
use feverlink3_core::crypto::SessionKey;

use crate::steamnet::{CoreError, CoreRef, End, Handle, Owner};

/// The virtual port of the admission exchange (the session has 0, the probe 1, the parallel transfers 2).
pub const ADMIT_PORT: i32 = 3;
/// A connection that has not sent its request within this long is closed.
pub const REQUEST_WAIT: Duration = Duration::from_secs(15);
/// Admission connections a host keeps at once; more are closed at once.
pub const MAX_CONNECTIONS: usize = 16;
/// How long a friend waits for a route to the host.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// What the friend's window says while the host decides.
pub const STATUS_WAITING: &str = "waiting for the host to let you in";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A short form of an account for the log: the last digits are enough to tell friends apart.
pub fn short_id(id: u64) -> String {
    let text = id.to_string();
    format!("...{}", &text[text.len().saturating_sub(4)..])
}

// ---------------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------------

struct Conn {
    account: Option<u64>,
    opened: Instant,
    /// Its request is in the gate's queue.
    asked: bool,
    /// An answer was sent; the connection is closing.
    done: bool,
}

struct HostState {
    gate: Gate,
    key: [u8; SessionKey::KEY_SIZE],
    conns: HashMap<Handle, Conn>,
    log: Vec<String>,
    stopped: bool,
}

struct HostShared {
    core: CoreRef,
    state: Mutex<HostState>,
}

impl HostShared {
    /// Sends the last answer of a connection and closes it (Steam's linger delivers the answer first).
    fn finish(&self, state: &mut HostState, conn: Handle, answer: AdmitMessage) {
        if let Some(c) = state.conns.get_mut(&conn) {
            c.done = true;
        }
        let _ = self.core.send(conn, &answer.encode());
        self.core.close(conn);
    }

    fn handle_request(&self, state: &mut HostState, conn: Handle, data: &[u8]) {
        let Some(c) = state.conns.get(&conn) else { return };
        let (Some(account), false, false) = (c.account, c.asked, c.done) else { return };
        let Some(msg) = AdmitMessage::decode(data) else {
            state.log.push(format!("invites: an unreadable request from {}: refused", short_id(account)));
            return self.finish(state, conn, AdmitMessage::Refused { reason: RefuseReason::Malformed });
        };
        let name = match &msg {
            AdmitMessage::Request { name, .. } => name.clone(),
            _ => String::new(),
        };
        let invited = matches!(&msg, AdmitMessage::Request { ticket: Some(_), .. });
        match state.gate.on_request(account, &msg, Instant::now()) {
            Decision::Admit => {
                let how = if invited { "came through your invite" } else { "admitted earlier in this session" };
                state.log.push(format!("invites: {name} ({}) {how}: admitted", short_id(account)));
                let key = state.key;
                self.finish(state, conn, AdmitMessage::Admitted { key });
            }
            Decision::Ask => {
                // a second request of the same account replaces the first one's connection
                let older: Vec<Handle> = state
                    .conns
                    .iter()
                    .filter(|(h, c)| **h != conn && c.account == Some(account) && c.asked && !c.done)
                    .map(|(h, _)| *h)
                    .collect();
                for h in older {
                    self.finish(state, h, AdmitMessage::Refused { reason: RefuseReason::Busy });
                }
                if let Some(c) = state.conns.get_mut(&conn) {
                    c.asked = true;
                }
                state.log.push(format!("invites: join request from {name} ({}): asking the host", short_id(account)));
                let _ = self.core.send(conn, &AdmitMessage::Waiting.encode());
            }
            Decision::Refuse(reason) => {
                state.log.push(format!(
                    "invites: request from {name} ({}) refused: {}",
                    short_id(account),
                    reason.as_str()
                ));
                self.finish(state, conn, AdmitMessage::Refused { reason });
            }
        }
    }

    /// The connection of `account` that waits for the host's answer.
    fn waiting_conn(state: &HostState, account: u64) -> Option<Handle> {
        state.conns.iter().find(|(_, c)| c.account == Some(account) && c.asked && !c.done).map(|(h, _)| *h)
    }
}

impl Owner for HostShared {
    fn opened(&self, conn: Handle, _incoming: bool, remote: Option<u64>) {
        let mut state = lock(&self.state);
        let account = remote.filter(|id| is_individual_account(*id));
        if state.stopped || account.is_none() || state.conns.len() >= MAX_CONNECTIONS {
            drop(state);
            self.core.close(conn);
            return;
        }
        state.conns.insert(conn, Conn { account, opened: Instant::now(), asked: false, done: false });
    }

    fn message(&self, conn: Handle, data: &[u8]) {
        let mut state = lock(&self.state);
        self.handle_request(&mut state, conn, data);
    }

    fn closed(&self, conn: Handle, _end: End) {
        let mut state = lock(&self.state);
        let Some(c) = state.conns.remove(&conn) else { return };
        if let (Some(account), true, false) = (c.account, c.asked, c.done) {
            if HostShared::waiting_conn(&state, account).is_none() {
                state.gate.withdraw(account);
                state.log.push(format!("invites: {} stopped waiting", short_id(account)));
            }
        }
    }
}

/// The host's admission listener: one per session hosted over Steam. Dropping it refuses whoever still waits
/// (`ended`) and stops listening.
pub struct AdmitHost {
    shared: Arc<HostShared>,
    listener: Handle,
}

impl AdmitHost {
    /// Listens on [`ADMIT_PORT`]. `key` is what an admitted friend receives.
    pub fn start(core: &CoreRef, gate: Gate, key: &SessionKey) -> Result<AdmitHost, CoreError> {
        let shared = Arc::new(HostShared {
            core: core.clone(),
            state: Mutex::new(HostState {
                gate,
                key: *key.key(),
                conns: HashMap::new(),
                log: Vec::new(),
                stopped: false,
            }),
        });
        let owner: Arc<dyn Owner> = shared.clone();
        let listener = core.listen(ADMIT_PORT, owner)?;
        Ok(AdmitHost { shared, listener })
    }

    /// The tokens of this session (public, and the invite's with the ticket).
    pub fn tokens(&self, host: u64) -> (ConnectToken, ConnectToken) {
        let state = lock(&self.shared.state);
        (state.gate.public_token(host), state.gate.invite_token(host))
    }

    /// The host's player let `account` in: it receives the key now.
    pub fn admit(&self, account: u64) -> Result<String, String> {
        let mut state = lock(&self.shared.state);
        let name = state.gate.pending().iter().find(|p| p.account == account).map(|p| p.name.clone());
        let Some(name) = name else { return Err("that request is no longer waiting".into()) };
        let conn = HostShared::waiting_conn(&state, account);
        state.gate.admit(account);
        let Some(conn) = conn else { return Err(format!("{name} is no longer waiting")) };
        state.log.push(format!("invites: the host admitted {name} ({})", short_id(account)));
        let key = state.key;
        self.shared.finish(&mut state, conn, AdmitMessage::Admitted { key });
        Ok(format!("{name} is joining"))
    }

    /// The host's player said no.
    pub fn refuse(&self, account: u64) -> Result<String, String> {
        let mut state = lock(&self.shared.state);
        let name = state.gate.pending().iter().find(|p| p.account == account).map(|p| p.name.clone());
        let Some(name) = name else { return Err("that request is no longer waiting".into()) };
        state.gate.refuse(account, Instant::now());
        state.log.push(format!("invites: the host refused {name} ({})", short_id(account)));
        if let Some(conn) = HostShared::waiting_conn(&state, account) {
            self.shared.finish(&mut state, conn, AdmitMessage::Refused { reason: RefuseReason::Refused });
        }
        Ok(format!("{name} was not let in"))
    }

    /// Times out what waited too long: requests nobody answered, connections that never asked. Called by the
    /// controller every step.
    pub fn tick(&self) {
        let now = Instant::now();
        let mut state = lock(&self.shared.state);
        for account in state.gate.expire(now) {
            state.log.push(format!("invites: the request of {} timed out", short_id(account)));
            if let Some(conn) = HostShared::waiting_conn(&state, account) {
                self.shared.finish(&mut state, conn, AdmitMessage::Refused { reason: RefuseReason::Timeout });
            }
        }
        let silent: Vec<Handle> = state
            .conns
            .iter()
            .filter(|(_, c)| !c.asked && !c.done && now.saturating_duration_since(c.opened) >= REQUEST_WAIT)
            .map(|(h, _)| *h)
            .collect();
        for conn in silent {
            if let Some(c) = state.conns.get_mut(&conn) {
                c.done = true;
            }
            self.shared.core.close(conn);
        }
    }

    /// The requests waiting for the host's player, oldest first.
    pub fn pending(&self) -> Vec<Pending> {
        lock(&self.shared.state).gate.pending().to_vec()
    }

    /// What happened since the last call, for the session log.
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.shared.state).log)
    }

    /// Refuses whoever waits (`ended`) and stops listening. Idempotent; also done on drop.
    pub fn stop(&self) {
        let mut state = lock(&self.shared.state);
        if std::mem::replace(&mut state.stopped, true) {
            return;
        }
        let waiting: Vec<Handle> = state.conns.iter().filter(|(_, c)| !c.done).map(|(h, _)| *h).collect();
        for conn in waiting {
            self.shared.finish(&mut state, conn, AdmitMessage::Refused { reason: RefuseReason::Ended });
        }
        drop(state);
        self.shared.core.close_listener(self.listener);
    }
}

impl Drop for AdmitHost {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Friend
// ---------------------------------------------------------------------------------------------------------------

enum Phase {
    Connecting,
    Open(Handle),
    Answer(AdmitMessage),
    Closed(String),
}

struct ClientShared {
    state: Mutex<(Phase, bool)>,
    changed: Condvar,
}

impl Owner for ClientShared {
    fn opened(&self, conn: Handle, _incoming: bool, _remote: Option<u64>) {
        let mut s = lock(&self.state);
        if matches!(s.0, Phase::Connecting) {
            s.0 = Phase::Open(conn);
        }
        self.changed.notify_all();
    }

    fn message(&self, _conn: Handle, data: &[u8]) {
        let mut s = lock(&self.state);
        match AdmitMessage::decode(data) {
            Some(AdmitMessage::Waiting) => s.1 = true,
            Some(answer @ (AdmitMessage::Admitted { .. } | AdmitMessage::Refused { .. })) => {
                if matches!(s.0, Phase::Open(_)) {
                    s.0 = Phase::Answer(answer);
                }
            }
            _ => {
                if matches!(s.0, Phase::Open(_)) {
                    s.0 = Phase::Closed("the host sent something that is not an answer".into());
                }
            }
        }
        self.changed.notify_all();
    }

    fn closed(&self, _conn: Handle, end: End) {
        let mut s = lock(&self.state);
        if matches!(s.0, Phase::Connecting | Phase::Open(_)) {
            s.0 = Phase::Closed(if end.opened {
                "the host's game closed the connection before answering".to_owned()
            } else {
                format!("the host's game cannot be reached or is not hosting right now ({})", end.describe())
            });
        }
        self.changed.notify_all();
    }
}

/// Asks the host of `token` to be let in, as the friend's game does after a "Join game" or an accepted invite. Returns
/// the session key once the host admitted this account, or the reason in words. `progress` hears what is happening;
/// `cancelled` stops the wait.
pub fn request_admission(
    core: &CoreRef,
    token: &ConnectToken,
    name: &str,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&str),
) -> Result<SessionKey, String> {
    progress("waiting for Steam");
    core.wait_ready(Instant::now() + CONNECT_TIMEOUT, cancelled).map_err(|e| e.to_string())?;
    let shared = Arc::new(ClientShared { state: Mutex::new((Phase::Connecting, false)), changed: Condvar::new() });
    let owner: Arc<dyn Owner> = shared.clone();
    progress("contacting the host");
    let handle = core.connect(token.host, ADMIT_PORT, owner).map_err(|e| e.to_string())?;
    let mut deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut sent = false;
    let mut waiting_said = false;
    let mut s = lock(&shared.state);
    let outcome = loop {
        match &s.0 {
            Phase::Open(conn) if !sent => {
                let conn = *conn;
                let request = AdmitMessage::Request {
                    session: token.session,
                    ticket: token.ticket,
                    name: name.to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                };
                drop(s);
                if core.send(conn, &request.encode()) != crate::steamnet::SendOutcome::Sent {
                    s = lock(&shared.state);
                    break Err("could not send the request to the host".to_owned());
                }
                sent = true;
                progress("asking the host to let you in");
                // the host's player may take up to the request timeout to answer
                deadline = Instant::now() + REQUEST_TIMEOUT + Duration::from_secs(15);
                s = lock(&shared.state);
                continue;
            }
            Phase::Answer(AdmitMessage::Admitted { key }) => break Ok(SessionKey::from_key_bytes(*key)),
            Phase::Answer(AdmitMessage::Refused { reason }) => break Err(reason.sentence().to_owned()),
            Phase::Answer(_) => break Err("the host sent something that is not an answer".to_owned()),
            Phase::Closed(why) => break Err(why.clone()),
            _ => {}
        }
        if s.1 && !waiting_said {
            waiting_said = true;
            drop(s);
            progress(STATUS_WAITING);
            s = lock(&shared.state);
            continue;
        }
        if cancelled() {
            break Err("cancelled".to_owned());
        }
        let now = Instant::now();
        if now >= deadline {
            break Err(if sent {
                "the host did not answer your request in time".to_owned()
            } else {
                "no route to the host's game over Steam".to_owned()
            });
        }
        s = shared
            .changed
            .wait_timeout(s, (deadline - now).min(Duration::from_millis(50)))
            .unwrap_or_else(|p| p.into_inner())
            .0;
    };
    drop(s);
    core.close(handle);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steamnet::fake::FakeMachine;
    use crate::steamnet::SteamCore;
    use std::thread;

    const WAIT: Duration = Duration::from_secs(10);

    struct Host {
        machine: FakeMachine,
        _core: crate::steamnet::SteamCore,
        admit: AdmitHost,
        key: SessionKey,
    }

    fn host() -> Host {
        let machine = FakeMachine::new();
        let core = machine.core();
        let key = SessionKey::derive(&SessionKey::new_secret(), Some("pw"));
        let admit = AdmitHost::start(&core.handle(), Gate::new(11, 22), &key).expect("listen");
        Host { machine, _core: core, admit, key }
    }

    /// Runs the friend's request on a thread; the result arrives on the channel. `core` is the friend's one core: a
    /// game has one Steam client and one process-wide core (`steam_core::GLOBAL`). A second core on the same fake
    /// machine would race the first for the machine's status callbacks (whichever poller runs `RunCallbacks` takes
    /// them, as two pollers on one real `ISteamNetworkingSockets` would), and the new connection's events could be
    /// taken by the core that does not know it: the friend then never hears that it connected.
    fn ask(core: &SteamCore, token: ConnectToken) -> std::sync::mpsc::Receiver<Result<SessionKey, String>> {
        let handle = core.handle();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(request_admission(&handle, &token, "Ben", &|| false, &mut |_| {}));
        });
        rx
    }

    fn wait_pending(host: &Host, n: usize) {
        let deadline = Instant::now() + WAIT;
        while host.admit.pending().len() != n {
            assert!(Instant::now() < deadline, "pending {:?}", host.admit.pending());
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_invited_friend_gets_the_key_at_once() {
        let h = host();
        let (_, invite) = h.admit.tokens(h.machine.steam_id());
        let friend = FakeMachine::new();
        let core = friend.core();
        let rx = ask(&core, invite);
        let key = rx.recv_timeout(WAIT).expect("an answer").expect("admitted");
        assert_eq!(key.key(), h.key.key());
        let log = h.admit.take_log().join("\n");
        assert!(log.contains("came through your invite: admitted"), "{log}");
        assert!(h.admit.pending().is_empty());
    }

    #[test]
    fn a_join_through_rich_presence_waits_for_the_host_who_admits() {
        let h = host();
        let (public, _) = h.admit.tokens(h.machine.steam_id());
        let friend = FakeMachine::new();
        let core = friend.core();
        let rx = ask(&core, public);
        wait_pending(&h, 1);
        // the account the host sees is the one Steam authenticated, not a name the friend wrote
        assert_eq!(h.admit.pending()[0].account, friend.steam_id());
        assert_eq!(h.admit.pending()[0].name, "Ben");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no answer before the host decides");
        assert_eq!(h.admit.admit(friend.steam_id()), Ok("Ben is joining".into()));
        let key = rx.recv_timeout(WAIT).expect("an answer").expect("admitted");
        assert_eq!(key.key(), h.key.key());
    }

    #[test]
    fn a_refused_friend_is_told_so_and_cannot_ask_again_at_once() {
        let h = host();
        let (public, _) = h.admit.tokens(h.machine.steam_id());
        let friend = FakeMachine::new();
        let core = friend.core();
        let rx = ask(&core, public);
        wait_pending(&h, 1);
        assert!(h.admit.refuse(friend.steam_id()).is_ok());
        let why = rx.recv_timeout(WAIT).expect("an answer").expect_err("refused");
        assert_eq!(why, RefuseReason::Refused.sentence());
        // the friend presses Join again at once, from the same game
        let rx = ask(&core, public);
        let why = rx.recv_timeout(WAIT).expect("an answer").expect_err("refused again");
        assert_eq!(why, RefuseReason::Refused.sentence());
        assert!(h.admit.pending().is_empty());
    }

    #[test]
    fn a_token_of_an_older_session_is_stale() {
        let h = host();
        let friend = FakeMachine::new();
        let old = ConnectToken { host: h.machine.steam_id(), session: 10, ticket: Some(22) };
        let core = friend.core();
        let rx = ask(&core, old);
        let why = rx.recv_timeout(WAIT).expect("an answer").expect_err("stale");
        assert_eq!(why, RefuseReason::Stale.sentence());
    }

    #[test]
    fn a_host_that_stops_tells_the_waiting_friend() {
        let h = host();
        let (public, _) = h.admit.tokens(h.machine.steam_id());
        let friend = FakeMachine::new();
        let core = friend.core();
        let rx = ask(&core, public);
        wait_pending(&h, 1);
        drop(h.admit);
        let why = rx.recv_timeout(WAIT).expect("an answer").expect_err("ended");
        assert_eq!(why, RefuseReason::Ended.sentence());
    }

    #[test]
    fn nobody_listening_is_a_clear_failure() {
        let nobody = FakeMachine::new();
        let friend = FakeMachine::new();
        let token = ConnectToken { host: nobody.steam_id(), session: 1, ticket: None };
        let core = friend.core();
        let rx = ask(&core, token);
        let why = rx.recv_timeout(Duration::from_secs(40)).expect("an answer").expect_err("failed");
        assert!(why.contains("cannot be reached or is not hosting"), "{why}");
    }

    #[test]
    fn a_friend_who_goes_away_is_no_longer_listed() {
        let h = host();
        let (public, _) = h.admit.tokens(h.machine.steam_id());
        let friend = FakeMachine::new();
        let core = friend.core();
        let handle = core.handle();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let t = thread::spawn(move || {
            request_admission(&handle, &public, "Ben", &|| flag.load(std::sync::atomic::Ordering::Acquire), &mut |_| {})
        });
        wait_pending(&h, 1);
        stop.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(t.join().unwrap().err().as_deref(), Some("cancelled"));
        wait_pending(&h, 0);
        assert!(h.admit.admit(friend.steam_id()).is_err());
    }

    #[test]
    fn garbage_on_the_admission_port_is_refused_without_a_key() {
        let h = host();
        let friend = FakeMachine::new();
        let core = friend.core();
        #[derive(Default)]
        struct Rec(Mutex<Vec<Vec<u8>>>, Mutex<bool>);
        impl Owner for Rec {
            fn message(&self, _c: Handle, d: &[u8]) {
                lock(&self.0).push(d.to_vec());
            }
            fn closed(&self, _c: Handle, _e: End) {
                *lock(&self.1) = true;
            }
        }
        let rec = Arc::new(Rec::default());
        let conn = core.handle().connect(h.machine.steam_id(), ADMIT_PORT, rec.clone()).unwrap();
        let deadline = Instant::now() + WAIT;
        while !core.handle().is_open(conn) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let _ = core.handle().send(conn, b"hello");
        while !*lock(&rec.1) {
            assert!(Instant::now() < deadline, "the host closes");
            thread::sleep(Duration::from_millis(5));
        }
        let got = lock(&rec.0).clone();
        assert_eq!(got.len(), 1);
        assert_eq!(AdmitMessage::decode(&got[0]), Some(AdmitMessage::Refused { reason: RefuseReason::Malformed }));
    }
}
