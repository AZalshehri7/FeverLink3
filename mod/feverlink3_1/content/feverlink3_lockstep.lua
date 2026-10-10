-- FeverLink3 lockstep core (docs/ARCHITECTURE.md sections 5, 6, 7, 8).
--
-- Pure functions over state tables: no engine calls, no files. The bridge (feverlink3_bridge.script.lua) feeds
-- them the engine's clock and the launcher's lines; the tests feed them mocks. Every rule of the lockstep lives
-- here:
--   * session state from `session` lines: started, speed, pauseAt, lead;
--   * peer clocks from `peerclock` lines and the barrier: a game holds (speed 0) once it is more than `ahead`
--     steps past the slowest peer; a pause is a stamp and every game stops at exactly that step;
--   * stamps for local actions: now + lead, rounded up to a step; during a pause, the pause time itself;
--   * the queue of stamped items, applied when at <= game time in (stamp, sender id, sequence) order, late ones
--     flagged for the LATE log;
--   * command bodies: engine entity ids never travel; they are replaced by per-session keys;
--   * a deterministic math.random reseeded from the game time each step;
--   * checksum parts over an injectable engine accessor.

local L = {}

L.STEP = 200

function L.roundUpStep(t)
	return math.ceil(t / L.STEP) * L.STEP
end

-- ------------------------------------------------------------------ session

function L.newSession()
	return { started = false, speed = 0, pauseAt = nil, lead = 4 * L.STEP }
end

-- Applies a `session` payload {started, speed, pauseAt?, lead}. Returns true when anything changed.
function L.applySession(s, p)
	if type(p) ~= "table" then return false end
	local started = p.started == true
	local speed = tonumber(p.speed) or 0
	local pauseAt = tonumber(p.pauseAt)
	local lead = tonumber(p.lead) or s.lead
	local changed = started ~= s.started or speed ~= s.speed or pauseAt ~= s.pauseAt or lead ~= s.lead
	s.started, s.speed, s.pauseAt, s.lead = started, speed, pauseAt, lead
	return changed
end

-- The lead a game adds to its clock when stamping. The host's value is the floor: a game may raise it (minLead),
-- never lower it.
function L.leadOf(s, minLead)
	return math.max(s.lead or 0, minLead or L.STEP)
end

-- ------------------------------------------------------------------ peers and pacing

-- Applies a `peerclock` payload {from, t, ahead}; the game's own clock, relayed back, is ignored.
function L.applyPeerClock(peers, p, selfId)
	if type(p) ~= "table" or type(p.t) ~= "number" or p.from == nil or p.from == selfId then return false end
	peers[p.from] = { t = p.t, ahead = tonumber(p.ahead) }
	return true
end

function L.removePeer(peers, id)
	peers[id] = nil
end

function L.slowestPeer(peers)
	local m = nil
	for _, pc in pairs(peers) do
		if m == nil or pc.t < m then m = pc.t end
	end
	return m
end

-- How many steps this game may run ahead of the slowest peer (the `ahead` it reports in its clock). More speed
-- means more steps per engine batch, so a little more room; always below lead (2 + 2 * speed steps).
function L.aheadFor(speed)
	return 2 + math.max(1, math.min(4, speed or 1))
end

-- The speed the engine should be set to right now.
--   not started          -> 0 (every game holds at the save time)
--   barrier              -> 0 once past slowest peer + ahead steps; never a batch that would cross it
--   pause                -> 0 at pauseAt exactly; the last batch is shortened to land on it
function L.pace(s, peers, now, ahead)
	if not s.started then return 0 end
	local want = s.speed or 0
	if want <= 0 then return 0 end
	local stopAt = s.pauseAt
	local slowest = L.slowestPeer(peers)
	if slowest then
		local limit = slowest + (ahead or L.aheadFor(want)) * L.STEP
		if stopAt == nil or limit < stopAt then stopAt = limit end
	end
	if stopAt then
		if now >= stopAt then return 0 end
		local room = math.floor((stopAt - now) / L.STEP)
		if room < want then want = math.max(room, 1) end
	end
	return want
end

-- ------------------------------------------------------------------ stamps

-- The stamp of an action issued now. Stopped at a pause, every game is at exactly pauseAt, so that is the stamp
-- (the action applies during the pause everywhere). Otherwise now + lead, rounded up to a step.
function L.stampFor(s, now, minLead)
	if s.pauseAt and now >= s.pauseAt then return s.pauseAt end
	local at = L.roundUpStep(now + L.leadOf(s, minLead))
	if at <= now then at = now + L.STEP end
	return at
end

-- ------------------------------------------------------------------ queue

-- Items: { kind = "action", at, from, seq, body, key } or { kind = "checksum", at, n, paused }.
-- At one stamp, checksums come first (so every game hashes the same world), then actions in (from, seq) order.
function L.itemBefore(a, b)
	if a.at ~= b.at then return a.at < b.at end
	local ra = a.kind == "checksum" and 0 or 1
	local rb = b.kind == "checksum" and 0 or 1
	if ra ~= rb then return ra < rb end
	local fa, fb = tonumber(a.from) or 0, tonumber(b.from) or 0
	if fa ~= fb then return fa < fb end
	return (tonumber(a.seq) or 0) < (tonumber(b.seq) or 0)
end

function L.itemKey(from, seq)
	return tostring(from) .. ":" .. tostring(seq)
end

function L.enqueue(queue, item)
	queue[#queue + 1] = item
end

-- Removes and returns the items due at `now` (at <= now), sorted; each gets lateSteps (> 0 means LATE).
function L.takeDue(queue, now)
	local due, keep = {}, {}
	for _, it in ipairs(queue) do
		if type(it.at) == "number" and it.at <= now then due[#due + 1] = it else keep[#keep + 1] = it end
	end
	for i = #queue, 1, -1 do queue[i] = nil end
	for i, it in ipairs(keep) do queue[i] = it end
	table.sort(due, L.itemBefore)
	for _, it in ipairs(due) do it.lateSteps = (now - it.at) / L.STEP end
	return due
end

-- True when the interface half must apply the item itself: the game is stopped at the pause (the simulation
-- script is not called while the engine is paused) and the item is due. Everything else goes to the simulation
-- queue and applies at its exact step.
function L.applyNow(s, now, item)
	return s.pauseAt ~= nil and now >= s.pauseAt and type(item.at) == "number" and item.at <= now
end

-- ------------------------------------------------------------------ commands and entity references

-- Which arguments of each engine command are entities. Entities created by a command get the key
-- "<sender>:<seq>" of the action that created them; entities from the savegame are "s:<id>" (the save gives them
-- the same id everywhere; finding from MPFever (MIT): ids allocated during the session differ between games, ids
-- loaded from the save do not). `player` arguments are replaced by each game's own player entity.
L.COMMANDS = {
	makeVehicleBuyCmd = { player = { 1 }, entity = { 2 }, creates = "TRANSPORT_VEHICLE", result = "resultVehicleEntity", category = "spend" },
	makeLineCreateCmd = { player = { 3 }, creates = "LINE", result = "resultEntity", category = "build" },
	makeVehicleSetLineCmd = { entity = { 1, 2 }, category = "other" },
}

-- Other areas keep their schemas in modules of their own (feverlink3_cmds_*.lua, loaded by the bridge); each returns
-- { commands = { <api.cmd function name> = spec }, events = { <id> = { <name> = handler } } }. A spec has the fields
-- above and may carry two functions for arguments the generic path cannot carry (engine userdata such as proposals):
--   spec.encode(name, args, ctx) -> body | nil, reason      (same contract as L.encodeCommand)
--   spec.decode(body, ctx)       -> { n = count, ..., chain = { ... } } | nil, reason
-- and these optional ones:
--   spec.categoryFor(args)       -> category | nil          (a permission category that depends on the arguments;
--                                                            nil: the spec's own category)
--   args.chain (from decode)     further commands of the same action: argument lists or functions that make them
--                                (see L.sendNow, L.runChained)
--   spec.bindsComp, spec.bind    keys for entities the action created, beyond the single one spec.creates names
--                                (see L.bindsComp, L.bindMap)
-- and for what is no engine command, or needs the company registry (feverlink3_cmds_company.lua, docs/COMPANIES.md):
--   spec.virtual = true          a mod-level action: no api.cmd function. The bridge makes its factory in the interface
--                                state and runs spec.apply on every game instead of an engine command.
--   spec.apply(ctx, it, args) -> true, effects | nil, reason     (optional) runs on every game before the engine
--                                command is sent; ctx.co is the company registry, ctx.from the sender id, ctx.at the
--                                stamp, ctx.key the action key; a reason refuses the action on every game alike.
function L.registerCommands(commands)
	for name, spec in pairs(commands or {}) do L.COMMANDS[name] = spec end
end

-- Events. makeScriptingSendEventCmd(src, id, name, param) is one engine command for dozens of unrelated messages to
-- game scripts (loans, subsidies, town and company actions, but also notifications and tutorial steps). It has ONE
-- spec, here, and the modules register the (id, name) pairs they replicate, so none of them replaces the others':
--   handler.category                    permission category of the event
--   handler.encode(param, ctx)          -> plain param | nil, reason   (default: a plain deep copy)
--   handler.decode(param, ctx)          -> param | nil, reason         (default: the shipped value)
-- An event nobody registered stays native and is never refused: its category is "local", which no grant covers.
L.EVENTS = {}

local function eventHandler(id, name)
	local byName = L.EVENTS[id]
	return byName and byName[name] or nil
end

L.EVENT_SPEC = {
	category = "other",
	categoryFor = function(args)
		local h = eventHandler(args[2], args[3])
		return h and h.category or "local"
	end,
	encode = function(name, args, ctx)
		local h = eventHandler(args[2], args[3])
		if not h then return nil, "event " .. tostring(args[2]) .. "/" .. tostring(args[3]) .. " is not replicated" end
		local param, why
		if h.encode then
			-- a handler that raises must not throw into the window that sent the event
			local ok, p1, p2 = pcall(h.encode, args[4], ctx)
			if ok then param, why = p1, p2 else why = "encode failed: " .. tostring(p1) end
		else
			local ok, copy = pcall(ctx.plain, args[4])
			if ok then param = copy else why = copy end
		end
		if why ~= nil then return nil, "event " .. tostring(args[2]) .. "/" .. tostring(args[3]) .. ": " .. tostring(why) end
		return { fn = name, args = { n = 4, args[1], args[2], args[3], param } }
	end,
	decode = function(body, ctx)
		local a = body.args
		local h = eventHandler(a[2], a[3])
		if not h then return nil, "event " .. tostring(a[2]) .. "/" .. tostring(a[3]) .. " is unknown here" end
		local param, why = a[4], nil
		if h.decode then
			-- nor may one stop the simulation on an event another player sent
			local ok, p1, p2 = pcall(h.decode, a[4], ctx)
			if ok then param, why = p1, p2 else param, why = nil, "decode failed: " .. tostring(p1) end
		end
		if why ~= nil then return nil, "event " .. tostring(a[2]) .. "/" .. tostring(a[3]) .. ": " .. tostring(why) end
		return { n = 4, a[1], a[2], a[3], param }
	end,
}

function L.registerEvents(events)
	for id, byName in pairs(events or {}) do
		L.EVENTS[id] = L.EVENTS[id] or {}
		for name, handler in pairs(byName) do L.EVENTS[id][name] = handler end
	end
	L.COMMANDS.makeScriptingSendEventCmd = L.EVENT_SPEC
end

function L.keyFor(rev, e)
	return rev[e] or ("s:" .. tostring(e))
end

function L.resolveKey(bind, key)
	if type(key) ~= "string" then return nil end
	local saved = key:match("^s:(%d+)$")
	if saved then return tonumber(saved) end
	return bind[key]
end

-- Encodes a captured command for the wire. ctx.keyOf(entity) -> key or nil; ctx.plain(value) -> deep copy that
-- raises on values that cannot travel. Returns the body or nil and a reason (the command then stays native).
function L.encodeCommand(name, args, ctx)
	local spec = L.COMMANDS[name]
	if not spec then return nil, "no replication schema for " .. tostring(name) end
	if spec.encode then return spec.encode(name, args, ctx) end
	local n = args.n or #args
	local out = { n = n }
	for i = 1, n do
		local ok, v = pcall(ctx.plain, args[i])
		if not ok then return nil, "argument " .. i .. ": " .. tostring(v) end
		out[i] = v
	end
	for _, i in ipairs(spec.player or {}) do
		if i <= n then out[i] = { __ref = "player" } end
	end
	for _, i in ipairs(spec.entity or {}) do
		if type(args[i]) ~= "number" then return nil, "argument " .. i .. " is not an entity id" end
		local key = ctx.keyOf(args[i])
		if not key then return nil, "argument " .. i .. " has no key" end
		out[i] = { __ref = "entity", key = key }
	end
	return { fn = name, args = out }
end

-- Decodes a body on the applying game. ctx.player -> entity (the acting player's company entity in a company
-- session); ctx.resolve(key) -> entity or nil; ctx.refuse -> a reason the acting player's company cannot be resolved
-- on this game. Returns the argument list ({ n = count, ... }) or nil and a reason (the action is refused, never
-- applied with a bad id).
function L.decodeCommand(body, ctx)
	if ctx.refuse then return nil, ctx.refuse end
	if type(body) ~= "table" or type(body.args) ~= "table" then return nil, "malformed body" end
	local spec = L.COMMANDS[body.fn]
	if not spec then return nil, "unknown command " .. tostring(body.fn) end
	if spec.decode then return spec.decode(body, ctx) end
	local n = body.args.n or #body.args
	local args = { n = n }
	for i = 1, n do
		local v = body.args[i]
		if type(v) == "table" and v.__ref == "player" then
			v = ctx.player
		elseif type(v) == "table" and v.__ref == "entity" then
			local e = ctx.resolve(v.key)
			if e == nil then return nil, "unresolved entity reference " .. tostring(v.key) end
			v = e
		end
		args[i] = v
	end
	return args
end

-- The entity a command created: the newest id with the expected component that was not there before.
function L.newestEntity(before, after)
	local seen = {}
	for _, e in ipairs(before) do seen[e] = true end
	local created = nil
	for _, e in ipairs(after) do
		if not seen[e] and (created == nil or e > created) then created = e end
	end
	return created
end

-- Every entity in `after` that was not in `before`, ascending.
function L.newEntities(before, after)
	local seen, out = {}, {}
	for _, e in ipairs(before) do seen[e] = true end
	for _, e in ipairs(after) do
		if not seen[e] then out[#out + 1] = e end
	end
	table.sort(out)
	return out
end

-- Actions of more than one engine command (a depot needs its street part built before the depot itself): a schema's
-- decode may put further commands in args.chain, each an argument list or a function that makes one when its turn comes.
-- sendNow sends them one after the other at once (the simulation state runs a command as it is sent); runChained does
-- it through callbacks (the interface state), stopping at the first that fails, and hands the last result to done.
function L.sendNow(args, send)
	send(args)
	for _, entry in ipairs(args.chain or {}) do
		local nextArgs = entry
		if type(entry) == "function" then nextArgs = entry() end
		if type(nextArgs) ~= "table" then error("a further command of the action could not be made") end
		send(nextArgs)
	end
end

function L.runChained(args, send, done)
	local chain = args.chain or {}
	local function step(a, i)
		send(a, function(data, success, entities)
			if i >= #chain or not success then return done(data, success, entities) end
			local entry = chain[i + 1]
			local ok, nextArgs = pcall(function()
				if type(entry) == "function" then return entry() end
				return entry
			end)
			if ok and type(nextArgs) == "table" then
				local okS = pcall(step, nextArgs, i + 1)
				if okS then return end
			end
			return done(data, false, {})
		end)
	end
	step(args, 0)
end

-- Keys for entities an action created. A schema names the components whose new entities to look for (spec.bindsComp: a
-- name, a list of names, or a function of the body that returns either) and maps the new entities of all of them,
-- ascending, to keys (spec.bind(body, key, made) -> { [key] = entity }, complete). L.bindsComp returns the list or nil.
function L.bindsComp(spec, body)
	local c = spec.bindsComp
	if type(c) == "function" then
		local ok, v = pcall(c, body)
		c = ok and v or nil
	end
	if type(c) == "string" then return { c } end
	if type(c) ~= "table" then return nil end
	local list = {}
	for _, name in ipairs(c) do
		if type(name) == "string" then list[#list + 1] = name end
	end
	if #list == 0 then return nil end
	return list
end

function L.bindMap(spec, body, key, made)
	if not spec.bind then return {}, true end
	local ok, map, complete = pcall(spec.bind, body, key, made)
	if not ok or type(map) ~= "table" then return {}, false end
	return map, complete ~= false
end

-- The same when several creations may appear together (a player buying five vehicles in one go): the oldest new id
-- that no other watch has claimed. Watches are served in the order their commands were applied, which is the same on
-- every game, and the engine numbers entities in creation order, so the n-th command gets the n-th entity everywhere.
function L.claimEntity(before, after, claimed)
	local seen = {}
	for _, e in ipairs(before) do seen[e] = true end
	local found = nil
	for _, e in ipairs(after) do
		if not seen[e] and not (claimed and claimed[e]) and (found == nil or e < found) then found = e end
	end
	return found
end

-- ------------------------------------------------------------------ permissions

-- The host grants each player a set of categories: build, bulldoze, spend (vehicle purchases), other. A command
-- belongs to the category its COMMANDS entry names (or its categoryFor(args), for commands whose arguments decide,
-- such as events); a command without an entry is classified by its name, so a command that stays native is still
-- checked. Anything unrecognised is "other".
L.CATEGORIES = { "build", "bulldoze", "spend", "other" }

function L.categoryOf(name, args)
	local spec = L.COMMANDS[name]
	if spec and spec.categoryFor and args then
		local category = spec.categoryFor(args)
		if category then return category end
	end
	if spec and spec.category then return spec.category end
	local n = tostring(name):lower()
	if n:find("bulldoze", 1, true) or n:find("remove", 1, true) or n:find("demolish", 1, true) then return "bulldoze" end
	if n:find("buy", 1, true) or n:find("purchase", 1, true) then return "spend" end
	if n:find("construct", 1, true) or n:find("build", 1, true) or n:find("create", 1, true) then return "build" end
	return "other"
end

-- Everything is allowed until the host says otherwise.
function L.newPermissions()
	return { build = true, bulldoze = true, spend = true, other = true }
end

-- Applies a `permissions` payload {build=bool, bulldoze=bool, spend=bool, other=bool}. Only an explicit false
-- denies; a missing key keeps what the player had. Returns true when anything changed.
function L.applyPermissions(perms, p)
	if type(p) ~= "table" then return false end
	local changed = false
	for _, cat in ipairs(L.CATEGORIES) do
		if type(p[cat]) == "boolean" and perms[cat] ~= p[cat] then
			perms[cat] = p[cat]
			changed = true
		end
	end
	return changed
end

function L.permitted(perms, category)
	return perms[category] ~= false
end

-- ------------------------------------------------------------------ host authority (money)

-- The account as the checksum round saw it: what the host's report carries as `authority`, and what a player keeps
-- per round to measure its difference against at the same game time.
function L.accountSnapshot(engine)
	local ok, balance, loan = pcall(engine.account)
	if not ok then return nil end
	return { balance = balance, loan = loan }
end

-- Differences host - own for the values both sides have (balance rounded to whole money units, as a journal entry
-- holds them). Zero differences are left out.
function L.authorityDeltas(own, host)
	local out = {}
	if type(own) ~= "table" or type(host) ~= "table" then return out end
	if type(own.balance) == "number" and type(host.balance) == "number" then
		local d = math.floor(host.balance - own.balance + 0.5)
		if d ~= 0 then out.balance = d end
	end
	if type(own.loan) == "number" and type(host.loan) == "number" then
		local d = host.loan - own.loan
		if d ~= 0 then out.loan = d end
	end
	return out
end

-- Brings this game's account to the host's values: engine.bookJournal(amount) books the balance difference as a
-- journal entry, engine.adjustLoan(delta) moves the loan (absent on a build where no way is known: reported as
-- unavailable). `previous` is what the round before measured (the bridge keeps it): only a difference that is still the
-- same one round later is corrected. With replay on peers an action reaches another game a stamp's lead after the game it
-- came from, so a checksum round that falls in between sees its effect on one side only: a lag lasting less than a round,
-- not a drift (the loan taken on the host showed up twice on the guest in the first real test). Returns the deltas applied,
-- the names that could not be corrected, and every difference measured (to be passed as `previous` next round).
function L.applyAuthority(engine, own, host, previous)
	local measured = L.authorityDeltas(own, host)
	local deltas = {}
	for key, d in pairs(measured) do
		local was = previous and previous[key]
		local tolerance = key == "balance" and 50 or 0
		if type(was) == "number" and math.abs(was - d) <= tolerance then deltas[key] = d end
	end
	local applied, failed = {}, {}
	if deltas.balance then
		local ok = engine.bookJournal and pcall(engine.bookJournal, deltas.balance)
		if ok then applied.balance = deltas.balance else failed[#failed + 1] = "balance" end
	end
	if deltas.loan then
		local ok = engine.adjustLoan and pcall(engine.adjustLoan, deltas.loan)
		if ok then applied.loan = deltas.loan else failed[#failed + 1] = "loan" end
	end
	return applied, failed, measured
end

-- ------------------------------------------------------------------ deterministic random (section 7)

local TWO32 = 4294967296

-- a * b mod 2^32, exact in doubles (each partial product stays below 2^53)
local function mul32(a, b)
	local ah, al = math.floor(a / 65536), a % 65536
	return ((ah * b) % 65536 * 65536 + al * b) % TWO32
end

-- The n-th draw of a simulation step depends on the game time and n only: whatever a game drew before, and
-- whatever state the process's own generator is in, every game draws the same numbers.
function L.newRandom(timeFn)
	return { timeFn = timeFn, t = nil, n = 0 }
end

function L.randomUnit(rng)
	local ok, t = pcall(rng.timeFn)
	t = ok and tonumber(t) or 0
	if t ~= rng.t then rng.t, rng.n = t, 0 end
	rng.n = rng.n + 1
	local x = (mul32((t % TWO32) + 1, 2654435761) + mul32(rng.n, 2246822519)) % TWO32
	for _ = 1, 3 do
		x = (mul32(x, 1664525) + 1013904223) % TWO32
		x = (x + math.floor(x / 65536)) % TWO32
	end
	return x / TWO32
end

-- math.random semantics: () -> [0,1); (m) -> integer in [1,m]; (m,k) -> integer in [m,k]
function L.randomDraw(rng, m, k)
	local u = L.randomUnit(rng)
	if m == nil then return u end
	if k == nil then m, k = 1, m end
	if k < m then error("interval is empty") end
	return m + math.floor(u * (k - m + 1))
end

function L.installRandom(rng)
	math.random = function(m, k) return L.randomDraw(rng, m, k) end
	return math.random
end

-- ------------------------------------------------------------------ checksums (section 8)

-- Parts over an engine accessor:
--   engine.gameTime()       -> number
--   engine.account()        -> balance, loan
--   engine.constructions()  -> list of { file = string, x, y, z }
--   engine.lines()          -> list of { stops = number }
-- Each part is hashed separately so the launcher can name the differing one. Entity ids never enter a hash:
-- constructions are compared by file and position, lines by their stop counts, both sorted.
function L.checksumParts(engine, hashFn)
	local parts = {}
	local function part(name, fn)
		local ok, v = pcall(fn)
		parts[name] = ok and hashFn(v) or "error"
	end
	local okT, t = pcall(engine.gameTime)
	t = okT and t or -1
	parts.time = hashFn(t)
	part("balance", function() local balance = engine.account(); return balance end)
	part("loan", function() local _, loan = engine.account(); return loan end)
	part("constructions", function()
		local list = engine.constructions()
		local keys = {}
		for _, c in ipairs(list) do
			keys[#keys + 1] = string.format("%s@%.2f,%.2f,%.2f", tostring(c.file), c.x or 0, c.y or 0, c.z or 0)
		end
		table.sort(keys)
		return { n = #list, items = keys }
	end)
	part("lines", function()
		local list = engine.lines()
		local keys = {}
		for _, l in ipairs(list) do keys[#keys + 1] = tostring(l.stops or 0) end
		table.sort(keys)
		return { n = #list, items = keys }
	end)
	return parts, t
end

-- A `checksum_request` payload {n, at, paused} as a queue item.
function L.checksumItem(p)
	if type(p) ~= "table" or type(p.n) ~= "number" or type(p.at) ~= "number" then return nil end
	return { kind = "checksum", at = p.at, n = p.n, paused = p.paused == true }
end

-- An `action` record from the link as a queue item; `from` is the sender id the launcher attached.
function L.actionItem(p, from, parseBody)
	if type(p) ~= "table" or type(p.stamp) ~= "number" or type(p.seq) ~= "number" or type(p.body) ~= "string" then
		return nil, "malformed action"
	end
	local body, err = parseBody(p.body)
	if type(body) ~= "table" then return nil, "unreadable body: " .. tostring(err) end
	return { kind = "action", at = p.stamp, from = from, seq = p.seq, body = body, key = L.itemKey(from, p.seq) }
end

return L
