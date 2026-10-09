/// Frame kinds. The byte value travels in the frame header. See `docs/PROTOCOL.md`, "Who may send what".
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MessageKind {
    // ---- unsealed: the only frames sent before a player holds a sender id
    Challenge = 1,
    Join = 2,
    Reject = 3,

    // ---- host -> player, sealed
    Welcome = 10,
    Session = 11,
    PeerClock = 12,
    PlayerJoined = 13,
    PlayerLeft = 14,
    ChecksumRequest = 15,
    Authority = 16,
    ResyncSave = 17,
    ResyncChunk = 18,
    ResyncLoad = 19,

    // ---- player -> host, sealed
    Hello = 30,
    Clock = 31,
    SpeedRequest = 32,
    ChecksumReport = 33,
    SaveDone = 34,
    ReplayFailed = 35,
    ActionRefused = 36,

    // ---- player -> host, relayed by the host to everyone else with the origin filled in
    Action = 50,
    Chat = 51,
}

impl MessageKind {
    /// What a player may send. The host drops anything else, so a player can never pass itself off as the host
    /// (session state, checksum requests, authority values, resynchronisation).
    pub const FROM_PLAYER: [MessageKind; 9] = [
        MessageKind::Hello,
        MessageKind::Clock,
        MessageKind::SpeedRequest,
        MessageKind::ChecksumReport,
        MessageKind::SaveDone,
        MessageKind::ReplayFailed,
        MessageKind::ActionRefused,
        MessageKind::Action,
        MessageKind::Chat,
    ];

    /// The kind for a header byte; `None` for a value this build does not know.
    pub fn from_u8(value: u8) -> Option<Self> {
        use MessageKind::*;
        Some(match value {
            1 => Challenge,
            2 => Join,
            3 => Reject,
            10 => Welcome,
            11 => Session,
            12 => PeerClock,
            13 => PlayerJoined,
            14 => PlayerLeft,
            15 => ChecksumRequest,
            16 => Authority,
            17 => ResyncSave,
            18 => ResyncChunk,
            19 => ResyncLoad,
            30 => Hello,
            31 => Clock,
            32 => SpeedRequest,
            33 => ChecksumReport,
            34 => SaveDone,
            35 => ReplayFailed,
            36 => ActionRefused,
            50 => Action,
            51 => Chat,
            _ => return None,
        })
    }

    pub fn is_unsealed(self) -> bool {
        matches!(self, Self::Challenge | Self::Join | Self::Reject)
    }

    pub fn allowed_from_player(self) -> bool {
        Self::FROM_PLAYER.contains(&self)
    }
}

impl From<MessageKind> for u8 {
    fn from(kind: MessageKind) -> u8 {
        kind as u8
    }
}
