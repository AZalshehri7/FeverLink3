//! The session's own system messages for the players (the host announces them to every game). They are notices, not chat:
//! a [`ChatMessage`](super::ChatMessage) with a `level`, which the mod files under the window's notices
//! (`mod/feverlink3/content/feverlink3_text.lua` holds the mod's own messages, in the same plain words: Host, Session, Sync,
//! Player). A notice says what happened and what the player can do; the technical reason stays in the host's session.log.

/// How a notice is shown: `info`, `warn`, `error` (the only values a game files as a notice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// The level a relayed line may carry: one of the three names, anything else is no notice.
    pub fn parse(text: &str) -> Option<Level> {
        match text {
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

/// A sync of the session's own: its start, a player who joined or came back (every game gets the host's world and reloads).
pub const SYNC_STARTED: &str =
    "A sync starts: everyone gets the host's world. The games stand still and reload for a moment.";

/// The host started a sync by hand.
pub const SYNC_STARTED_BY_HOST: &str =
    "The host started a sync. Everyone gets the host's world; the game continues in a moment.";

/// The worlds differ and the session repairs them (the reason stays in the log).
pub const SYNC_REPAIR_STARTED: &str = "The worlds drifted apart. Syncing everyone to the host's world... \
     Anything that did not reach every player yet may be lost.";

/// The host's world may be damaged by a build that failed half-way; a player's clean world is used instead.
pub fn sync_from_player(who: &str) -> String {
    format!("A build failed half-way in the host's game. Syncing everyone to {who}'s world instead...")
}

/// The same, with no clean world anywhere: the host's is copied as it is.
pub const SYNC_DAMAGED_WORLD: &str =
    "A build failed half-way in the host's game and no clean copy of the world exists. \
     Syncing everyone to the host's world as it is; if the game misbehaves, load an earlier save.";

/// The sync is over and every game has the world of player `who`.
pub fn sync_done_from_player(who: &str) -> String {
    format!("Sync done: everyone has {who}'s world.")
}

/// The sync is over and every game has the host's world.
pub const SYNC_DONE: &str = "Sync done: everyone has the host's world.";

/// A player made a bug report: what every other player's game shows (their logs went to the host for it).
pub fn report_made(who: &str) -> String {
    format!("{who} made a bug report; your logs were sent to the host.")
}

/// The same for the host's own game, whose logs are in the report too.
pub fn report_made_host(who: &str) -> String {
    format!("{who} made a bug report; your logs are in it.")
}

/// Another report is still being made (asked by `who`): what the window of the player who asked now says.
pub fn report_busy(who: &str) -> String {
    format!("A bug report is already being made; it will reach {who}.")
}

/// This game runs a Transport Fever 3 build the installed FeverLink3 does not know (the game updated itself first): its
/// multiplayer part stays off, so it may not host and a host refuses its join. `build` is the game's build number.
pub fn game_unsupported(build: &str) -> String {
    format!(
        "Transport Fever 3 was updated (build {build}); this FeverLink3 does not support it yet. \
         An update of FeverLink3 is needed before you can play together."
    )
}

/// This game runs a build the installed FeverLink3 does not know, but its native module found every address by repeating the
/// address tool's proof (`native/src/search.rs`). It plays, and says that the build is not yet confirmed.
pub fn game_found_by_search(build: &str) -> String {
    format!(
        "Transport Fever 3 was updated (build {build}). FeverLink3 found this build by search: it is not yet confirmed.          Play only with players who have the same game build; report anything odd."
    )
}

/// A join between two games whose builds were not both found by the same search (one from the table, or other addresses).
pub fn searched_differ(host: &str, mine: &str) -> String {
    let build = |s: &str| s.split(':').next().unwrap_or("").to_owned();
    let say = |s: &str| {
        if s.is_empty() {
            "a build FeverLink3 knows".to_owned()
        } else {
            format!("build {}, found by search", build(s))
        }
    };
    if !host.is_empty() && build(host) == build(mine) {
        return format!(
            "You and the host both play build {} found by search, but the searches did not agree.",
            build(mine)
        );
    }
    format!(
        "The game builds do not match: the host plays {}, you play {}. Both need the same game build.",
        say(host),
        say(mine)
    )
}

/// What the host's window shows when such a game asked to join (it was refused).
pub fn player_game_unsupported(who: &str, build: &str) -> String {
    format!(
        "{who} could not join: their Transport Fever 3 (build {build}) is newer than their FeverLink3 supports. \
         They need an update of FeverLink3."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words a player should never read (they belong in the logs).
    const INTERNAL: [&str; 10] =
        ["checksum", "lane", " id", "stamp", "inject", "verdict", "resync", "engine", "FeverLink3:", "partly"];

    #[test]
    fn every_notice_is_plain_language() {
        let all = [
            SYNC_STARTED.to_owned(),
            SYNC_STARTED_BY_HOST.to_owned(),
            SYNC_REPAIR_STARTED.to_owned(),
            sync_from_player("Pat"),
            SYNC_DAMAGED_WORLD.to_owned(),
            sync_done_from_player("Pat"),
            SYNC_DONE.to_owned(),
            report_made("Pat"),
            report_made_host("Pat"),
            report_busy("Pat"),
            game_unsupported("40423"),
            player_game_unsupported("Pat", "40423"),
            game_found_by_search("40426"),
            searched_differ("", "40426:0123"),
            searched_differ("40426:0123", "40426:4567"),
        ];
        for text in &all {
            for word in INTERNAL {
                assert!(!text.to_lowercase().contains(&word.to_lowercase()), "{text:?} says {word:?}");
            }
        }
        assert_eq!(Some(Level::Warn), Level::parse("warn"));
        assert_eq!(None, Level::parse("chat"));
    }
}
