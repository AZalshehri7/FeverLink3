//! Everything that depends on a particular game build lives here and nowhere else.
//!
//! A build is identified by the link timestamp in its PE header. Each entry carries the relative virtual
//! addresses the hooks need and the exact prologue bytes [`crate::detour::install`] must find there. An unknown
//! build means no hooks: the mod runs degraded and says so. Entries are produced by the address tool in
//! `tools/` from a copy of the game executable, never typed by hand.

/// A relative virtual address inside the game executable.
pub type Rva = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSite {
    pub rva: Rva,
    /// The first bytes of the function, whole instructions, position-independent, at least 14.
    pub prologue: &'static [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildAddrs {
    /// PE link timestamp.
    pub timestamp: u32,
    /// Human name, e.g. the Steam build id.
    pub name: &'static str,
    /// `CommandList::Add`: where every engine command enters the queue; construction tools are held here.
    pub command_list_add: Option<HookSite>,
    /// The simulation's command application: where held commands are counted and released at their stamp.
    pub apply_command: Option<HookSite>,
    /// The game-script update's thread pool construction: patched to one worker for determinism.
    pub script_pool: Option<HookSite>,
}

/// Known builds. Empty until the address tool has run against a real executable.
pub const BUILDS: &[BuildAddrs] = &[];

pub fn select(timestamp: u32) -> Option<&'static BuildAddrs> {
    select_in(BUILDS, timestamp)
}

pub fn select_in(table: &'static [BuildAddrs], timestamp: u32) -> Option<&'static BuildAddrs> {
    table.iter().find(|b| b.timestamp == timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    static FAKE: &[BuildAddrs] = &[BuildAddrs {
        timestamp: 0x1234,
        name: "test",
        command_list_add: Some(HookSite { rva: 0x1000, prologue: &[0x48; 14] }),
        apply_command: None,
        script_pool: None,
    }];

    #[test]
    fn unknown_build_selects_nothing() {
        assert!(select(0).is_none());
        assert!(select_in(FAKE, 0x9999).is_none());
    }

    #[test]
    fn known_build_selects_its_row() {
        let b = select_in(FAKE, 0x1234).unwrap();
        assert_eq!(b.name, "test");
        assert_eq!(b.command_list_add.as_ref().unwrap().rva, 0x1000);
    }

    #[test]
    fn every_recorded_prologue_is_long_enough_for_a_detour() {
        for b in BUILDS.iter().chain(FAKE) {
            for site in [&b.command_list_add, &b.apply_command, &b.script_pool].into_iter().flatten() {
                assert!(site.prologue.len() >= crate::detour::JMP_ABS_LEN, "{} has a short prologue", b.name);
            }
        }
    }
}
