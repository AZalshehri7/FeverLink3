//! Steam invites: the public connect token and the admission exchange. See `docs/PROTOCOL.md`, "Steam invites".
//!
//! A host that hosts over Steam publishes a **connect token** in its Steam rich presence (`connect`) and puts one in the
//! invites it sends through Steam's overlay. The token is public: it names the host's Steam ID and a random session
//! number, never the session secret or key. A friend who clicks "Join game" or accepts an invite connects to the host
//! over Steam (Steam authenticates the friend's account) and sends an [`AdmitMessage::Request`]; the host's [`Gate`]
//! decides: the friend is admitted at once (it came through the host's own invite, or the host admitted that account
//! earlier in this session), or the host's player is asked, and only an admitted account receives the session key, on
//! that authenticated connection. Everything here is pure; the launcher does the Steam calls.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::crypto::SessionKey;

/// The command-line switch Steam passes on to the game (`connect` rich presence value, invite connect string).
pub const CONNECT_PREFIX: &str = "+feverlink3_join";
/// The token format this build writes and reads: the first field of the payload, `1:`.
pub const TOKEN_VERSION: u32 = 1;
/// Steam's limit for a rich presence value (`k_cchMaxRichPresenceValueLength`, 256 with the NUL).
pub const MAX_CONNECT_STRING: usize = 255;
/// The admission exchange's magic, the first bytes of every message.
pub const MAGIC: &[u8; 4] = b"FL3A";
/// The largest admission message either side reads.
pub const MAX_MESSAGE: usize = 2048;
/// Join requests a host keeps waiting for its player's answer at once; more are refused as `busy`.
pub const MAX_PENDING: usize = 8;
/// How long a request waits for the host's answer before it is refused as `timeout`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// After a refusal, further requests of the same account are refused without asking for this long.
pub const REFUSE_COOLDOWN: Duration = Duration::from_secs(60);
/// The longest name a request carries, in characters.
pub const MAX_NAME_CHARS: usize = 24;

/// Whether `id` is an individual account of the public universe (`steamID64`), the only kind a host can be.
pub fn is_individual_account(id: u64) -> bool {
    id >> 56 == 1 && (id >> 52) & 0xF == 1 && id & 0xFFFF_FFFF != 0
}

/// Where a friend's game finds the host: the host's Steam ID and the session number, plus the invite ticket in an
/// invite the host sent through Steam's overlay. None of it is secret material: the ticket only spares the friend the
/// host's Admit prompt.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectToken {
    pub host: u64,
    /// Random per hosted session: a token of an earlier session is recognised as stale.
    pub session: u64,
    /// Random per hosted session, only in the overlay invite: a request carrying it is admitted without the prompt.
    pub ticket: Option<u64>,
}

/// Why a text holds no usable token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// No `+feverlink3_join` in it.
    Missing,
    /// A token of a newer (or unknown) format: the text after the prefix up to the first `:`.
    UnknownVersion(String),
    /// The prefix is there but what follows is not a token.
    Malformed,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing => f.write_str("no FeverLink3 invite in it"),
            TokenError::UnknownVersion(v) => {
                write!(
                    f,
                    "the invite is of format {v:?}, this FeverLink3 reads format {TOKEN_VERSION}: update FeverLink3"
                )
            }
            TokenError::Malformed => f.write_str("the invite is damaged"),
        }
    }
}

impl std::error::Error for TokenError {}

impl ConnectToken {
    /// The token without its ticket: what rich presence carries.
    pub fn public(self) -> ConnectToken {
        ConnectToken { ticket: None, ..self }
    }

    /// `1:<host steamID64>:<session, 16 hex digits>[:<ticket, 16 hex digits>]`.
    pub fn payload(&self) -> String {
        let mut text = format!("{TOKEN_VERSION}:{}:{:016x}", self.host, self.session);
        if let Some(ticket) = self.ticket {
            text.push_str(&format!(":{ticket:016x}"));
        }
        text
    }

    /// `+feverlink3_join <payload>`: the rich presence `connect` value and the invite's connect string.
    pub fn connect_string(&self) -> String {
        format!("{CONNECT_PREFIX} {}", self.payload())
    }

    /// Reads a payload (the text after the prefix).
    pub fn parse_payload(text: &str) -> Result<ConnectToken, TokenError> {
        let mut parts = text.split(':');
        let version = parts.next().unwrap_or("");
        if version != TOKEN_VERSION.to_string() {
            return Err(if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric()) {
                TokenError::Malformed
            } else {
                TokenError::UnknownVersion(version.chars().take(16).collect())
            });
        }
        let host = parts.next().filter(|h| h.len() == 17 && h.bytes().all(|b| b.is_ascii_digit()));
        let host = host.and_then(|h| h.parse::<u64>().ok()).filter(|h| is_individual_account(*h));
        let hex = |p: Option<&str>| p.filter(|p| p.len() == 16).and_then(|p| u64::from_str_radix(p, 16).ok());
        let (Some(host), Some(session)) = (host, hex(parts.next())) else { return Err(TokenError::Malformed) };
        let ticket = match parts.next() {
            None => None,
            some => Some(hex(some).ok_or(TokenError::Malformed)?),
        };
        if parts.next().is_some() {
            return Err(TokenError::Malformed);
        }
        Ok(ConnectToken { host, session, ticket })
    }

    /// Finds the token in a command line or a connect string: `+feverlink3_join` as a word of its own, then spaces, an
    /// optional `=` and optional quotes, then the payload up to the next space or quote. Steam appends the connect
    /// string to the game's command line on a cold start, and hands it unchanged to a running game (callback 337), so
    /// both forms are read by this one function.
    pub fn find(text: &str) -> Result<ConnectToken, TokenError> {
        let mut from = 0;
        while let Some(at) = text[from..].find(CONNECT_PREFIX).map(|i| i + from) {
            from = at + CONNECT_PREFIX.len();
            let before_ok = text[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || c == '"');
            let rest = &text[from..];
            let after_ok = rest.chars().next().is_none_or(|c| c.is_whitespace() || c == '"' || c == '=');
            if !before_ok || !after_ok {
                continue;
            }
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '=');
            let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
            let payload = &rest[..end];
            if payload.is_empty() || payload.len() > 128 {
                return Err(TokenError::Malformed);
            }
            return ConnectToken::parse_payload(payload);
        }
        Err(TokenError::Missing)
    }
}

impl fmt::Debug for ConnectToken {
    // the ticket is not secret, but it lets its holder skip the prompt: logs say whether there is one, not what it is
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectToken")
            .field("host", &self.host)
            .field("session", &format_args!("{:016x}", self.session))
            .field("ticket", &self.ticket.is_some())
            .finish()
    }
}

/// Why a host refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The host's player said no (or said no a moment ago, [`REFUSE_COOLDOWN`]).
    Refused,
    /// Nobody answered within [`REQUEST_TIMEOUT`].
    Timeout,
    /// The token names another session: the host has hosted again since, or stopped.
    Stale,
    /// Too many requests are waiting ([`MAX_PENDING`]).
    Busy,
    /// The request could not be read.
    Malformed,
    /// The host stopped hosting while the request waited.
    Ended,
}

impl RefuseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RefuseReason::Refused => "refused",
            RefuseReason::Timeout => "timeout",
            RefuseReason::Stale => "stale",
            RefuseReason::Busy => "busy",
            RefuseReason::Malformed => "malformed",
            RefuseReason::Ended => "ended",
        }
    }

    fn parse(text: &str) -> Option<RefuseReason> {
        [
            RefuseReason::Refused,
            RefuseReason::Timeout,
            RefuseReason::Stale,
            RefuseReason::Busy,
            RefuseReason::Malformed,
            RefuseReason::Ended,
        ]
        .into_iter()
        .find(|r| r.as_str() == text)
    }

    /// What the friend's window says.
    pub fn sentence(self) -> &'static str {
        match self {
            RefuseReason::Refused => "the host did not let you in",
            RefuseReason::Timeout => "the host did not answer your request in time",
            RefuseReason::Stale => "that invite is for a session that is over; ask the host for a new one",
            RefuseReason::Busy => "the host has too many join requests waiting; try again in a moment",
            RefuseReason::Malformed => "the host could not read the request (different FeverLink3 versions?)",
            RefuseReason::Ended => "the host stopped hosting",
        }
    }
}

/// One message of the admission exchange: one Steam message each, [`MAGIC`] then UTF-8 JSON.
#[derive(Clone, PartialEq, Eq)]
pub enum AdmitMessage {
    /// Friend to host, first and only.
    Request { session: u64, ticket: Option<u64>, name: String, version: String },
    /// Host to friend: the host's player is being asked; the friend keeps waiting.
    Waiting,
    /// Host to friend, then the host closes: the session key. Only ever sent to an admitted account.
    Admitted { key: [u8; SessionKey::KEY_SIZE] },
    /// Host to friend, then the host closes.
    Refused { reason: RefuseReason },
}

impl fmt::Debug for AdmitMessage {
    // the key never reaches a log through a Debug print
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmitMessage::Request { session, ticket, name, version } => f
                .debug_struct("Request")
                .field("session", &format_args!("{session:016x}"))
                .field("ticket", &ticket.is_some())
                .field("name", name)
                .field("version", version)
                .finish(),
            AdmitMessage::Waiting => f.write_str("Waiting"),
            AdmitMessage::Admitted { .. } => f.write_str("Admitted { key: .. }"),
            AdmitMessage::Refused { reason } => f.debug_struct("Refused").field("reason", reason).finish(),
        }
    }
}

fn hex16(v: u64) -> String {
    format!("{v:016x}")
}

fn read_hex16(v: Option<&Value>) -> Option<u64> {
    v?.as_str().filter(|s| s.len() == 16).and_then(|s| u64::from_str_radix(s, 16).ok())
}

impl AdmitMessage {
    pub fn encode(&self) -> Vec<u8> {
        let body = match self {
            AdmitMessage::Request { session, ticket, name, version } => {
                let mut v = json!({ "kind": "request", "session": hex16(*session), "name": name, "version": version });
                if let Some(t) = ticket {
                    v["ticket"] = json!(hex16(*t));
                }
                v
            }
            AdmitMessage::Waiting => json!({ "kind": "waiting" }),
            AdmitMessage::Admitted { key } => {
                let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
                json!({ "kind": "admitted", "key": hex })
            }
            AdmitMessage::Refused { reason } => json!({ "kind": "refused", "reason": reason.as_str() }),
        };
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(body.to_string().as_bytes());
        out
    }

    /// `None` for anything that is not exactly one well-formed message.
    pub fn decode(bytes: &[u8]) -> Option<AdmitMessage> {
        if bytes.len() > MAX_MESSAGE || !bytes.starts_with(MAGIC) {
            return None;
        }
        let v: Map<String, Value> = serde_json::from_slice(&bytes[MAGIC.len()..]).ok()?;
        match v.get("kind")?.as_str()? {
            "request" => {
                let session = read_hex16(v.get("session"))?;
                let ticket = match v.get("ticket") {
                    None | Some(Value::Null) => None,
                    some => Some(read_hex16(some)?),
                };
                let name = v.get("name").and_then(Value::as_str).unwrap_or("");
                let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
                let version: String = v.get("version").and_then(Value::as_str).unwrap_or("").chars().take(32).collect();
                Some(AdmitMessage::Request { session, ticket, name: name.trim().to_owned(), version })
            }
            "waiting" => Some(AdmitMessage::Waiting),
            "admitted" => {
                let text = v.get("key")?.as_str()?;
                if text.len() != SessionKey::KEY_SIZE * 2 {
                    return None;
                }
                let mut key = [0u8; SessionKey::KEY_SIZE];
                for (i, b) in key.iter_mut().enumerate() {
                    *b = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
                }
                Some(AdmitMessage::Admitted { key })
            }
            "refused" => Some(AdmitMessage::Refused { reason: RefuseReason::parse(v.get("reason")?.as_str()?)? }),
            _ => None,
        }
    }
}

/// What the host does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Send [`AdmitMessage::Admitted`] now.
    Admit,
    /// Send [`AdmitMessage::Waiting`] and ask the host's player.
    Ask,
    Refuse(RefuseReason),
}

/// A request waiting for the host's player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub account: u64,
    /// The name the friend's game sent.
    pub name: String,
    pub since: Instant,
}

/// The host's side of the admission: one per hosted session.
#[derive(Debug)]
pub struct Gate {
    session: u64,
    ticket: u64,
    /// Accounts admitted in this session: a reconnect or a second request is admitted without asking again.
    admitted: Vec<u64>,
    pending: Vec<Pending>,
    refused_until: HashMap<u64, Instant>,
}

impl Gate {
    /// A gate for a new session; `session` and `ticket` come from the operating system's random source.
    pub fn new(session: u64, ticket: u64) -> Gate {
        Gate { session, ticket, admitted: Vec::new(), pending: Vec::new(), refused_until: HashMap::new() }
    }

    /// Two random numbers for [`Gate::new`]; `None` when the random source fails.
    pub fn random_numbers() -> Option<(u64, u64)> {
        let mut b = [0u8; 16];
        getrandom::getrandom(&mut b).ok()?;
        let (s, t) = b.split_at(8);
        Some((u64::from_le_bytes(s.try_into().ok()?), u64::from_le_bytes(t.try_into().ok()?)))
    }

    /// The token rich presence carries (no ticket).
    pub fn public_token(&self, host: u64) -> ConnectToken {
        ConnectToken { host, session: self.session, ticket: None }
    }

    /// The token of an invite sent through the overlay (with the ticket).
    pub fn invite_token(&self, host: u64) -> ConnectToken {
        ConnectToken { host, session: self.session, ticket: Some(self.ticket) }
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    /// A request from `account` (the Steam ID Steam authenticated for the connection, never what the friend says).
    pub fn on_request(&mut self, account: u64, msg: &AdmitMessage, now: Instant) -> Decision {
        let AdmitMessage::Request { session, ticket, name, .. } = msg else {
            return Decision::Refuse(RefuseReason::Malformed);
        };
        if *session != self.session {
            return Decision::Refuse(RefuseReason::Stale);
        }
        if *ticket == Some(self.ticket) || self.admitted.contains(&account) {
            self.pending.retain(|p| p.account != account);
            if !self.admitted.contains(&account) {
                self.admitted.push(account);
            }
            return Decision::Admit;
        }
        if self.refused_until.get(&account).is_some_and(|until| now < *until) {
            return Decision::Refuse(RefuseReason::Refused);
        }
        if let Some(p) = self.pending.iter_mut().find(|p| p.account == account) {
            p.name = name.clone();
            return Decision::Ask;
        }
        if self.pending.len() >= MAX_PENDING {
            return Decision::Refuse(RefuseReason::Busy);
        }
        self.pending.push(Pending { account, name: name.clone(), since: now });
        Decision::Ask
    }

    /// The host's player admitted `account`: true when it was waiting (its connection gets the key now).
    pub fn admit(&mut self, account: u64) -> bool {
        let was = self.pending.iter().any(|p| p.account == account);
        if was {
            self.pending.retain(|p| p.account != account);
            self.admitted.push(account);
        }
        was
    }

    /// The host's player refused `account`: true when it was waiting.
    pub fn refuse(&mut self, account: u64, now: Instant) -> bool {
        let was = self.pending.iter().any(|p| p.account == account);
        self.pending.retain(|p| p.account != account);
        if was {
            self.refused_until.insert(account, now + REFUSE_COOLDOWN);
        }
        was
    }

    /// The friend went away before an answer.
    pub fn withdraw(&mut self, account: u64) {
        self.pending.retain(|p| p.account != account);
    }

    /// Requests that waited [`REQUEST_TIMEOUT`]: removed, to be refused as `timeout`.
    pub fn expire(&mut self, now: Instant) -> Vec<u64> {
        let (old, keep): (Vec<Pending>, Vec<Pending>) =
            self.pending.drain(..).partition(|p| now.saturating_duration_since(p.since) >= REQUEST_TIMEOUT);
        self.pending = keep;
        old.into_iter().map(|p| p.account).collect()
    }

    /// The requests waiting, oldest first.
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: u64 = 76561198000000042;
    const FRIEND: u64 = 76561198000000077;

    fn token() -> ConnectToken {
        ConnectToken { host: HOST, session: 0x0123_4567_89ab_cdef, ticket: None }
    }

    #[test]
    fn the_connect_string_is_versioned_and_fits_rich_presence() {
        let t = token();
        assert_eq!(t.connect_string(), "+feverlink3_join 1:76561198000000042:0123456789abcdef");
        let invite = ConnectToken { ticket: Some(u64::MAX), ..t };
        assert_eq!(invite.payload(), "1:76561198000000042:0123456789abcdef:ffffffffffffffff");
        assert!(invite.connect_string().len() <= MAX_CONNECT_STRING);
        assert_eq!(invite.public(), t);
    }

    #[test]
    fn a_token_is_found_in_a_command_line_and_in_a_connect_string() {
        let t = ConnectToken { ticket: Some(7), ..token() };
        let cs = t.connect_string();
        assert_eq!(ConnectToken::find(&cs), Ok(t));
        let cmd = format!("\"C:\\Games\\TF3\\TransportFever3.exe\" -something {cs} +other 1");
        assert_eq!(ConnectToken::find(&cmd), Ok(t));
        let quoted = format!("TransportFever3.exe \"+feverlink3_join\" \"{}\"", t.payload());
        assert_eq!(ConnectToken::find(&quoted), Ok(t));
        assert_eq!(ConnectToken::find(&format!("x +feverlink3_join={}", t.payload())), Ok(t));
    }

    #[test]
    fn texts_without_a_good_token_say_why() {
        assert_eq!(ConnectToken::find("TransportFever3.exe -nosound"), Err(TokenError::Missing));
        // another mod's switch that merely starts with ours is not ours
        assert_eq!(ConnectToken::find("+feverlink3_joinx 1:2:3"), Err(TokenError::Missing));
        assert_eq!(ConnectToken::find("+feverlink3_join 2:abc"), Err(TokenError::UnknownVersion("2".into())));
        for bad in [
            "+feverlink3_join",
            "+feverlink3_join 1:76561198000000042",
            "+feverlink3_join 1:12345:0123456789abcdef",
            "+feverlink3_join 1:76561198000000042:0123",
            "+feverlink3_join 1:76561198000000042:0123456789abcdef:zz",
            "+feverlink3_join 1:76561198000000042:0123456789abcdef:0123456789abcdef:0",
            // a group or game-server ID cannot host
            "+feverlink3_join 1:85568392920039424:0123456789abcdef",
        ] {
            assert_eq!(ConnectToken::find(bad), Err(TokenError::Malformed), "{bad}");
        }
    }

    #[test]
    fn messages_round_trip_and_garbage_is_nothing() {
        let msgs = [
            AdmitMessage::Request { session: 9, ticket: Some(3), name: "Ana".into(), version: "0.2.0".into() },
            AdmitMessage::Request { session: 9, ticket: None, name: "".into(), version: "".into() },
            AdmitMessage::Waiting,
            AdmitMessage::Admitted { key: [0xA5; 32] },
            AdmitMessage::Refused { reason: RefuseReason::Timeout },
        ];
        for m in msgs {
            assert_eq!(AdmitMessage::decode(&m.encode()), Some(m.clone()), "{m:?}");
        }
        assert_eq!(AdmitMessage::decode(b"FL3A{\"kind\":\"admitted\",\"key\":\"00\"}"), None);
        assert_eq!(AdmitMessage::decode(b"XXXX{\"kind\":\"waiting\"}"), None);
        assert_eq!(AdmitMessage::decode(b"FL3A{\"kind\":\"other\"}"), None);
        assert_eq!(AdmitMessage::decode(b"FL3A{\"kind\":\"refused\",\"reason\":\"why\"}"), None);
        let mut big = MAGIC.to_vec();
        big.resize(MAX_MESSAGE + 1, b' ');
        assert_eq!(AdmitMessage::decode(&big), None);
    }

    #[test]
    fn a_request_name_is_cleaned_and_limited() {
        let raw = b"FL3A{\"kind\":\"request\",\"session\":\"0000000000000001\",\"name\":\"  A\\u0007na_____________________________ \"}";
        let Some(AdmitMessage::Request { name, .. }) = AdmitMessage::decode(raw) else { panic!("a request") };
        assert_eq!(name.chars().count(), MAX_NAME_CHARS - 2);
        assert!(name.starts_with("Ana"), "{name}");
    }

    #[test]
    fn the_key_is_never_in_a_debug_print() {
        let m = AdmitMessage::Admitted { key: [0xAB; 32] };
        assert!(!format!("{m:?}").contains("ab"), "{m:?}");
        let t = ConnectToken { ticket: Some(0x1122_3344_5566_7788), ..token() };
        assert!(!format!("{t:?}").contains("1122334455667788"), "{t:?}");
    }

    fn request(session: u64, ticket: Option<u64>) -> AdmitMessage {
        AdmitMessage::Request { session, ticket, name: "Ben".into(), version: "x".into() }
    }

    #[test]
    fn the_invite_ticket_admits_at_once_and_the_public_token_asks() {
        let now = Instant::now();
        let mut gate = Gate::new(5, 6);
        assert_eq!(gate.on_request(FRIEND, &request(5, Some(6)), now), Decision::Admit);
        assert!(gate.pending().is_empty());
        let other = FRIEND + 1;
        assert_eq!(gate.on_request(other, &request(5, None), now), Decision::Ask);
        assert_eq!(gate.pending().len(), 1);
        // a wrong ticket is no invite: the host is asked
        assert_eq!(gate.on_request(other + 1, &request(5, Some(7)), now), Decision::Ask);
        // an admitted account comes back (a reconnect, a second click): no second prompt
        assert_eq!(gate.on_request(FRIEND, &request(5, None), now), Decision::Admit);
        assert_eq!(gate.public_token(HOST).ticket, None);
        assert_eq!(gate.invite_token(HOST).ticket, Some(6));
    }

    #[test]
    fn a_token_of_another_session_is_stale() {
        let mut gate = Gate::new(5, 6);
        assert_eq!(
            gate.on_request(FRIEND, &request(4, Some(6)), Instant::now()),
            Decision::Refuse(RefuseReason::Stale)
        );
        assert_eq!(
            gate.on_request(FRIEND, &AdmitMessage::Waiting, Instant::now()),
            Decision::Refuse(RefuseReason::Malformed)
        );
    }

    #[test]
    fn the_host_admits_or_refuses_and_a_refused_account_waits_before_asking_again() {
        let now = Instant::now();
        let mut gate = Gate::new(1, 2);
        assert_eq!(gate.on_request(FRIEND, &request(1, None), now), Decision::Ask);
        // asking twice keeps one entry
        assert_eq!(gate.on_request(FRIEND, &request(1, None), now), Decision::Ask);
        assert_eq!(gate.pending().len(), 1);
        assert!(gate.refuse(FRIEND, now));
        assert!(!gate.refuse(FRIEND, now), "only once");
        assert_eq!(gate.on_request(FRIEND, &request(1, None), now), Decision::Refuse(RefuseReason::Refused));
        assert_eq!(gate.on_request(FRIEND, &request(1, None), now + REFUSE_COOLDOWN), Decision::Ask);
        assert!(gate.admit(FRIEND));
        assert!(!gate.admit(FRIEND), "nothing waits any more");
        assert_eq!(gate.on_request(FRIEND, &request(1, None), now), Decision::Admit);
    }

    #[test]
    fn requests_time_out_and_the_queue_is_bounded() {
        let now = Instant::now();
        let mut gate = Gate::new(1, 2);
        for i in 0..MAX_PENDING as u64 {
            assert_eq!(gate.on_request(FRIEND + i, &request(1, None), now), Decision::Ask);
        }
        assert_eq!(gate.on_request(FRIEND + 100, &request(1, None), now), Decision::Refuse(RefuseReason::Busy));
        gate.withdraw(FRIEND);
        assert_eq!(gate.pending().len(), MAX_PENDING - 1);
        assert!(gate.expire(now + REQUEST_TIMEOUT - Duration::from_secs(1)).is_empty());
        assert_eq!(gate.expire(now + REQUEST_TIMEOUT).len(), MAX_PENDING - 1);
        assert!(gate.pending().is_empty());
    }

    #[test]
    fn random_numbers_differ() {
        let (a, b) = Gate::random_numbers().expect("random");
        let (c, d) = Gate::random_numbers().expect("random");
        assert!(a != c || b != d);
    }
}
