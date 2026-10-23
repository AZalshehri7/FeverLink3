-- FeverLink3 player text: every system message a player reads (the notices list of the window, never the chat), in one place.
-- Plain words, the same everywhere: Host (the player who hosts), Session, Sync (everyone gets one world), Player. A message says what
-- happened and what the player can do; build numbers, file names, engine errors and internal terms stay in mod.log (the caller logs
-- them next to the notice). The session's own announcements (a sync starting or done) are written by the host's launcher:
-- core/src/session/notices.rs holds those, in the same words.
--
-- Each entry: { level, mine, theirs }. `mine` is what the player whose action it was reads; `theirs` (optional, else `mine`) what the
-- other players read. Placeholders: {whose} = "Your" / "Pat's", {who} = a player's name, {thing} = what the player did ("road",
-- "line update"), {cause} = one of T.CAUSES. No other code here: tests/lua load it as a plain module.

local T = {}

T.LEVELS = { info = true, warn = true, error = true }

T.MSG = {
	-- a build refused on its own game before anything was built anywhere (a failed check, nothing recorded, a pause order not sent)
	build_cancelled = { "warn",
		"{whose} build could not be shared with the other players and was cancelled. Nothing was built. Try again." },
	-- a build whose record is over the size one message may have
	build_too_big = { "warn",
		"{whose} build is too big to be shared with the other players and was cancelled. Try building it in smaller parts." },
	-- a kind of build that cannot travel yet (stations and depots blocked, a tool that cannot be recorded)
	build_kind_unsupported = { "warn",
		"{whose} build was cancelled: this kind of build cannot be shared with the other players yet." },
	-- the host does not let this player build
	build_not_allowed = { "warn",
		"Your build was cancelled: the host has not given you permission to build.",
		"{who}'s build was cancelled: the host has not given them permission to build." },
	-- a build that waited too long in a paused game
	build_paused_too_long = { "warn",
		"{whose} build was cancelled because the game stayed paused too long. Build it again once the game runs." },
	-- another game could not build it: cancelled everywhere
	build_refused_elsewhere = { "warn",
		"{whose} build was cancelled because it could not be built in {who}'s game ({cause}). Nothing was built. Try again." },
	-- another game could not build it, but this game had it already: the worlds differ until the sync
	build_only_here = { "warn",
		"{whose} build could not be built in {who}'s game ({cause}). A sync will bring everyone back to the same world." },
	-- applied on its own game but it cannot travel: the worlds differ until the sync
	build_not_shared_sync = { "warn",
		"{whose} build could not be shared with the other players. A sync will bring everyone back to the same world." },
	-- shared, then failed on its own game
	build_failed_after_share = { "warn",
		"{whose} build failed after it was shared. A sync will bring everyone back to the same world." },
	-- refused at its turn because the world changed in the meantime (this player's game only: nothing was built)
	build_world_changed = { "warn",
		"Your {thing} could not be built: the world changed before its turn (for example, something it needed was removed). Nothing was built. Try again." },
	-- refused at its turn because FeverLink3 could not remake it exactly as the game made it (not a world change: trying again fails the same way)
	build_not_remade = { "warn",
		"Your {thing} could not be built: FeverLink3 could not copy it exactly as your game made it, so it was not built anywhere. This is a FeverLink3 problem, not a change in the world." },
	-- an action this game made while it was catching up, refused by the host on every game
	action_undone_catching_up = { "warn",
		"Something you did while your game was catching up could not be shared and was undone. Please do it again." },
	-- the game threw in the middle of an action: the world may be damaged here, the host repairs it
	action_failed_midway = { "error",
		"Something went wrong in your game while it built. A sync will repair your world." },
	-- a window command or tool action that cannot travel: refused here too (never applied on one game alone)
	action_not_shared = { "warn",
		"Your {thing} could not be shared with the other players, so it was not applied. Try again." },
	-- stations and depots: they travel, but are still experimental
	constructions_experimental = { "info",
		"Stations and depots are still experimental in a session: one you build may not appear for the other players." },
	-- terrain tools are switched off for the session
	terrain_off = { "info",
		"Terrain tools are off during a session: terrain changes cannot be shared with the other players yet." },
	-- the game cannot stay in sync for the rest of this game run (a part of FeverLink3 is missing)
	game_cannot_sync = { "error",
		"Your game cannot stay in sync with the other players. Leave the session and restart the game.",
		"{who}'s game cannot stay in sync with the others. {who} needs to leave the session and restart the game." },
	-- another mod changed the world with its own command, which cannot be shared (the bridge's simForeign); {what} = the mod's own name (Hud.modName)
	foreign_mod_change = { "warn",
		"Another mod ({what}) changes the world on its own. FeverLink3 cannot share that yet, so the games may need extra syncs. " ..
		"Turn off its automatic changes while playing together." },
	-- one fault in FeverLink3's part of a game: the host repairs it
	game_fault = { "warn",
		"Your game hit a problem. A sync will bring everyone back to the same world.",
		"{who}'s game hit a problem. A sync will bring everyone back to the same world." },
}

-- "Report a problem" (the window's Troubleshooting and Players tab, and the pop-up while the bar icon is red;
-- launcher/src/live/report.rs does the work). The notice the other players read is the host's (core/src/session/notices.rs,
-- report_made), and so is the line a player reads when another report is still being made (report_busy).
T.REPORT = {
	button = "Report a problem",
	note = "What were you doing? (optional)",
	explain = "Makes one zip with the logs of every player in this session (Steam IDs, user names and addresses " ..
		"removed) and opens a GitHub page to attach it to. Nothing is sent anywhere by itself.",
	contents = "The zip holds:",
}

-- The automatic update (launcher/src/autoupdate.rs; `update` in ctl_status.txt). {version} = the new version, {current} = the one
-- the player keeps, {reason} = why it failed, in plain words from the module. Shown in the window and the pop-up by T.updateLine.
-- Short sentences (at most ~45 characters each): the window and the pop-up wrap long lines at a word (Hud.wrapLines), but a
-- short line reads at a glance (real game 2026-10-22: the old texts were cut at the pop-up's edge).
T.UPDATE = {
	downloading = "Downloading FeverLink3 {version}.",
	ready = "Updated to {version}. Restart the game to finish.",
	updated = "FeverLink3 is now {version}.",
	failed = "Update to {version} failed: {reason}. You keep {current}.",
	-- this game run must not play: the files on disk are another version than the one running
	restart = "Restart the game before you play together.",
	on = "Automatic updates: on",
	off = "Automatic updates: off",
	explain = "At each game start FeverLink3 looks on GitHub for a newer version and gets it ready for the next start, " ..
		"never during a session. A change here counts from the next start.",
	dev = "This copy of FeverLink3 was not made by the release page, so it does not update itself.",
}

-- Why another game could not build it ({cause}), from the technical reason (which stays in the log).
T.CAUSES = {
	missing = "a road or building it connects to is missing there",
	ambiguous = "it matches more than one road there",
	other = "the game refused it there",
}

function T.cause(why)
	why = tostring(why or "")
	if why:find("^unresolved ") then return T.CAUSES.missing end
	if why:find("^ambiguous") then return T.CAUSES.ambiguous end
	return T.CAUSES.other
end

-- What a command does, as the player calls it ({thing}). Names not listed: the command name in words ("makeLineUpdateCmd" ->
-- "line update"), as before.
T.THINGS = {
	makeWorldBuildProposalCmd = "build",
	makeBuildCmd = "build",
	makeBulldozeCmd = "bulldozing",
}

function T.thing(name)
	name = tostring(name or "")
	if T.THINGS[name] then return T.THINGS[name] end
	local s = name:gsub("^make", ""):gsub("Cmd$", "")
	s = s:gsub("(%l)(%u)", "%1 %2"):lower()
	return #s > 0 and s or "action"
end

local function fill(text, args)
	return (text:gsub("{(%w+)}", function(k)
		local v = args[k]
		return v ~= nil and tostring(v) or ""
	end))
end

-- The text and level of message `key`. `owner`: nil = the player whose action it is reads it ("Your ..."), else that player's name
-- for everyone else ("Pat's ..."). An unknown key still says something (and the caller's log has the detail).
function T.say(key, args, owner)
	local m = T.MSG[key]
	if not m then return "FeverLink3 hit a problem; details are in the log.", "warn" end
	local a = {}
	for k, v in pairs(args or {}) do a[k] = v end
	if owner == nil then
		a.whose = "Your"
		return fill(m[2], a), m[1]
	end
	a.whose = tostring(owner) .. "'s"
	a.who = a.who or tostring(owner)
	return fill(m[3] or m[2], a), m[1]
end

-- The window's line for the automatic update (ctl.update): text and level, or nil when there is nothing to say.
function T.updateLine(u)
	if type(u) ~= "table" or type(T.UPDATE[u.state]) ~= "string" then return nil end
	if u.state ~= "downloading" and u.state ~= "ready" and u.state ~= "updated" and u.state ~= "failed" then return nil end
	local text = fill(T.UPDATE[u.state], {
		version = u.version or "", current = u.current or "", reason = (u.reason and u.reason ~= "") and u.reason or "unknown problem",
	})
	if u.restart then text = text .. " " .. T.UPDATE.restart end
	return text, (u.state == "failed" or u.restart) and "warn" or "info"
end

return T
