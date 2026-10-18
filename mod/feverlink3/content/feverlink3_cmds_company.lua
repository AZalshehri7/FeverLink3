-- FeverLink3 command schemas: companies (several players, one economy).
--
-- Findings and design: docs/COMPANIES.md. In short: Transport Fever 3 can hold more than one player entity, but its
-- windows and its simulation scripts all act as the one local player (api.engine.util.getPlayer()), so a playable
-- second company is not provable without the game. This module therefore ships ONE shared company with a registry
-- of companies and members, a per-player ledger (who built, bought, earned what), per-player spending limits and a
-- report for the window, and builds the attribution so that real per-player companies are a mode of the same code
-- (mode "separate", off until docs/COMPANIES.md section 8 has been run in a game).
--
-- Returns { commands = { <api.cmd function name> = spec, ... }, events = { <id> = { <name> = handler } }, company = Co }.
--   commands  registered by the bridge in L.COMMANDS (feverlink3_lockstep.lua documents the spec fields):
--               makeGameAddPlayerCmd     the engine command that adds a player entity, bound to the key of its action
--               makeEntitySetPlayerCmd   moves an entity to another player (host only)
--               feverlink3Company        a mod-level action (spec.virtual: no engine command) carrying one registry
--                                        operation: found, bind, assign, join, leave, rename, open, remove, mode,
--                                        limit, perm, reset, pay
--   events    Companies/applyLevel (feverlink3_lockstep.lua, L.registerEvents)
--   company   the registry, attribution, ledger and report functions the bridge calls. Pure Lua 5.2: nothing here
--             touches the engine at load time; the few engine reads and bookings sit in Co.extendEngine.
--
-- Everything in the registry state (Co.new) is plain data (numbers, strings, booleans, tables) because the bridge keeps
-- it in the game-script state, which the savegame carries: a resync or a hot join brings the host's registry with it.
-- Operations are authorised by the sender id the launcher attached (0 = host), on every game, so a forged operation
-- is refused everywhere alike.

local Co = {}

Co.VERSION = 1
Co.NAME_MAX = 40
Co.MEMO_MAX = 60
Co.COMPANIES_MAX = 16
Co.MADE_MAX = 400 -- entities remembered with their creator; the oldest go first
Co.PLAYER_MAX = 65534
Co.AMOUNT_MAX = 1e12
Co.OPS = { "found", "bind", "assign", "join", "leave", "rename", "open", "remove", "mode", "limit", "perm", "reset", "pay" }
local CATEGORIES = { build = true, bulldoze = true, spend = true, other = true } -- as feverlink3_lockstep.lua names them

local PALETTE = {
	{ 0.2, 0.45, 0.85 }, { 0.85, 0.25, 0.25 }, { 0.2, 0.7, 0.35 }, { 0.95, 0.7, 0.15 },
	{ 0.6, 0.35, 0.8 }, { 0.15, 0.7, 0.75 }, { 0.9, 0.5, 0.15 }, { 0.5, 0.5, 0.5 },
}

-- ------------------------------------------------------------------ small helpers

local function round(x)
	if x >= 0 then return math.floor(x + 0.5) end
	return -math.floor(-x + 0.5)
end

-- The common module's integral number text (feverlink3_common.lua C.integerText: "%d" raises above 2^31 in the game's Lua); this module
-- also runs where the common module is not loadable (its own tests), then "%.0f", which is what the helper prints.
local integerText
do
	local okC, C = pcall(ug_require, "feverlink3_common.lua")
	if not okC then okC, C = pcall(ug_require, "feverlink3::/feverlink3_common.lua") end
	integerText = okC and type(C) == "table" and C.integerText or function(n) return string.format("%.0f", n) end
end

local function fmt0(n)
	return integerText(round(n))
end

-- 1234567 -> "1,234,567"
local function money(n)
	n = round(tonumber(n) or 0)
	local digits = fmt0(math.abs(n))
	local out, count = "", 0
	for i = #digits, 1, -1 do
		out = digits:sub(i, i) .. out
		count = count + 1
		if count % 3 == 0 and i > 1 then out = "," .. out end
	end
	return (n < 0 and "-" or "") .. out
end
Co.money = money

-- Text typed by a player: control characters out, trimmed, at most `max` bytes (never cutting a UTF-8 character in
-- two: names are often not ASCII). nil when nothing is left.
local function clean(s, max)
	if type(s) ~= "string" then return nil end
	s = s:gsub("[%c]", " "):gsub("^%s+", ""):gsub("%s+$", "")
	if #s > max then
		local cut = max
		while cut > 0 do
			local b = s:byte(cut + 1)
			if b >= 0x80 and b < 0xC0 then cut = cut - 1 else break end
		end
		s = s:sub(1, cut):gsub("%s+$", "")
	end
	if #s == 0 then return nil end
	return s
end

local function isId(v)
	return type(v) == "number" and v == math.floor(v) and v >= 0 and v <= Co.PLAYER_MAX
end

-- An entity key as the bridge writes them: "s:<id>" for an entity of the savegame, "<sender>:<seq>" for one a
-- command created.
local function isEntityKey(s)
	return type(s) == "string" and (s:match("^s:%d+$") ~= nil or s:match("^%d+:%d+$") ~= nil)
end

-- An amount of money: a finite number from 0 to AMOUNT_MAX.
local function amountOf(v)
	v = tonumber(v)
	if v == nil or v ~= v or v < 0 or v > Co.AMOUNT_MAX then return nil end
	return round(v)
end

local function clamp01(x)
	if x ~= x then return 0 end
	if x < 0 then return 0 end
	if x > 1 then return 1 end
	return math.floor(x * 1000 + 0.5) / 1000
end

local function component(v, name, index)
	local ok, x = pcall(function() return v[name] end)
	if ok and type(x) == "number" then return x end
	ok, x = pcall(function() return v[index] end)
	if ok and type(x) == "number" then return x end
	return nil
end

-- {r, g, b} from an engine Vec3f (fields x, y, z), from a plain {x=,y=,z=} or from an array; nil when it is none.
local function colorOf(v)
	if type(v) ~= "table" and type(v) ~= "userdata" then return nil end
	local r, g, b = component(v, "x", 1), component(v, "y", 2), component(v, "z", 3)
	if not (r and g and b) then return nil end
	return { clamp01(r), clamp01(g), clamp01(b) }
end

local function sortedKeys(t)
	local keys = {}
	for k in pairs(t) do keys[#keys + 1] = k end
	table.sort(keys, function(a, b)
		local ta, tb = type(a), type(b)
		if ta ~= tb then return ta == "number" end
		return a < b
	end)
	return keys
end

local function count(t)
	local n = 0
	for _ in pairs(t) do n = n + 1 end
	return n
end

local function copy(v)
	if type(v) ~= "table" then return v end
	local out = {}
	for k, x in pairs(v) do out[k] = copy(x) end
	return out
end

-- "makeVehicleBuyCmd" -> "vehicle buy" when the table below has no better words for it
local LABELS = {
	makeVehicleBuyCmd = "vehicles bought", makeVehicleSellCmd = "vehicles sold", makeVehicleReplaceCmd = "vehicles replaced",
	makeLineCreateCmd = "lines created", makeLineDestroyCmd = "lines deleted", makeLineUpdateCmd = "lines edited",
	makeVehicleSetLineCmd = "vehicles put on a line", makeWorldBuildProposalCmd = "builds",
	makeGameAddPlayerCmd = "companies founded", makeEntitySetPlayerCmd = "owners changed",
	makeEntitySetNameCmd = "renames", makeEntitySetColorCmd = "recolours", makeJournalBookAssetCmd = "journal bookings",
	-- events are counted as "event <id>/<name>" (the bridge names them so)
	["event Loan/Obtain"] = "loans taken", ["event Loan/Repay"] = "loans repaid",
	["event Subvention/onAccept"] = "subsidies accepted", ["event Subvention/onDecline"] = "subsidies declined",
	["event Companies/startMarketingCampaign"] = "marketing campaigns", ["event Companies/spawnIndustry"] = "prospections",
	["event Companies/MakeGreen"] = "industries greened", ["event Companies/applyLevel"] = "company levels applied",
}
local function labelOf(fn)
	if LABELS[fn] then return LABELS[fn] end
	local event = tostring(fn):match("^event (.+)$")
	if event then return (event:gsub("/", " ")) end
	local words = tostring(fn):gsub("^make", ""):gsub("Cmd$", ""):gsub("(%u)", " %1"):gsub("^%s+", "")
	return words:lower()
end

-- ------------------------------------------------------------------ registry state

-- The registry. Plain data only.
--   mode       "shared": every player acts as the one company (the game's own player entity); "separate": a player acts
--              as the company they belong to, through that company's own player entity (off until verified)
--   serial     companies founded so far (keys are "c1", "c2", ...; never reused)
--   companies  key -> { key, name, color = {r,g,b}, entity = entity key | nil, owner = player id, open = bool }
--   members    player id -> company key
--   limits     player id -> { amount, per }   spending cap, per = length of the period in game-time units (nil: for good)
--   denied     player id -> { category = true }   what the host forbade this player (build, bulldoze, spend, other)
--   ledger     player id -> see ledgerOf
--   made       entity key -> { kind, by, at }  what each player created (component name, sender id, stamp)
--   rev        counts every change; the window and the interface mirror compare it
function Co.new()
	return { v = Co.VERSION, rev = 0, mode = "shared", serial = 0, companies = {}, members = {}, limits = {}, denied = {}, ledger = {}, made = {}, nmade = 0 }
end

local function bump(co)
	co.rev = co.rev + 1
end

-- Nothing was ever configured or recorded: the registry then has no checksum part and no window.
function Co.pristine(co)
	return type(co) ~= "table" or (co.rev or 0) == 0
end

-- One player's ledger.
--   n        category -> number of actions applied (build, bulldoze, spend, other)
--   gross    category -> money spent (the cost of actions that cost money)
--   refund   money that came back (bulldozing)
--   made     command name -> how many
--   paid, received   payments between players (the `pay` operation)
--   per      { idx, gross }  the spending counted against the limit in the current period
--   refused  actions the registry refused; last = why the latest was
local function ledgerOf(co, id)
	local led = co.ledger[id]
	if not led then
		led = { n = {}, gross = {}, refund = 0, made = {}, paid = 0, received = 0, per = { idx = 0, gross = 0 }, refused = 0 }
		co.ledger[id] = led
	end
	return led
end

local function periodIndex(per, at)
	if type(per) ~= "number" or per <= 0 then return 0 end
	return math.floor((tonumber(at) or 0) / per)
end

-- What a player has spent in the current period. Read-only: a period that has rolled over counts as zero.
local function usedIn(co, id, at)
	local led = co.ledger[id]
	if not led then return 0 end
	local lim = co.limits[id]
	if led.per.idx ~= periodIndex(lim and lim.per, at) then return 0 end
	return led.per.gross
end

-- ------------------------------------------------------------------ attribution

-- Decides which entity the `player` arguments of this action become (ctx.player) before the action is decoded. ctx has
-- player (the acting game's own player entity), from (sender id), co (the registry) and resolve (key -> entity).
-- Shared mode and players outside any company keep ctx.player. In separate mode a member acts as their company's own
-- player entity; if this game does not know that entity yet, ctx.refuse says why and the action is refused (never
-- applied with a wrong id).
function Co.actor(ctx)
	local co = ctx.co
	if type(co) ~= "table" or co.mode ~= "separate" then return ctx end
	local key = co.members[ctx.from]
	local c = key and co.companies[key] or nil
	if c and c.entity then
		local e = ctx.resolve and ctx.resolve(c.entity) or nil
		if e == nil then
			ctx.refuse = "the player entity of company " .. c.name .. " is not known on this game yet"
		else
			ctx.player = e
		end
	end
	return ctx
end

-- ------------------------------------------------------------------ operations

local OPS = {}

local function needHost(ctx)
	if ctx.from ~= 0 then return nil, "only the host can do that" end
	return true
end

local function entityTaken(co, entity, except)
	for key, c in pairs(co.companies) do
		if c.entity == entity and key ~= except then return true end
	end
	return false
end

local function newCompany(co, ctx, name, color, entity, open)
	if count(co.companies) >= Co.COMPANIES_MAX then return nil, "too many companies" end
	co.serial = co.serial + 1
	local key = "c" .. fmt0(co.serial)
	co.companies[key] = {
		key = key, name = name, color = color or copy(PALETTE[(co.serial - 1) % #PALETTE + 1]),
		entity = entity, owner = ctx.from, open = open == true,
	}
	bump(co)
	return key
end

-- Host or the company's owner.
local function mayEdit(ctx, c)
	if ctx.from == 0 or ctx.from == c.owner then return true end
	return nil, "only the host or the owner of the company can do that"
end

-- found { name, color?, entity?, open? }: a new company. In separate mode it needs its player entity's key.
OPS.found = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	local name = clean(op.name, Co.NAME_MAX)
	if not name then return nil, "a company needs a name" end
	if op.entity ~= nil and not isEntityKey(op.entity) then return nil, "bad entity key" end
	if op.entity == nil and co.mode == "separate" then return nil, "in separate mode a company needs its player entity" end
	if op.entity ~= nil and entityTaken(co, op.entity) then return nil, "that player entity belongs to another company" end
	local key, err = newCompany(co, ctx, name, colorOf(op.color), op.entity, op.open)
	if not key then return nil, err end
	return true, { company = key, note = "company " .. key .. " (" .. name .. ") founded" }
end

-- bind { company, entity }: the player entity (key) a company acts through.
OPS.bind = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	local c = co.companies[op.company]
	if not c then return nil, "no such company" end
	if not isEntityKey(op.entity) then return nil, "bad entity key" end
	if entityTaken(co, op.entity, c.key) then return nil, "that player entity belongs to another company" end
	if c.entity ~= op.entity then
		c.entity = op.entity
		bump(co)
	end
	return true, { note = "company " .. c.key .. " is bound to " .. op.entity }
end

-- assign { player, company? }: the host puts a player in a company, or (no company) takes them out.
OPS.assign = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	if not isId(op.player) then return nil, "bad player" end
	if op.company ~= nil and not co.companies[op.company] then return nil, "no such company" end
	if co.members[op.player] ~= op.company then
		co.members[op.player] = op.company
		bump(co)
	end
	return true
end

-- join { company }: a player joins a company that is open (the host and the owner may always).
OPS.join = function(co, ctx, op)
	local c = co.companies[op.company]
	if not c then return nil, "no such company" end
	if not (c.open or ctx.from == 0 or ctx.from == c.owner) then return nil, "that company is closed" end
	if co.members[ctx.from] ~= c.key then
		co.members[ctx.from] = c.key
		bump(co)
	end
	return true
end

-- leave {}: the sender leaves their company.
OPS.leave = function(co, ctx, op)
	if co.members[ctx.from] ~= nil then
		co.members[ctx.from] = nil
		bump(co)
	end
	return true
end

-- rename { company, name }
OPS.rename = function(co, ctx, op)
	local c = co.companies[op.company]
	if not c then return nil, "no such company" end
	local ok, why = mayEdit(ctx, c)
	if not ok then return nil, why end
	local name = clean(op.name, Co.NAME_MAX)
	if not name then return nil, "a company needs a name" end
	if c.name ~= name then
		c.name = name
		bump(co)
	end
	return true
end

-- open { company, open }: whether players may join on their own.
OPS.open = function(co, ctx, op)
	local c = co.companies[op.company]
	if not c then return nil, "no such company" end
	local ok, why = mayEdit(ctx, c)
	if not ok then return nil, why end
	local open = op.open == true
	if c.open ~= open then
		c.open = open
		bump(co)
	end
	return true
end

-- remove { company }: the host removes a company from the registry (its player entity stays in the game).
OPS.remove = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	local c = co.companies[op.company]
	if not c then return nil, "no such company" end
	for id, key in pairs(co.members) do
		if key == c.key then co.members[id] = nil end
	end
	co.companies[c.key] = nil
	bump(co)
	return true, { note = "company " .. c.key .. " removed" }
end

-- mode { mode = "shared" | "separate", experimental? }: separate needs the host's explicit confirmation and a player
-- entity for every company (docs/COMPANIES.md section 8 says what has to be shown in a game first).
OPS.mode = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	if op.mode ~= "shared" and op.mode ~= "separate" then return nil, "unknown mode" end
	if op.mode == "separate" then
		if op.experimental ~= true then return nil, "separate companies are experimental: confirm to switch them on" end
		for _, c in pairs(co.companies) do
			if not c.entity then return nil, "company " .. c.name .. " has no player entity yet" end
		end
	end
	if co.mode ~= op.mode then
		co.mode = op.mode
		bump(co)
	end
	return true, { note = "companies are " .. op.mode }
end

-- limit { player, amount?, per? }: how much a player may spend in building and buying (amount nil removes the limit).
-- `per` is the length of the period in game-time units; nil counts for good. Setting a limit starts counting from zero.
OPS.limit = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	if not isId(op.player) then return nil, "bad player" end
	if op.amount == nil then
		if co.limits[op.player] ~= nil then
			co.limits[op.player] = nil
			bump(co)
		end
		return true
	end
	local amount = amountOf(op.amount)
	if amount == nil then return nil, "bad amount" end
	local per = op.per
	if per ~= nil and not (type(per) == "number" and per >= 1 and per <= 1e12) then return nil, "bad period" end
	per = per and round(per) or nil
	co.limits[op.player] = { amount = amount, per = per }
	local led = ledgerOf(co, op.player)
	led.per = { idx = periodIndex(per, ctx.at), gross = 0 }
	bump(co)
	return true
end

-- perm { player, category, allowed }: the host forbids (allowed = false) or allows one category of action for one player.
-- Every game checks it at the stamp, besides the grant the launcher sends to the player (docs/PROTOCOL.md, Permissions);
-- company operations are never blocked by it.
OPS.perm = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	if not isId(op.player) then return nil, "bad player" end
	if not CATEGORIES[op.category] then return nil, "unknown category" end
	if type(op.allowed) ~= "boolean" then return nil, "allowed is true or false" end
	co.denied = co.denied or {}
	local row = co.denied[op.player]
	if (row ~= nil and row[op.category] == true) == (not op.allowed) then return true end   -- nothing to change
	if op.allowed then
		row[op.category] = nil
		if next(row) == nil then co.denied[op.player] = nil end
	else
		row = row or {}
		row[op.category] = true
		co.denied[op.player] = row
	end
	bump(co)
	return true
end

-- reset { player? }: the host clears the spending counted against the limit (one player, or everyone).
OPS.reset = function(co, ctx, op)
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	if op.player ~= nil and not isId(op.player) then return nil, "bad player" end
	for id, led in pairs(co.ledger) do
		if op.player == nil or op.player == id then
			led.per = { idx = periodIndex(co.limits[id] and co.limits[id].per, ctx.at), gross = 0 }
			led.last = nil
		end
	end
	bump(co)
	return true
end

-- pay { to, amount, memo?, from? }: money from the sender to another player (the host may charge anyone: a fee). In
-- separate mode, between two different companies that both have a player entity, it is also two journal entries,
-- one on each account (effects.postings, booked by Co.runEffects); otherwise it is a ledger entry only.
OPS.pay = function(co, ctx, op)
	local from = ctx.from
	if op.from ~= nil then
		if ctx.from ~= 0 then return nil, "only the host can charge someone else" end
		if not isId(op.from) then return nil, "bad payer" end
		from = op.from
	end
	if not isId(op.to) then return nil, "bad payee" end
	if op.to == from then return nil, "nobody pays themselves" end
	local amount = amountOf(op.amount)
	if amount == nil or amount <= 0 then return nil, "bad amount" end
	local memo = clean(op.memo, Co.MEMO_MAX)
	local fx = { note = "player " .. fmt0(from) .. " paid " .. money(amount) .. " to player " .. fmt0(op.to) .. (memo and (" (" .. memo .. ")") or "") }
	local a, b = co.members[from], co.members[op.to]
	if co.mode == "separate" and a and b and a ~= b and co.companies[a].entity and co.companies[b].entity then
		fx.postings = { { company = a, amount = -amount, memo = memo }, { company = b, amount = amount, memo = memo } }
	end
	ledgerOf(co, from).paid = ledgerOf(co, from).paid + amount
	ledgerOf(co, op.to).received = ledgerOf(co, op.to).received + amount
	bump(co)
	return true, fx
end

-- One operation by the sender in ctx (from, at, key). Returns true and the effects (note, company, postings), or nil
-- and the reason. Validation happens before any change, so a refused operation changes nothing.
function Co.apply(co, ctx, op)
	if type(co) ~= "table" then return nil, "no company registry" end
	if type(op) ~= "table" or type(op.op) ~= "string" then return nil, "malformed company action" end
	local fn = OPS[op.op]
	if not fn then return nil, "unknown company action " .. op.op end
	return fn(co, ctx, op)
end

-- The company the engine command makeGameAddPlayerCmd(name, colour) creates, bound to the key of its action (ctx.key,
-- "<sender>:<seq>"): the registry entry is made by the same action that creates the entity, on every game.
function Co.addPlayer(co, ctx, name, color)
	if type(co) ~= "table" then return nil, "no company registry" end
	local ok, why = needHost(ctx)
	if not ok then return nil, why end
	local label = clean(name, Co.NAME_MAX)
	if not label then return nil, "a company needs a name" end
	if not isEntityKey(ctx.key) then return nil, "no action key to bind the company to" end
	local key, err = newCompany(co, ctx, label, colorOf(color), ctx.key, false)
	if not key then return nil, err end
	return true, { company = key, note = "company " .. key .. " (" .. label .. ") added as player entity " .. ctx.key }
end

-- ------------------------------------------------------------------ ledger and admission

-- The name the ledger and the limit know a command by: its function, or for an event (one engine command carries dozens
-- of them) "event <id>/<name>". `args` is the argument list, as shipped or as decoded: the id is at [2], the name at [3].
function Co.nameOf(fn, args)
	if fn == "makeScriptingSendEventCmd" and type(args) == "table" then
		return "event " .. tostring(args[2]) .. "/" .. tostring(args[3])
	end
	return fn
end

-- Not stopped by a spending limit: repaying a loan gives money back, it does not spend any. (A forbidden category
-- still stops it.)
local LIMIT_FREE = { ["event Loan/Repay"] = true }

function Co.freeOfLimit(fn, args)
	return LIMIT_FREE[Co.nameOf(fn, args)] == true
end

-- May this player's next action of this category go ahead? A category the host forbade never may. Spending limits apply
-- to building and buying (not to what is `free` of them, see Co.freeOfLimit); selling, bulldozing and everything else
-- always may. Pure on the registry, so the originator's pre-check and every game's check at the stamp agree.
function Co.gate(co, from, category, at, free)
	local forbidden = co.denied and co.denied[from]
	if forbidden and forbidden[category] then return false, "not allowed to " .. category end
	if free then return true end
	local lim = co.limits[from]
	if not lim or lim.amount == nil then return true end
	if category ~= "build" and category ~= "spend" then return true end
	local used = usedIn(co, from, at)
	if used >= lim.amount then
		return false, "spending limit reached (" .. money(used) .. " of " .. money(lim.amount) .. (lim.per and " in this period" or "") .. ")"
	end
	return true
end

-- The registry refused an action of this player.
function Co.refuse(co, from, why)
	local led = ledgerOf(co, from)
	led.refused = led.refused + 1
	led.last = clean(tostring(why), 120)
	bump(co)
end

local function forgetOldest(co)
	local keys = sortedKeys(co.made)
	table.sort(keys, function(a, b)
		local x, y = co.made[a], co.made[b]
		if x.at ~= y.at then return x.at < y.at end
		return a < b
	end)
	for i = 1, math.floor(Co.MADE_MAX / 4) do
		co.made[keys[i]] = nil
		co.nmade = co.nmade - 1
	end
end

-- An applied action: info = { fn, category, cost?, created? = { key, kind } }. `cost` is the money the action took
-- (negative: it paid back), nil when the account cannot be read: the action is counted and no money is booked.
-- The money part of an applied action alone (Co.record's, also used when the cost is measured a step later: Co.bookCost).
local function bookCost(co, led, from, at, category, cost)
	cost = tonumber(cost)
	if cost == nil or cost ~= cost then return false end
	cost = round(cost)
	if cost > 0 then
		led.gross[category] = (led.gross[category] or 0) + cost
		local idx = periodIndex(co.limits[from] and co.limits[from].per, at)
		if led.per.idx ~= idx then led.per = { idx = idx, gross = 0 } end
		led.per.gross = led.per.gross + cost
	elseif cost < 0 then
		led.refund = led.refund - cost
	end
	return cost ~= 0
end

function Co.record(co, from, at, info)
	local led = ledgerOf(co, from)
	local category = info.category or "other"
	led.n[category] = (led.n[category] or 0) + 1
	if info.fn then led.made[info.fn] = (led.made[info.fn] or 0) + 1 end
	bookCost(co, led, from, at, category, info.cost)
	if info.created and info.created.key then
		if co.made[info.created.key] == nil then co.nmade = co.nmade + 1 end
		co.made[info.created.key] = { kind = info.created.kind, by = from, at = tonumber(at) or 0 }
		if co.nmade > Co.MADE_MAX then forgetOldest(co) end
	end
	bump(co)
end

-- The cost of an action already recorded (counted by Co.record with no cost), measured later: the simulation state's sendCommand only
-- queues the command and the engine runs it after the script's update, so the account read right after the send has not moved yet
-- (friend test 2026-10-16: 82 builds and 15 vehicles bought, every ledger said spent 0). Gross and refund stay out of the checksum
-- (Co.digest). Returns whether money was booked.
function Co.bookCost(co, from, at, category, cost)
	if type(co) ~= "table" or from == nil then return false end
	local booked = bookCost(co, ledgerOf(co, from), from, at, category or "other", cost)
	if booked then bump(co) end
	return booked
end

-- ------------------------------------------------------------------ the bridge's two calls around an action

-- Before an action's engine command: the spending limit, then the spec's own `apply` (registry changes). Returns true
-- and the effects to run, or false and the reason (the action is refused on every game alike).
function Co.beforeApply(ctx, it, spec, args, category)
	local co = ctx.co
	if type(co) ~= "table" or it.from == nil then return true end
	if not spec.virtual then
		local ok, why = Co.gate(co, it.from, category, it.at, Co.freeOfLimit(it.body.fn, args))
		if not ok then
			Co.refuse(co, it.from, why)
			return false, why
		end
	end
	if spec.apply then
		local ok, fx = spec.apply(ctx, it, args)
		if not ok then
			Co.refuse(co, it.from, fx)
			return false, fx
		end
		return true, fx
	end
	return true
end

-- What the ledger is told about an applied action. An event (a loan, marketing, a level...) has no cost of its own: a
-- game script handles it, not the engine at once, so what the account did meanwhile is not its cost.
function Co.info(it, spec, category, cost)
	local name = Co.nameOf(it.body.fn, it.body.args)
	if name ~= it.body.fn then cost = nil end
	return { fn = name, category = category, cost = cost, created = spec.creates and { key = it.key, kind = spec.creates } or nil }
end

-- After the engine command went out: the ledger. Mod-level actions are not counted.
function Co.afterApply(ctx, it, spec, info)
	local co = ctx.co
	if type(co) ~= "table" or spec.virtual or it.from == nil then return end
	Co.record(co, it.from, it.at, info)
end

-- Runs the effects of an operation: the journal entries of a payment between companies. ctx.resolve turns a
-- company's entity key into this game's entity; engine.bookJournalFor(entity, amount, kind) books it. Returns how many
-- entries went in and the list of what could not (the games are then no longer identical in money: logged).
function Co.runEffects(ctx, fx, engine)
	local done, failed = 0, {}
	for _, p in ipairs(fx and fx.postings or {}) do
		local c = ctx.co.companies[p.company]
		local e = c and c.entity and ctx.resolve(c.entity) or nil
		local ok = e ~= nil and engine.bookJournalFor ~= nil and pcall(engine.bookJournalFor, e, p.amount, p.kind)
		if ok then
			done = done + 1
		else
			failed[#failed + 1] = "journal entry of " .. money(p.amount) .. " for company " .. tostring(p.company)
		end
	end
	return done, failed
end

-- ------------------------------------------------------------------ checksum and report

-- What the games must agree on, as a plain value for the checksum hash; nil while the registry is pristine (then
-- there is no part). Money measured at the engine (gross, refund) stays out: it is a measurement, not a command, so
-- only counts, configuration and payments are compared. A resync copies everything anyway (the registry is part of
-- the savegame's script state).
function Co.digest(co)
	if Co.pristine(co) then return nil end
	local players = {}
	for id, led in pairs(co.ledger) do
		players[id] = { n = led.n, made = led.made, paid = led.paid, received = led.received, refused = led.refused }
	end
	return { mode = co.mode, serial = co.serial, companies = co.companies, members = co.members, limits = co.limits, denied = co.denied, players = players }
end

-- The result of the lines each player created, from the engine's balance of every line: resultOf(lineKey) -> number|nil.
-- Returns player id -> money. A player none of whose lines could be read has no entry.
function Co.earned(co, resultOf)
	local out = {}
	if type(co) ~= "table" then return out end
	for key, m in pairs(co.made) do
		if m.kind == "LINE" then
			local r = resultOf(key)
			if type(r) == "number" and r == r then out[m.by] = (out[m.by] or 0) + round(r) end
		end
	end
	return out
end

local function describe(row)
	local parts = {}
	local bits = {}
	if row.built > 0 then bits[#bits + 1] = fmt0(row.built) .. " built" end
	if row.bought > 0 then bits[#bits + 1] = fmt0(row.bought) .. " bought" end
	if row.bulldozed > 0 then bits[#bits + 1] = fmt0(row.bulldozed) .. " bulldozed" end
	if row.other > 0 then bits[#bits + 1] = fmt0(row.other) .. " other" end
	parts[1] = row.name .. (row.you and " (you)" or "") .. ": " .. (#bits > 0 and table.concat(bits, ", ") or "nothing yet")
	if row.total > 0 or row.refunded > 0 then
		parts[#parts + 1] = "spent " .. money(row.total) .. (row.refunded > 0 and (", got back " .. money(row.refunded)) or "")
	end
	if row.earned ~= nil then parts[#parts + 1] = "lines " .. money(row.earned) end
	if row.limit then
		parts[#parts + 1] = "limit " .. money(row.limit.amount) .. (row.limit.per and " per period" or "") .. ", used " .. money(row.used)
	end
	if row.paid > 0 or row.received > 0 then parts[#parts + 1] = "paid " .. money(row.paid) .. ", received " .. money(row.received) end
	if row.blocked then parts[#parts + 1] = "BLOCKED" end
	if row.denied then parts[#parts + 1] = "not allowed to " .. table.concat(row.denied, ", ") end
	return table.concat(parts, "; ")
end

-- The model of the window's company page (docs/COMPANIES.md section 6 lists every field).
-- view = { self = id, names = { [id] = name }, now = game time, earned = { [id] = money } (Co.earned),
--          hostEntityKey = key of the savegame's player entity, monthLength and yearLength = the game's month and year in
--          game-time units,
--          perms = what the host allows this player (build, bulldoze, spend, other) }
function Co.status(co, view)
	co = type(co) == "table" and co or Co.new()
	view = view or {}
	local names = view.names or {}
	local earned = view.earned or {}
	local now = tonumber(view.now) or 0
	local out = {
		version = co.rev, mode = co.mode, self = view.self, isHost = view.self == 0, hostEntityKey = view.hostEntityKey,
		monthLength = view.monthLength, yearLength = view.yearLength, perms = view.perms and copy(view.perms) or nil,
		companies = {}, players = {}, lines = {}, warnings = {}, openCompanies = {},
	}
	local ids = {}
	for id in pairs(names) do ids[id] = true end
	for id in pairs(co.members) do ids[id] = true end
	for id in pairs(co.ledger) do ids[id] = true end
	for id in pairs(co.limits) do ids[id] = true end
	for id in pairs(co.denied or {}) do ids[id] = true end
	if view.self ~= nil then ids[view.self] = true end
	local byCompany = {}
	for _, id in ipairs(sortedKeys(ids)) do
		local led = co.ledger[id] or { n = {}, gross = {}, refund = 0, made = {}, paid = 0, received = 0, per = { idx = 0, gross = 0 }, refused = 0 }
		local lim = co.limits[id]
		local row = {
			id = id, name = names[id] or ("player " .. fmt0(id)), you = id == view.self, present = names[id] ~= nil, company = co.members[id],
			built = led.n.build or 0, bulldozed = led.n.bulldoze or 0, bought = led.n.spend or 0, other = led.n.other or 0,
			spent = { build = led.gross.build or 0, spend = led.gross.spend or 0, other = led.gross.other or 0, bulldoze = led.gross.bulldoze or 0 },
			refunded = led.refund, paid = led.paid, received = led.received, earned = earned[id],
			refused = led.refused, lastRefusal = led.last, made = {},
		}
		row.total = row.spent.build + row.spent.spend + row.spent.other + row.spent.bulldoze
		local forbidden = co.denied and co.denied[id]
		if forbidden then
			row.denied = {}
			for _, category in ipairs({ "build", "bulldoze", "spend", "other" }) do
				if forbidden[category] then row.denied[#row.denied + 1] = category end
			end
		end
		if row.earned ~= nil then row.net = row.earned + row.refunded - row.total end
		if lim and lim.amount ~= nil then
			row.limit = { amount = lim.amount, per = lim.per }
			row.used = usedIn(co, id, now)
			row.remaining = math.max(0, lim.amount - row.used)
			row.blocked = row.used >= lim.amount
			if row.blocked then
				out.warnings[#out.warnings + 1] = row.name .. " has reached the spending limit (" .. money(row.used) .. " of " .. money(lim.amount) .. ")"
			elseif lim.amount > 0 and row.used * 5 >= lim.amount * 4 then
				out.warnings[#out.warnings + 1] = row.name .. " has used " .. fmt0(row.used * 100 / lim.amount) .. "% of the spending limit"
			end
		end
		for _, fn in ipairs(sortedKeys(led.made)) do
			row.made[#row.made + 1] = { fn = fn, label = labelOf(fn), n = led.made[fn] }
		end
		out.players[#out.players + 1] = row
		out.lines[#out.lines + 1] = describe(row)
		if row.company then
			local list = byCompany[row.company] or {}
			list[#list + 1] = row
			byCompany[row.company] = list
		end
	end
	for _, key in ipairs(sortedKeys(co.companies)) do
		local c = co.companies[key]
		local row = {
			key = key, name = c.name, color = copy(c.color), entity = c.entity, bound = c.entity ~= nil, open = c.open == true,
			owner = c.owner, members = {}, spent = 0, refunded = 0, you = co.members[view.self] == key,
		}
		for _, p in ipairs(byCompany[key] or {}) do
			row.members[#row.members + 1] = p.id
			row.spent = row.spent + p.total
			row.refunded = row.refunded + p.refunded
		end
		out.companies[#out.companies + 1] = row
		if row.open then out.openCompanies[#out.openCompanies + 1] = key end
	end
	return out
end

-- ------------------------------------------------------------------ engine adapters

-- The few engine reads and bookings the company code needs, added to the bridge's engine accessor unless it already
-- has them (the test harness or a build that knows better replaces any of them through FEVERLINK3_ENGINE). Every one
-- is a guess about the API that the game's own scripts support but nobody has run for this mod: the findings give the
-- script that does the same, and docs/COMPANIES.md section 8 the experiment that checks it.
--   canonicalPlayer()           the savegame's player entity, whatever api.engine.util.getPlayer is later made to say
--   setLocalCompany(entity)     separate mode, EXPERIMENTAL: makes this game's windows act as that player entity by
--                               replacing api.engine.util.getPlayer (nil puts the game's own back). Returns whether
--                               getPlayer then says so. Nobody has shown that the windows follow (section 8, E6).
--                               The replacement is what MPFever's company probe tried (finding from MPFever (MIT),
--                               mpfever_bridge.script.lua:3076); its result is not published.
--   balanceOf(entity)           the account balance of a player entity; nil when infinite or unreadable
--                               (api.engine.util.finance.getPlayersBalance, gui.zip!gui/game_bar/game_bar.tl:220)
--   bookJournalFor(entity, n, kind)  one journal entry of type `kind` (default OTHER) on that player's account, like the
--                               bridge's bookJournal(amount, kind) does for the savegame's player (the game bar's balance
--                               edit does the same; finding from MPFever (MIT): JournalEntry with time -1 and category
--                               OTHER). Sent with the game's own sendCommand: the operations module owns
--                               makeJournalBookAssetCmd, and a booking made here is never a player's action.
--   lineResult(entity, t0, t1)  income minus running costs of a line between two game times
--                               (api.engine.util.finance.calculateBalance, gui.zip!gui/line_vehicle_mgmt/line_react_util.tl:626)
--   monthLength(), yearLength() the game's month and year in game-time units (api.util.getDefaultMonthDuration and
--                               getDefaultYearDuration, declared in api/tealdef/api/util.d.tl:114-120), for monthly limits
-- And while a local company is set, engine.account and engine.bookJournal (the money the checksum compares and the host
-- corrects) keep reading and booking the savegame's player, not the company the windows act as.
-- deps.sender() -> the original api.cmd.sendCommand (the bridge's wrapped one would capture the booking as a player action).
-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): setLocalCompany replaces api.engine.util.getPlayer; the API
-- has no documented way to set the player the windows act as.
function Co.extendEngine(engine, deps)
	deps = deps or {}
	-- The game's own getPlayer, kept in package.loaded: a savegame reload runs this file again in the same Lua state,
	-- and an override of the previous run must not be taken for the original (finding from MPFever (MIT), as for api.cmd).
	local realGetPlayer = nil
	pcall(function()
		local store = package.loaded["feverlink3_orig_player"]
		if type(store) ~= "table" then
			store = {}
			package.loaded["feverlink3_orig_player"] = store
		end
		if store.getPlayer == nil then store.getPlayer = api.engine.util.getPlayer end
		realGetPlayer = store.getPlayer
	end)
	if realGetPlayer == nil then pcall(function() realGetPlayer = api.engine.util.getPlayer end) end
	if engine.canonicalPlayer == nil then
		engine.canonicalPlayer = function()
			if engine.localCompanyActive and realGetPlayer then return realGetPlayer() end
			return engine.player()
		end
	end
	if engine.setLocalCompany == nil then
		engine.setLocalCompany = function(entity)
			local ok = pcall(function()
				if entity == nil then
					if realGetPlayer then api.engine.util.getPlayer = realGetPlayer end
					engine.localCompanyActive = nil
				else
					api.engine.util.getPlayer = function() return entity end
					engine.localCompanyActive = true
				end
			end)
			if not ok then return false end
			return entity == nil or api.engine.util.getPlayer() == entity
		end
	end
	if engine.balanceOf == nil then
		engine.balanceOf = function(entity) return api.engine.util.finance.getPlayersBalance(entity) end
	end
	if engine.bookJournalFor == nil then
		engine.bookJournalFor = function(entity, amount, kind)
			local entry = api.type.JournalEntry.new()
			entry.amount = amount
			entry.time = -1
			entry.category.type = api.type.JournalEntry.Type[kind or "OTHER"]
			local send = (deps.sender and deps.sender()) or api.cmd.sendCommand
			send(api.cmd.makeJournalBookAssetCmd(entity, entry))
		end
	end
	if not engine.canonicalWrapped then
		engine.canonicalWrapped = true
		local function canonical(fn)
			return function(...)
				if not (engine.localCompanyActive and realGetPlayer) then return fn(...) end
				local current = api.engine.util.getPlayer
				api.engine.util.getPlayer = realGetPlayer
				local res = { pcall(fn, ...) }
				api.engine.util.getPlayer = current
				if not res[1] then error(res[2], 0) end
				return res[2], res[3]
			end
		end
		if engine.account then engine.account = canonical(engine.account) end
		if engine.bookJournal then engine.bookJournal = canonical(engine.bookJournal) end
	end
	-- Only a line that still exists is read (nil otherwise): a registry entry outlives its line, and its key stays bound to the
	-- line's old entity id, which is gone or reused by something else. calculateBalance on such an id cost the interface frame
	-- 450-610 ms by itself, once for every line deleted, every pass (friend test 2026-10-18: the first slow companies= frame came
	-- in the same second as the first makeLineDestroyCmd, a second one per pass from the second deletion on; the lines alive, old
	-- and new, cost nothing measurable). entityExists and the LINE component are cheap reads.
	if engine.lineResult == nil then
		engine.lineResult = function(entity, from, to)
			if not (api.engine.entityExists(entity) and api.engine.getComponent(entity, api.type.ComponentType.LINE)) then return nil end
			return api.engine.util.finance.calculateBalance({ entity }, from, to, true)
		end
	end
	if engine.monthLength == nil then
		engine.monthLength = function() return api.util.getDefaultMonthDuration() end
	end
	if engine.yearLength == nil then
		engine.yearLength = function() return api.util.getDefaultYearDuration() end
	end
end

-- The engine command for a record the window wrote into ui_out.log: { op = "add", name, color = {r,g,b} } becomes
-- makeGameAddPlayerCmd, any other operation of Co.OPS the mod-level action. nil for anything else.
function Co.windowCommand(p, api)
	if type(p) ~= "table" or type(p.op) ~= "string" then return nil end
	if p.op == "add" then
		local c = colorOf(p.color) or PALETTE[1]
		return api.cmd.makeGameAddPlayerCmd(clean(p.name, Co.NAME_MAX) or "Company", api.type.Vec3f.new(c[1], c[2], c[3]))
	end
	if OPS[p.op] then return api.cmd.feverlink3Company(p) end
	return nil
end

-- ------------------------------------------------------------------ command specs

local commands = {}

-- makeGameAddPlayerCmd(name, colour): adds a player entity (the game's own mission code calls it that way; finding from
-- MPFever (MIT): its company probe does too). The entity is created by the engine on every game and
-- found by the bridge's watch (spec.creates), which binds the action's key to it; the registry entry is made by the
-- same action (apply), so the company and its entity key exist together. Host only.
commands.makeGameAddPlayerCmd = {
	category = "other",
	creates = "PLAYER", result = "resultEntity",
	encode = function(name, args, ctx)
		local label = clean(args[1], Co.NAME_MAX)
		if not label then return nil, "a company needs a name" end
		return { fn = name, args = { n = 2, label, colorOf(args[2]) or copy(PALETTE[1]) } }
	end,
	decode = function(body, ctx)
		local a = body.args
		local c = colorOf(a[2]) or PALETTE[1]
		return { n = 2, a[1], api.type.Vec3f.new(c[1], c[2], c[3]) }
	end,
	apply = function(ctx, it, args)
		return Co.addPlayer(ctx.co, ctx, args[1], args[2])
	end,
}

-- makeEntitySetPlayerCmd(entity, player): moves an entity to another player (-1: to nobody). Both arguments travel as
-- entity keys. Host only: handing assets around is a decision for the host.
commands.makeEntitySetPlayerCmd = {
	category = "other",
	encode = function(name, args, ctx)
		if type(args[1]) ~= "number" then return nil, "argument 1 is not an entity id" end
		local first = ctx.keyOf(args[1])
		if not first then return nil, "argument 1 has no key" end
		local second = -1
		if args[2] ~= -1 then
			if type(args[2]) ~= "number" then return nil, "argument 2 is not a player entity id" end
			local key = ctx.keyOf(args[2])
			if not key then return nil, "argument 2 has no key" end
			second = { __ref = "entity", key = key }
		end
		return { fn = name, args = { n = 2, { __ref = "entity", key = first }, second } }
	end,
	decode = function(body, ctx)
		local out = { n = 2 }
		for i = 1, 2 do
			local v = body.args[i]
			if type(v) == "table" and v.__ref == "entity" then
				local e = ctx.resolve(v.key)
				if e == nil then return nil, "unresolved entity reference " .. tostring(v.key) end
				v = e
			end
			out[i] = v
		end
		return out
	end,
	apply = function(ctx, it, args)
		local ok, why = needHost(ctx)
		if not ok then return nil, "only the host can change owners" end
		return true
	end,
}

-- feverlink3Company({ op = ..., ... }): one registry operation, replicated like a game command (stamped, applied on
-- every game at the stamp, in order) but with no engine command: the bridge makes its factory and runs `apply`.
-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): a factory of the mod's own in api.cmd (no engine command).
commands.feverlink3Company = {
	virtual = true,
	category = "other",
	encode = function(name, args, ctx)
		local op = args[1]
		if type(op) ~= "table" or type(op.op) ~= "string" then return nil, "a company action is a table with an op" end
		local ok, plain = pcall(ctx.plain, op)
		if not ok then return nil, tostring(plain) end
		return { fn = name, args = { n = 1, plain } }
	end,
	apply = function(ctx, it, args)
		return Co.apply(ctx.co, ctx, args[1])
	end,
}

-- Events (feverlink3_lockstep.lua, L.registerEvents). The company window sends Companies/applyLevel when the player opens a
-- rank they have earned (game_mechanics/company/company.tl:422) and the progression script of every game must apply it, so
-- it is replicated. Marketing, prospecting and greening (the permits) are the operations module's events.
local events = {
	Companies = { applyLevel = { category = "other" } },
}

return { commands = commands, events = events, company = Co }
