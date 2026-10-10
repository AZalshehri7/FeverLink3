//! Who is in a session and how they are doing: what the host knows, and what each player is told about the others.

/// Whether a player has a live connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    Connected,
    /// The connection dropped. The host holds the player's id and slot so it can come back as itself.
    Away,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Away => "away",
        }
    }
}

/// One player as the roster shows it. The host's own game is id 0.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub id: u16,
    pub name: String,
    /// The game said hello after its last load.
    pub ready: bool,
    pub state: ConnectionState,
    /// Round-trip time between the host and this player, in milliseconds, smoothed over recent pings. `None` until
    /// the first answer and while the player is away; 0 for the host's own game.
    pub rtt_ms: Option<f64>,
}

impl RosterEntry {
    /// `host` for id 0, else `player`.
    pub fn role(&self) -> &'static str {
        if self.id == 0 {
            "host"
        } else {
            "player"
        }
    }
}
