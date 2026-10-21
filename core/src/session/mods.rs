//! The mod-list fingerprint (`docs/PROTOCOL.md`, "Mod list").
//!
//! Two games that load the same save but run different mods (a mod missing, another revision, other files under the same
//! revision), another game build or another FeverLink3 build fall apart from the first minute: the save loads, the
//! simulation does not agree.
//!
//! Two checks. At join, the builds (game, FeverLink3, native module): a difference refuses the join. In every round that
//! sends the host's world, between the save and the transfer: the mods that WORLD uses, read from the save the host just
//! made (`app.getSavegameInfo(id)`, `SaveGameData.info.metadata.gameModDescs`, tf3-docs reference/api/app.html and
//! reference/api/api/type; `feverlink3_hud.lua` `H.worldMods`), against each player's installed mods. Revision, cosmetic
//! flag and file hash of a mod come from the installed list (`app.getUserProfile():getModRep()`, `getInstalledMods()`,
//! `getModDesc(id).revision` / `.cosmetic`, tf3-docs reference/api/api/type, `UserProfile` and `ModRep`), the hash from the
//! mod folders (session controller, `modlist.rs`).
//!
//! What blocks: a proven difference that changes the simulation. The game's own `cosmetic` flag in `mod.json` ("If the mod is
//! purely cosmetic"; tf3-docs mod-structure: "Leave it false if the mod affects the simulation") is the only documented way
//! to tell a pure UI/visual mod apart, so a difference in a mod the host's game marks cosmetic only warns. Mods only the
//! player has never block (the save names the mods it loads). A list that could not be read, or a comparison that cannot
//! be made, warns loudly and does not refuse: only a proven mismatch blocks.
//!
//! The Join carries a compact form ([`ModTags`]: 4-byte tags, the first frame of a connection is limited to 4096 bytes); a
//! refused player gets the host's list ([`ModFingerprint`] in the Reject or Kicked) and computes the readable diff ([`diff`]).

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Most mods a Join names (each costs 8 bytes, about 11 characters of base64); more are left out and `more` is set.
pub const MAX_JOIN_MODS: usize = 256;
/// Most mods a fingerprint keeps; the rest are dropped (sorted by id).
pub const MAX_MODS: usize = 1024;
const TEXT_MAX: usize = 96;

/// One installed mod.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModEntry {
    /// `modId` of its `mod.json`.
    pub id: String,
    /// `revision` of its `mod.json`.
    pub revision: i64,
    /// `cosmetic` of its `mod.json`: the author says it does not affect the simulation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cosmetic: bool,
    /// Hash of the mod's files (hex), when its folder was found; `None`: compared by revision only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Everything about one game that must be equal to play together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModFingerprint {
    /// The game's build (`getBuildVersion()`).
    pub game: String,
    /// The FeverLink3 version.
    pub feverlink3: String,
    /// The native module's build (hash of its file), empty when unknown.
    #[serde(default)]
    pub native: String,
    pub mods: Vec<ModEntry>,
    /// The game could not list its mods (`mods` is empty and means nothing): nothing is compared, it is said instead.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unread: bool,
}

/// What a Join says about the player's game: the three build strings in clear and, per mod, 4 bytes of
/// `SHA-256(id, revision)` and 4 bytes of `SHA-256(id, revision, files)` (zero when the files are unknown), base64.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModTags {
    pub game: String,
    pub feverlink3: String,
    #[serde(default)]
    pub native: String,
    #[serde(default)]
    pub tags: String,
    /// The player has more mods than the join can name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub more: bool,
    /// The player's game could not list its mods.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unread: bool,
}

fn clip(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(TEXT_MAX).collect()
}

fn tag(parts: &[&[u8]]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(b"FeverLink3 mod");
    for p in parts {
        h.update((p.len() as u32).to_be_bytes());
        h.update(p);
    }
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

fn id_tag(e: &ModEntry) -> [u8; 4] {
    tag(&[e.id.as_bytes(), &e.revision.to_be_bytes()])
}

fn files_tag(e: &ModEntry) -> [u8; 4] {
    match &e.files {
        Some(f) => {
            let t = tag(&[e.id.as_bytes(), &e.revision.to_be_bytes(), f.as_bytes()]);
            // zero means "unknown"
            if t == [0; 4] {
                [0, 0, 0, 1]
            } else {
                t
            }
        }
        None => [0; 4],
    }
}

impl ModFingerprint {
    /// Sorted by id, one entry per id (the first wins), texts clipped, at most [`MAX_MODS`]; the base game's own entry
    /// (empty id) is left out, `game` stands for it.
    pub fn normalized(mut self) -> ModFingerprint {
        self.game = clip(&self.game);
        self.feverlink3 = clip(&self.feverlink3);
        self.native = clip(&self.native);
        let mut seen = BTreeMap::new();
        for mut e in self.mods {
            e.id = clip(&e.id);
            e.files = e.files.map(|f| clip(&f));
            if !e.id.is_empty() {
                seen.entry(e.id.clone()).or_insert(e);
            }
        }
        self.mods = seen.into_values().take(MAX_MODS).collect();
        self
    }

    /// The compact form a Join carries.
    pub fn tags(&self) -> ModTags {
        let mut bytes = Vec::with_capacity(8 * self.mods.len().min(MAX_JOIN_MODS));
        for e in self.mods.iter().take(MAX_JOIN_MODS) {
            bytes.extend_from_slice(&id_tag(e));
            bytes.extend_from_slice(&files_tag(e));
        }
        ModTags {
            game: self.game.clone(),
            feverlink3: self.feverlink3.clone(),
            native: self.native.clone(),
            tags: STANDARD.encode(bytes),
            more: self.mods.len() > MAX_JOIN_MODS,
            unread: self.unread,
        }
    }

    /// The join's check: the builds. Each difference is a line for the host's log; any refuses the join.
    pub fn check_builds(&self, guest: &ModTags) -> Vec<String> {
        let builds = [
            ("game build", &self.game, &guest.game),
            ("FeverLink3", &self.feverlink3, &guest.feverlink3),
            ("FeverLink3 native module", &self.native, &guest.native),
        ];
        builds
            .into_iter()
            // an empty build is one a game could not read: not a proven difference
            .filter(|(_, mine, theirs)| !mine.is_empty() && !theirs.is_empty() && mine != theirs)
            .map(|(what, mine, theirs)| format!("{what}: host {}, player {}", shown(mine), shown(&clip(theirs))))
            .collect()
    }

    /// The fingerprint of the mods a world uses: the builds of this one and, for each id the world's save names, its
    /// installed entry. The second value names the world's mods this list does not have (they cannot be compared).
    pub fn world(&self, ids: &[String]) -> (ModFingerprint, Vec<String>) {
        let own: BTreeMap<&str, &ModEntry> = self.mods.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut unknown = Vec::new();
        let mut mods = Vec::new();
        for id in ids {
            match own.get(clip(id).as_str()) {
                Some(e) => mods.push((*e).clone()),
                None if id.is_empty() => {}
                None => unknown.push(clip(id)),
            }
        }
        let world = ModFingerprint { mods, ..self.clone() }.normalized();
        unknown.sort();
        unknown.dedup();
        (world, unknown)
    }

    /// The host's check of a joining player: the differences that refuse the join (empty: welcome) and the ones that
    /// only warn, each a line for the host's log. Without the player's full list the host names its own mods only.
    pub fn check(&self, guest: &ModTags) -> (Vec<String>, Vec<String>) {
        let (mut block, mut warn) = (self.check_builds(guest), Vec::new());
        if guest.unread {
            warn.push("the player's game could not list its mods: they were not compared".to_owned());
            return (block, warn);
        }
        let raw = STANDARD.decode(guest.tags.as_bytes()).unwrap_or_default();
        let theirs: BTreeMap<[u8; 4], [u8; 4]> =
            raw.as_chunks::<8>().0.iter().map(|c| ([c[0], c[1], c[2], c[3]], [c[4], c[5], c[6], c[7]])).collect();
        for e in &self.mods {
            let problem = match theirs.get(&id_tag(e)) {
                None if guest.more => {
                    // not proven: the join names only the first MAX_JOIN_MODS of a player's mods
                    warn.push(format!("{} r{}: not compared (the player has too many mods to list)", e.id, e.revision));
                    None
                }
                None => Some(format!("{} r{}: missing or another revision on the player's game", e.id, e.revision)),
                Some(f) => {
                    let mine = files_tag(e);
                    if *f == [0; 4] || mine == [0; 4] {
                        if *f != mine {
                            warn.push(format!(
                                "{} r{}: files not compared (folder not found on one side)",
                                e.id, e.revision
                            ));
                        }
                        None
                    } else if *f != mine {
                        Some(format!("{} r{}: same revision, different files", e.id, e.revision))
                    } else {
                        None
                    }
                }
            };
            match problem {
                Some(p) if e.cosmetic => warn.push(format!("{p} (cosmetic)")),
                Some(p) => block.push(p),
                None => {}
            }
        }
        (block, warn)
    }
}

/// Who must update when the FeverLink3 versions differ, as the end of the diff line (`launcher/src/autoupdate.rs`
/// updates the module at a game start, unless the player turned it off).
fn update_advice(host: &str, mine: &str) -> &'static str {
    // major.minor.patch, numbers only; a pre-release suffix or an unreadable version gives no advice
    let core = |v: &str| -> Option<(u64, u64, u64)> {
        let mut it = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
        let t = (it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(t)
    };
    match (core(host), core(mine)) {
        (Some(h), Some(m)) if h > m => {
            ". An update is available: restart your game and FeverLink3 updates itself (Automatic updates, in Troubleshooting)"
        }
        (Some(h), Some(m)) if h < m => ". The host needs to restart their game so that FeverLink3 updates itself",
        _ => "",
    }
}

fn shown(s: &str) -> &str {
    if s.is_empty() {
        "unknown"
    } else {
        s
    }
}

/// One difference between the host's game and this player's, as the player's window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModDifference {
    /// It refuses the join (else it only warns).
    pub blocks: bool,
    pub text: String,
}

/// The player's readable diff of the host's list (from a Reject) against its own: blocking lines first.
pub fn diff(host: &ModFingerprint, mine: &ModFingerprint) -> Vec<ModDifference> {
    let mut out = Vec::new();
    let mut add = |blocks: bool, text: String| out.push(ModDifference { blocks, text });
    if mine.unread {
        add(false, "your game could not list its mods: they were not compared".to_owned());
    }
    let builds = [
        ("the game", &host.game, &mine.game),
        ("FeverLink3", &host.feverlink3, &mine.feverlink3),
        ("the FeverLink3 module (winhttp.dll)", &host.native, &mine.native),
    ];
    for (what, h, m) in builds {
        if !h.is_empty() && !m.is_empty() && h != m {
            // FeverLink3 itself: say who has to update (the module updates itself at the next game start)
            let advice = if what == "FeverLink3" { update_advice(h, m) } else { "" };
            add(true, format!("{what}: host has {}, you have {}{advice}", shown(h), shown(m)));
        }
    }
    let own: BTreeMap<&str, &ModEntry> = mine.mods.iter().map(|e| (e.id.as_str(), e)).collect();
    for h in &host.mods {
        let note = if h.cosmetic { " (cosmetic: warning only)" } else { "" };
        match own.get(h.id.as_str()) {
            None => add(!h.cosmetic, format!("missing {} (host has revision {}){note}", h.id, h.revision)),
            // the host's flag decides, as in the host's own check
            Some(m) if m.revision != h.revision => add(
                !h.cosmetic,
                format!("{}: host has revision {}, you have revision {}{note}", h.id, h.revision, m.revision),
            ),
            Some(m) => match (&h.files, &m.files) {
                (Some(a), Some(b)) if a != b => {
                    add(!h.cosmetic, format!("{} revision {}: same revision, different files{note}", h.id, h.revision))
                }
                (Some(_), None) | (None, Some(_)) => {
                    add(false, format!("{}: files not compared (mod folder not found on one side)", h.id))
                }
                _ => {}
            },
        }
    }
    // the player's other installed mods, in one line: the host's world does not load them
    let theirs: BTreeMap<&str, ()> = host.mods.iter().map(|e| (e.id.as_str(), ())).collect();
    let extra: Vec<&str> = mine.mods.iter().map(|m| m.id.as_str()).filter(|id| !theirs.contains_key(id)).collect();
    // (a host that names no mods refused over its builds only)
    if !extra.is_empty() && !host.mods.is_empty() {
        let more = if extra.len() > 5 { format!(" and {} more", extra.len() - 5) } else { String::new() };
        let names = extra.iter().take(5).copied().collect::<Vec<_>>().join(", ");
        add(false, format!("only you have {names}{more}; the host's world does not load them"));
    }
    out.sort_by_key(|d| !d.blocks);
    out
}
