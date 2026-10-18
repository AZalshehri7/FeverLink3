-- FeverLink3 link statistics: the mod's part of the latency logs (owner list 2026-10-17, items 4 and 6: the input barrier's
-- stand-stills and the build lag; docs/research/TODO.md "Logging for 4 and 6"). Observation only: pure functions over one state
-- table, no engine calls, no files; the bridge feeds them and writes the lines they make. Nothing here decides anything.
--
-- What a peer clock or an action carries on its way (core Hops, the `hop` table): g = the sending mod wrote it (sender's clock),
-- l = its launcher read it (sender's clock), h = the host relayed it (host's clock), so / sr = the sender's clock offset to the
-- host and its round trip, w = this game's launcher wrote it to to_game.log (this machine's clock), ro / rr = this machine's offset
-- and round trip; the bridge adds r, the wall time it read the line. A stamp x of a machine with offset o is x - o on the host's
-- clock, so every hop is measured on one clock (on one PC every offset is 0). Missing stamps leave their hops out.
--
-- Once a minute (S.minute) the lines, in the `name=min/median/max` form (ms, with `name_n=count`) the launcher and the host use too:
--   link.minute secs=<s> stood_s=<stand-still in the minute> holds=<n> longest_s=<s> frame_ms ... rtt_ms clock_gap_ms g2l_ms
--     l2h_ms h2w_ms w2r_ms e2e_ms act_e2e_ms build_ms build_p90_ms pace_mean=<factor> pace_max=<factor> paced_s=<s slowed>
--   (pace: the smooth pacing factor of this game, feverlink3_lockstep.lua "smooth pacing", averaged over the minute's real time)
--   link.peer peer=<id> clock_gap_ms gap_g_ms gap_l_ms gap_h_ms gap_w_ms g2l_ms l2h_ms h2w_ms w2r_ms e2e_ms rtt_ms actions act_e2e_ms
-- gap_X_ms is the gap between consecutive clock lines of that peer measured at stamp X: the first stamp at which a gap appears is
-- where it comes from (the sender's game at g, its file link at l, the network to the host at h, the host's relay or the network
-- to this game at w, this game's own reading at r = clock_gap_ms).

local S = {}

S.MINUTE_S = 60
S.KEEP = 4096   -- samples kept per group and minute (a clock line per frame is about 3600 a minute)

function S.new(clock)
	return { started = clock, peers = {}, frames = {}, builds = {}, holdN = 0, holdS = 0, longest = 0, holdSince = nil }
end

local function push(list, v)
	if type(v) == "number" and v == v and v > -math.huge and v < math.huge and #list < S.KEEP then list[#list + 1] = v end
end

local function sorted(list)
	local v = {}
	for i = 1, #list do v[i] = list[i] end
	table.sort(v)
	return v
end

-- the value at percentile p (0..100) of a sorted list, nil when empty
function S.pct(v, p)
	if #v == 0 then return nil end
	local k = math.floor(p / 100 * (#v - 1) + 0.5) + 1
	return v[math.max(1, math.min(#v, k))]
end

-- " name_n=N name=min/med/max" (whole ms), " name_n=0" without samples
function S.describe(name, list)
	if #list == 0 then return " " .. name .. "_n=0" end
	local v = sorted(list)
	return string.format(" %s_n=%s %s=%.0f/%.0f/%.0f", name, tostring(#v), name, v[1], v[math.floor(#v / 2) + 1], v[#v])
end

local GROUPS = { "gaps", "gap_g", "gap_l", "gap_h", "gap_w", "g2l", "l2h", "h2w", "w2r", "e2e", "rtt", "act_e2e" }

local function peerOf(st, id)
	local p = st.peers[id]
	if not p then
		p = { last = {}, actions = 0 }
		for _, g in ipairs(GROUPS) do p[g] = {} end
		st.peers[id] = p
	end
	return p
end

local function num(v) return type(v) == "number" and v or nil end

-- the hops of one line on the host's clock; nil entries for what cannot be measured
function S.hops(hop, r)
	if type(hop) ~= "table" then return {} end
	local g, l, h, w = num(hop.g), num(hop.l), num(hop.h), num(hop.w)
	local so, ro = num(hop.so), num(hop.ro)
	if r == 0 then r = nil end
	local out = {}
	if g and l then out.g2l = l - g end
	if l and so and h then out.l2h = h - (l - so) end
	if h and w and ro then out.h2w = (w - ro) - h end
	if w and r then out.w2r = r - w end
	if g and so and r and ro then out.e2e = (r - ro) - (g - so) end
	local sr, rr = num(hop.sr), num(hop.rr)
	if sr or rr then out.rtt = (sr or 0) + (rr or 0) end
	return out
end

-- A peer clock this game read: `clock` = C.clock() seconds (feverlink3_link.lua), `wall` = this game's wall clock in ms (0: not known yet).
function S.peerClock(st, id, hop, clock, wall)
	local p = peerOf(st, id)
	if p.clock then push(p.gaps, (clock - p.clock) * 1000) end
	p.clock = clock
	if type(hop) ~= "table" then return end
	for _, k in ipairs({ "g", "l", "h", "w" }) do
		local v = num(hop[k])
		if v then
			if p.last[k] then push(p["gap_" .. k], v - p.last[k]) end
			p.last[k] = v
		end
	end
	local x = S.hops(hop, wall)
	for _, k in ipairs({ "g2l", "l2h", "h2w", "w2r", "e2e", "rtt" }) do push(p[k], x[k]) end
	p.rtt_now = x.rtt or p.rtt_now
end

-- An action from peer `id` this game read.
function S.action(st, id, hop, wall)
	local p = peerOf(st, id)
	p.actions = p.actions + 1
	push(p.act_e2e, S.hops(hop, wall).e2e)
end

-- A peer left: its next clock does not count the outage as a gap.
function S.forget(st, id)
	st.peers[id] = nil
end

-- What the barrier's hold lines say about peer `id` now: the age of its last clock (ms) and its path round trip via the host (ms).
function S.peerNow(st, id, clock)
	local p = st.peers[id]
	if not p then return nil, nil end
	return p.clock and (clock - p.clock) * 1000 or nil, p.rtt_now
end

-- The interface frame: `dt` seconds since the frame before.
function S.frame(st, dt)
	st.lastFrame = dt * 1000
	push(st.frames, dt * 1000)
end

function S.holdStart(st, clock)
	st.holdSince = clock
end

-- `d`: the hold's whole length (s); a part an earlier minute counted already (S.minute) is not counted again
function S.holdEnd(st, clock, d)
	st.holdSince = nil
	st.holdN = st.holdN + 1
	st.holdS = st.holdS + math.max(0, d - (st.holdCarry or 0))
	st.holdCarry = nil
	if d > st.longest then st.longest = d end
end

-- The smooth pacing factor this frame (S.minute: its mean over real time, its largest value, the seconds it was above 1).
function S.pace(st, factor, clock)
	if type(factor) ~= "number" then return end
	if st.paceAt then
		local dt = math.max(0, clock - st.paceAt)
		st.paceT = (st.paceT or 0) + dt
		st.paceSum = (st.paceSum or 0) + dt * (st.paceLast or 1)
		if (st.paceLast or 1) > 1.005 then st.paceSlowS = (st.paceSlowS or 0) + dt end
	end
	st.paceAt, st.paceLast = clock, factor
	if factor > (st.paceMax or 1) then st.paceMax = factor end
end

-- A build's click-to-apply time on this game (ms).
function S.build(st, ms)
	push(st.builds, ms)
end

-- The minute's lines once S.MINUTE_S passed (else nil), then the minute starts over. stood_s counts the holds that ended in the minute
-- and the part of one still open.
function S.minute(st, clock)
	local secs = clock - st.started
	if secs < S.MINUTE_S then return nil end
	local stood = st.holdS
	local open = st.holdSince and (clock - math.max(st.holdSince, st.started)) or 0
	stood = stood + open
	local all = {}
	for _, g in ipairs(GROUPS) do all[g] = {} end
	local ids = {}
	for id in pairs(st.peers) do ids[#ids + 1] = id end
	table.sort(ids)
	local lines = {}
	for _, id in ipairs(ids) do
		local p = st.peers[id]
		local line = "link.peer peer=" .. tostring(id) .. S.describe("clock_gap_ms", p.gaps)
		for _, k in ipairs({ "gap_g", "gap_l", "gap_h", "gap_w" }) do line = line .. S.describe(k .. "_ms", p[k]) end
		for _, k in ipairs({ "g2l", "l2h", "h2w", "w2r", "e2e", "rtt" }) do line = line .. S.describe(k .. "_ms", p[k]) end
		line = line .. " actions=" .. tostring(p.actions) .. S.describe("act_e2e_ms", p.act_e2e)
		lines[#lines + 1] = line
		for _, g in ipairs(GROUPS) do
			for _, v in ipairs(p[g]) do push(all[g], v) end
			p[g] = {}
		end
		p.actions = 0
	end
	local b = sorted(st.builds)
	local p90 = S.pct(b, 90)
	local head = string.format("link.minute secs=%.0f stood_s=%.1f holds=%s longest_s=%.1f", secs, stood, tostring(st.holdN), math.max(st.longest, open))
		.. S.describe("frame_ms", st.frames) .. S.describe("rtt_ms", all.rtt) .. S.describe("clock_gap_ms", all.gaps)
	for _, k in ipairs({ "g2l", "l2h", "h2w", "w2r", "e2e", "act_e2e" }) do head = head .. S.describe(k .. "_ms", all[k]) end
	head = head .. S.describe("build_ms", st.builds) .. " build_p90_ms=" .. (p90 and string.format("%.0f", p90) or "?")
	head = head .. string.format(" pace_mean=%.2f pace_max=%.2f paced_s=%.1f", (st.paceT or 0) > 0 and st.paceSum / st.paceT or 1,
		math.max(st.paceMax or 1, st.paceLast or 1), st.paceSlowS or 0)
	st.paceT, st.paceSum, st.paceSlowS, st.paceMax = 0, 0, 0, st.paceLast or 1
	table.insert(lines, 1, head)
	st.started, st.frames, st.builds, st.holdN, st.holdS, st.longest = clock, {}, {}, 0, 0, 0
	if st.holdSince then st.holdCarry = clock - st.holdSince end
	return lines
end

return S
