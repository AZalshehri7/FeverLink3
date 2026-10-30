//! `FEVERLINK3_ACTING_PLAYER` (on by default since 0.3.0, `=0` off; separate companies, `docs/COMPANIES.md`): a build is owned by the player of its own
//! command, not by the player the originator's tool happened to write.
//!
//! What the executable says (static research of build 40425, `docs/NATIVE_LINK.md` "Acting player"; NOT yet seen in the game):
//!
//! - A construction tool writes its own player into the command it adds: the street tool (`UI::StreetBuilder::UpdateEngine`,
//!   0x589a70) makes the Context with `0x57ebf0(ctx, [tool+0xc0], ...)`, which stores that player at Context+0x14 (payload +0x36c,
//!   `context.player`), and the segments and edge objects of the tool's Proposal carry the same entity as their owners (every held
//!   tool command's text: `addedSegments[i].playerOwned` = `edgeObjectsToAdd[i].playerEntity` = `context.player`). `[tool+0xc0]` is the
//!   engine's own local player, which a Lua `api.engine.util.getPlayer` override does not change (real game 2026-10-27, E6).
//! - The money follows `context.player`: the proposal data maker (0xa26560) reads it (`mov ebx,[ctx+0x14]` at 0xa2a8eb) and stores it
//!   with the cost entries in the ProposalData (+0x568, +0x550); the apply (0x9fff80) books each entry to that entity's Account
//!   component (0xa04332 -> 0xcd5d00, which looks up `ecs::component::Account`). No global local-player field is read there.
//! - The owners follow the Proposal's own members (segment `playerOwned`, edge object `playerEntity`, construction `playerEntity`).
//!
//! FeverLink3 already applies every native build through the mod's own carrier command (`inject_next`), whose Context the mod makes
//! with the acting player of the action (`ctx.player`, the sender's company in a separate-companies session). Only the Proposal of the
//! carrier comes from the originator's text, and that text names the originator's tool player as owner. So, at the injection on every
//! game, each owner in the text that is the text's own `context.player` (the tool's player) is rewritten to the carrier's
//! `context.player` before the Proposal is built and proved. One rule for every build kind; in a shared-company session both are the
//! same entity and the text is unchanged. Owners that are anybody else (nobody, -1; another company) are never touched, and the members
//! that describe what already exists (`removedSegments`) are not owners of the build.

/// The switch: on unless exactly `0` (default on since 0.3.0: in a shared-company session the text's player and the carrier's are the
/// same entity and the text is unchanged; the two-window labs of 2026-10-27 ran shared and separate sessions with it on).
pub const ACTING_VAR: &str = "FEVERLINK3_ACTING_PLAYER";

/// What the `ready` event lists when the switch is on and the injection is ready (the mod's capability check).
pub const READY_NAME: &str = "acting_player";

/// `context.player` inside a `build_proposal` payload (`portable::PAYLOAD`, `dump::PAYLOAD_FIELDS`; the street tool's Context maker
/// stores it at Context+0x14, the Context starts at +0x358).
pub const CONTEXT_PLAYER: usize = 0x36c;

/// Whether the switch is on.
pub fn switch_on(get: impl Fn(&str) -> Option<String>) -> bool {
    get(ACTING_VAR).is_none_or(|v| v != "0")
}

/// The text's own member that names the build's player.
const TEXT_PLAYER: &str = "context.player";

/// The owners of what the build adds: a segment's `playerOwned`, an edge object's `playerEntity`, a construction's `playerEntity`.
fn is_owner(name: &str) -> bool {
    let indexed = |prefix: &str, suffix: &str| {
        name.strip_prefix(prefix)
            .and_then(|r| r.strip_suffix(suffix))
            .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
    };
    indexed("proposal.addedSegments[", "].playerOwned")
        || indexed("proposal.edgeObjectsToAdd[", "].playerEntity")
        || indexed("toAdd[", "].playerEntity")
}

/// An existing entity as the text writes it (`ent:old#ID`, no position: a player is never a node), or None.
fn old_entity(v: &str) -> Option<u32> {
    let d = v.strip_prefix("ent:old#")?;
    (!d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

/// The text's `context.player`, or None when the text has none or it is not an existing entity.
pub fn text_player(text: &str) -> Option<u32> {
    text.lines().find_map(|l| l.strip_prefix(TEXT_PLAYER).and_then(|r| r.strip_prefix('\t'))).and_then(old_entity)
}

/// What [`remap_owners`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remap {
    /// The text's player (the originator's tool player).
    pub from: u32,
    /// The carrier's player (the acting player of the action on this game).
    pub to: u32,
    /// Owner members rewritten (not counting `context.player`).
    pub owners: usize,
}

/// How far (m) a point may lie from a removed segment's curve and still be on it: a split piece lies on the segment it replaces (the
/// engine cuts the same cubic), a new road crossing it leaves it within a few metres.
const ON_CURVE_M: f32 = 1.0;

/// A `v3:` value of the text (three f32 as hex bit patterns).
fn v3(v: &str) -> Option<[f32; 3]> {
    let mut it = v.strip_prefix("v3:")?.split(',').map(|h| u32::from_str_radix(h, 16).ok().map(f32::from_bits));
    let p = [it.next()??, it.next()??, it.next()??];
    (it.next().is_none() && p.iter().all(|c| c.is_finite())).then_some(p)
}

/// A segment's cubic as the text gives it: the end positions and tangents (TF's tangents carry the length, a Hermite cubic).
#[derive(Default, Clone, Copy)]
struct Curve {
    p0: Option<[f32; 3]>,
    p1: Option<[f32; 3]>,
    t0: Option<[f32; 3]>,
    t1: Option<[f32; 3]>,
}

impl Curve {
    fn at(&self, t: f32) -> Option<[f32; 3]> {
        let (p0, p1, t0, t1) = (self.p0?, self.p1?, self.t0?, self.t1?);
        let (t2, t3) = (t * t, t * t * t);
        let (h00, h10, h01, h11) = (2.0 * t3 - 3.0 * t2 + 1.0, t3 - 2.0 * t2 + t, -2.0 * t3 + 3.0 * t2, t3 - t2);
        Some(std::array::from_fn(|k| h00 * p0[k] + h10 * t0[k] + h01 * p1[k] + h11 * t1[k]))
    }

    /// The distance of `q` from this curve (a fine polyline of it), or None when the text lacks a part of it.
    fn distance(&self, q: [f32; 3]) -> Option<f32> {
        const N: usize = 256;
        let mut best = f32::MAX;
        let mut prev = self.at(0.0)?;
        for i in 1..=N {
            let cur = self.at(i as f32 / N as f32)?;
            let d: [f32; 3] = std::array::from_fn(|k| cur[k] - prev[k]);
            let w: [f32; 3] = std::array::from_fn(|k| q[k] - prev[k]);
            let len2 = d.iter().map(|c| c * c).sum::<f32>();
            let s = if len2 > 0.0 {
                (w.iter().zip(d).map(|(a, b)| a * b).sum::<f32>() / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            best = best.min((0..3).map(|k| (w[k] - s * d[k]).powi(2)).sum::<f32>().sqrt());
            prev = cur;
        }
        Some(best)
    }
}

/// `prefix[<i>].<member>` -> (i, member).
fn indexed_member<'a>(name: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let (i, member) = name.strip_prefix(prefix)?.split_once("].")?;
    Some((i.parse().ok()?, member))
}

/// The added segments (indices) that are pieces of a removed segment the text's player owns, and their `entity` values: a tool gives
/// the pieces of a segment it splits that segment's owner, and in a separate-companies session the savegame's player (the text's
/// player) is also the host's company, so a piece of the host company's road must stay the host company's, while the builder's new
/// road becomes the builder's (two-window lab 2026-10-27: a guest's road crossing the host company's highway split 4 of its segments).
/// A piece: its ends and three points between lie on the curves of such removed segments.
fn kept_pieces(text: &str, from: u32) -> (std::collections::HashSet<usize>, std::collections::HashSet<String>) {
    use std::collections::{BTreeMap, HashSet};
    #[derive(Default)]
    struct Seg {
        curve: Curve,
        owner: Option<u32>,
        entity: Option<String>,
    }
    let mut added: BTreeMap<usize, Seg> = BTreeMap::new();
    let mut removed: BTreeMap<usize, Seg> = BTreeMap::new();
    for line in text.lines() {
        let Some((name, value)) = line.split_once('\t') else { continue };
        let value = value.trim_end_matches('\r');
        let (s, m) = if let Some((i, m)) = indexed_member(name, "proposal.addedSegments[") {
            (added.entry(i).or_default(), m)
        } else if let Some((i, m)) = indexed_member(name, "proposal.removedSegments[") {
            (removed.entry(i).or_default(), m)
        } else {
            continue;
        };
        match m {
            "position0" => s.curve.p0 = v3(value),
            "position1" => s.curve.p1 = v3(value),
            "tangent0" => s.curve.t0 = v3(value),
            "tangent1" => s.curve.t1 = v3(value),
            "playerOwned" => s.owner = old_entity(value),
            "entity" => s.entity = Some(value.to_string()),
            _ => {}
        }
    }
    let theirs: Vec<Curve> = removed.values().filter(|r| r.owner == Some(from)).map(|r| r.curve).collect();
    let mut idx = HashSet::new();
    let mut ents = HashSet::new();
    for (i, s) in &added {
        if s.owner != Some(from) {
            continue;
        }
        // one piece may span two removed segments whose joint node the split merged away (lab: a highway piece over a removed node)
        let on = |q: [f32; 3]| theirs.iter().any(|r| r.distance(q).is_some_and(|d| d <= ON_CURVE_M));
        if [0.0, 0.25, 0.5, 0.75, 1.0].iter().all(|t| s.curve.at(*t).is_some_and(on)) {
            idx.insert(*i);
            if let Some(e) = &s.entity {
                ents.insert(e.clone());
            }
        }
    }
    (idx, ents)
}

/// The text with every owner that is the text's own `context.player` (and `context.player` itself) set to `to`, the carrier command's
/// player, except the pieces of a segment the text's player owns and the edge objects on them ([`kept_pieces`]). Unchanged (`owners`
/// 0) when they are the same entity, and when the text names no `context.player` at all: there is no tool player to map, the text
/// is applied as the originator's engine made it (as before the switch). Owner's real game 2026-10-30, 0.2.10: the junction lane
/// tool's node-configuration builds name no player; they were refused here and every game applied the script's weaker copy, so
/// the tool "did not work". Refused when `to` is not an existing entity (a carrier without a player: the build would belong to
/// nobody and cost nobody, never guessed).
pub fn remap_owners(text: &str, to: u32) -> Result<(String, Remap), String> {
    let Some(from) = text_player(text) else {
        return Ok((text.to_string(), Remap { from: to, to, owners: 0 }));
    };
    if to >= 0x8000_0000 {
        return Err(format!("acting player: the carrier command's context.player is {} (no player)", to as i32));
    }
    if from == to {
        return Ok((text.to_string(), Remap { from, to, owners: 0 }));
    }
    let (pieces, piece_entities) = kept_pieces(text, from);
    // the segment each added edge object sits on
    let mut object_on: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((name, value)) = line.split_once('\t') {
            if let Some((j, "segmentEntity")) = indexed_member(name, "proposal.edgeObjectsToAdd[") {
                object_on.insert(j, value.trim_end_matches('\r').to_string());
            }
        }
    }
    let kept = |name: &str| {
        if let Some((i, "playerOwned")) = indexed_member(name, "proposal.addedSegments[") {
            return pieces.contains(&i);
        }
        if let Some((j, "playerEntity")) = indexed_member(name, "proposal.edgeObjectsToAdd[") {
            return object_on.get(&j).is_some_and(|s| piece_entities.contains(s));
        }
        false
    };
    let mut out = String::with_capacity(text.len() + 64);
    let mut owners = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        let rewritten = body.split_once('\t').and_then(|(name, value)| {
            let player = name == TEXT_PLAYER;
            ((player || (is_owner(name) && !kept(name))) && old_entity(value) == Some(from)).then(|| {
                if !player {
                    owners += 1;
                }
                format!("{name}\tent:old#{to}")
            })
        });
        match rewritten {
            Some(r) => {
                out.push_str(&r);
                out.push_str(&line[body.len()..]);
            }
            None => out.push_str(line),
        }
    }
    Ok((out, Remap { from, to, owners }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // the shape of a street tool's held command text (owner run texts, live folder native_text_*.txt): the owners are the context's player
    const TEXT: &str = "=== portable v1 layout=40420 event=held ===\n\
        proposal.addedSegments.#\tn:2\n\
        proposal.addedSegments[0].node0\tent:old#64976@c53f02c6,45415ffb,3fc00000\n\
        proposal.addedSegments[0].playerOwned\tent:old#64976\n\
        proposal.addedSegments[1].playerOwned\tent:new#1\n\
        proposal.removedSegments[0].playerOwned\tent:old#64976\n\
        proposal.edgeObjectsToAdd[0].playerEntity\tent:old#64976\n\
        proposal.edgeObjectsToAdd[1].playerEntity\tent:old#7\n\
        toAdd[0].playerEntity\tent:old#64976\n\
        toRemove[0]\tent:old#64976\n\
        context.player\tent:old#64976\n\
        withCostRep\tu8:01\n";

    #[test]
    fn the_tool_players_owners_become_the_carriers_player_and_nothing_else_changes() {
        let (out, r) = remap_owners(TEXT, 52855).unwrap();
        assert_eq!(r, Remap { from: 64976, to: 52855, owners: 3 });
        let want = TEXT
            .replace("addedSegments[0].playerOwned\tent:old#64976", "addedSegments[0].playerOwned\tent:old#52855")
            .replace(
                "edgeObjectsToAdd[0].playerEntity\tent:old#64976",
                "edgeObjectsToAdd[0].playerEntity\tent:old#52855",
            )
            .replace("toAdd[0].playerEntity\tent:old#64976", "toAdd[0].playerEntity\tent:old#52855")
            .replace("context.player\tent:old#64976", "context.player\tent:old#52855");
        assert_eq!(out, want);
        // what already exists, nobody, another owner and a node that happens to have the same number stay as they are
        for kept in [
            "proposal.removedSegments[0].playerOwned\tent:old#64976",
            "proposal.addedSegments[1].playerOwned\tent:new#1",
            "proposal.edgeObjectsToAdd[1].playerEntity\tent:old#7",
            "toRemove[0]\tent:old#64976",
            "node0\tent:old#64976@",
        ] {
            assert!(out.contains(kept), "{kept}");
        }
    }

    #[test]
    fn the_same_player_and_a_text_without_a_player_leave_the_text_byte_for_byte() {
        let (out, r) = remap_owners(TEXT, 64976).unwrap();
        assert_eq!((out.as_str(), r.owners), (TEXT, 0), "a shared-company session: nothing changes");
        // the junction lane tool's node configurations name no player (owner's game 2026-10-30): applied as made, not refused
        let nodecfg = "proposal.nodeConfigsToAdd.#\tn:1\nproposal.nodeConfigsToAdd[0].entity\tent:old#39106\n";
        let (out, r) = remap_owners(nodecfg, 2).unwrap();
        assert_eq!((out.as_str(), r.owners), (nodecfg, 0));
        assert!(
            remap_owners(TEXT, u32::MAX).unwrap_err().contains("-1"),
            "a carrier without a player is never guessed"
        );
        let crlf = TEXT.replace('\n', "\r\n");
        let (out, r) = remap_owners(&crlf, 9).unwrap();
        assert_eq!(r.owners, 3);
        assert!(out.contains("context.player\tent:old#9\r\n"));
    }

    #[test]
    fn only_indexed_owner_members_are_owners_and_the_switch_is_exactly_one() {
        assert!(is_owner("proposal.addedSegments[12].playerOwned"));
        assert!(!is_owner("proposal.addedSegments[].playerOwned"));
        assert!(!is_owner("proposal.addedSegments[1].objects[0].playerOwned"));
        assert!(!is_owner("proposal.removedSegments[0].playerOwned"));
        assert_eq!(text_player("context.player\tent:old#35419\n"), Some(35419));
        assert_eq!(text_player("context.player\tent:new#1\n"), None);
        assert!(switch_on(|n| (n == ACTING_VAR).then(|| "1".to_string())));
        assert!(switch_on(|_| None));
        assert!(!switch_on(|n| (n == ACTING_VAR).then(|| "0".to_string())));
    }

    fn hex3(p: [f32; 3]) -> String {
        format!("v3:{:08x},{:08x},{:08x}", p[0].to_bits(), p[1].to_bits(), p[2].to_bits())
    }

    #[test]
    fn pieces_of_a_split_segment_keep_their_owner_and_the_new_road_is_the_builders() {
        // the host company's (64976) straight road 0..100 m is split at 40 m by a guest's road going off to the side
        let seg = |pre: &str, a: [f32; 3], b: [f32; 3], owner: &str| {
            let t: [f32; 3] = std::array::from_fn(|k| b[k] - a[k]);
            format!(
                "{pre}.position0\t{}\n{pre}.position1\t{}\n{pre}.tangent0\t{}\n{pre}.tangent1\t{}\n{pre}.playerOwned\t{owner}\n",
                hex3(a),
                hex3(b),
                hex3(t),
                hex3(t)
            )
        };
        let mut text = String::from("context.player\tent:old#64976\n");
        text += &seg("proposal.removedSegments[0]", [0.0, 0.0, 0.0], [100.0, 0.0, 0.0], "ent:old#64976");
        text += "proposal.addedSegments[0].entity\tent:new#1\n";
        text += &seg("proposal.addedSegments[0]", [0.0, 0.0, 0.0], [40.0, 0.0, 0.0], "ent:old#64976");
        text += "proposal.addedSegments[1].entity\tent:new#2\n";
        text += &seg("proposal.addedSegments[1]", [40.0, 0.0, 0.0], [100.0, 0.0, 0.0], "ent:old#64976");
        text += "proposal.addedSegments[2].entity\tent:new#3\n";
        text += &seg("proposal.addedSegments[2]", [40.0, 0.0, 0.0], [40.0, 50.0, 0.0], "ent:old#64976");
        text += "proposal.edgeObjectsToAdd[0].segmentEntity\tent:new#2\nproposal.edgeObjectsToAdd[0].playerEntity\tent:old#64976\n";
        text += "proposal.edgeObjectsToAdd[1].segmentEntity\tent:new#3\nproposal.edgeObjectsToAdd[1].playerEntity\tent:old#64976\n";
        let (out, r) = remap_owners(&text, 52855).unwrap();
        assert_eq!(r.owners, 2, "the new road and the stop on it");
        for kept in [
            "proposal.addedSegments[0].playerOwned\tent:old#64976",
            "proposal.addedSegments[1].playerOwned\tent:old#64976",
            "proposal.edgeObjectsToAdd[0].playerEntity\tent:old#64976",
        ] {
            assert!(out.contains(kept), "{kept}");
        }
        for moved in [
            "proposal.addedSegments[2].playerOwned\tent:old#52855",
            "proposal.edgeObjectsToAdd[1].playerEntity\tent:old#52855",
            "context.player\tent:old#52855",
        ] {
            assert!(out.contains(moved), "{moved}");
        }
    }
}
