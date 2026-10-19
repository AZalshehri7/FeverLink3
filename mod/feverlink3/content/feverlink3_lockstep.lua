-- FeverLink3 lockstep core (docs/ARCHITECTURE.md sections 5, 6, 7, 8).
--
-- Pure functions over state tables: no engine calls, no files. The bridge (feverlink3_bridge.script.lua) feeds
-- them the engine's clock and the launcher's lines; the tests feed them mocks. Every rule of the lockstep lives
-- here:
--   * session state from `session` lines: started, speed, pauseAt, lead;
--   * peer clocks from `peerclock` lines and the pacing: a game holds (speed 0) once it is more than `ahead`
--     steps past the slowest peer; a pause is a stamp and every game stops at exactly that step;
--   * the input barrier: every clock carries a promise (`safe`: nothing its sender ships later is stamped before it),
--     and a game never runs into a step it may still get a peer's input for (L.promise, L.barrier, L.pace);
--   * stamps for local actions: now + lead, rounded up to a step and never before this game's own promise; during a
--     pause, the pause time itself;
--   * the queue of stamped items, applied when at <= game time in (stamp, sender id, sequence) order, late ones
--     flagged: a late apply is a fork, logged and repaired;
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
-- `s.loadedAt` (L.loadedAt): this world was loaded by a resync at that time. A pause before it is the pause of the world this game
-- left, a step this world never stood at: every game loaded the same save, so every game stands at loadedAt, and that is the pause.
-- Real friend test 2026-10-16: the session still said pauseAt 10341800 after both games had loaded the save at 10342400; a build
-- stamped at the stale pause reached the other game 3 steps after its stamp (LATE, a fork, another resync).
function L.applySession(s, p)
	if type(p) ~= "table" then return false end
	local started = p.started == true
	local speed = tonumber(p.speed) or 0
	local pauseAt = tonumber(p.pauseAt)
	if pauseAt and type(s.loadedAt) == "number" and pauseAt < s.loadedAt then pauseAt = s.loadedAt end
	local lead = tonumber(p.lead) or s.lead
	-- the stalled games (core SessionState::stalled, host/stall.rs): a list of player ids
	local stalled, key = {}, {}
	for _, id in ipairs(type(p.stalled) == "table" and p.stalled or {}) do
		if type(id) == "number" then stalled[id] = true; key[#key + 1] = id end
	end
	table.sort(key)
	key = table.concat(key, ",")
	local changed = started ~= s.started or speed ~= s.speed or pauseAt ~= s.pauseAt or lead ~= s.lead or key ~= (s.stalledKey or "")
	-- the pause that just ended: an item stamped at it may still arrive while this game sits at that time (L.applyNow)
	if s.pauseAt ~= nil and pauseAt == nil then s.lastPauseAt = s.pauseAt end
	s.started, s.speed, s.pauseAt, s.lead = started, speed, pauseAt, lead
	s.stalled, s.stalledKey = stalled, key
	return changed
end

-- Whether the host has game `id` as stalled (host/stall.rs): this game, if it is that one, has every action it ships stamped by the host
-- (the bridge's X.hostStamps) and catches up; the others run on without waiting for it.
function L.isStalled(s, id)
	return type(s.stalled) == "table" and s.stalled[id] == true
end

-- This world was loaded by a resync at game time `t`: no pause of the session may lie before it (L.applySession), and one that
-- already does moves to `t`. Returns the pause it replaced, or nil.
function L.loadedAt(s, t)
	if type(t) ~= "number" then return nil end
	s.loadedAt = t
	local old = s.pauseAt
	if old and old < t then
		s.pauseAt = t
		return old
	end
	return nil
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
	local need = tonumber(p.need)
	if need then need = math.max(0, math.min(L.MARGIN_MAX, math.floor(need))) end
	-- the promise is taken as it is, never as the larger of old and new: the sender lowers it at a pause (L.promise)
	-- `stalled`: the host keeps this promise for a game that stopped answering (host/stall.rs); its `t` is where it stopped
	peers[p.from] = { t = p.t, ahead = tonumber(p.ahead), need = need, safe = tonumber(p.safe), pause = p.pause == true, stalled = p.stalled == true or nil }
	return true
end

function L.removePeer(peers, id)
	peers[id] = nil
end

-- The slowest peer clock this game paces itself to. A stalled peer (the host keeps its promise, host/stall.rs) is not waited for: it
-- catches up by itself, and its promise still bounds this game (L.barrier).
function L.slowestPeer(peers)
	local m = nil
	for _, pc in pairs(peers) do
		if not pc.stalled and (m == nil or pc.t < m) then m = pc.t end
	end
	return m
end

-- The furthest clock of the peers that run (nil: none).
function L.furthestPeer(peers)
	local m = nil
	for _, pc in pairs(peers) do
		if type(pc) == "table" and type(pc.t) == "number" and not pc.stalled and (m == nil or pc.t > m) then m = pc.t end
	end
	return m
end

-- Catching up: the host has this game as stalled (`s.self`, this game's id, set by the bridge; host/stall.rs: its clocks did not come for
-- the stall timeout and the others ran on) and it is behind the furthest peer that runs. It then asks the engine for batches of up to
-- L.CATCHUP_SPEED steps whatever the session's speed (the steps are the same steps: the engine runs `speed` of them per batch, and L.pace
-- already sets fewer to land on a stop) and, with smooth pacing, a shorter batch interval. Only while stalled: two games in step see each
-- other a few steps behind (a peer's clock is a one-way trip old), and must not take turns to race. Returns the steps it is behind, or nil.
L.CATCHUP_SPEED = 4
function L.catchUp(s, peers, now)
	if not s.started or (s.speed or 0) <= 0 or s.pauseAt ~= nil or s.holdAt ~= nil then return nil end
	if s.self == nil or not L.isStalled(s, s.self) then return nil end
	local far = L.furthestPeer(peers)
	if not far or far <= now then return nil end
	return (far - now) / L.STEP
end

-- How many steps this game may run ahead of the slowest peer (the `ahead` it reports in its clock). More speed
-- means more steps per engine batch, so a little more room. With the session's `lead` (game-time units) never less than the lead
-- in steps: a peer's input barrier sits at its clock plus the lead, and a pacing limit nearer than that would take the room the lead
-- gives (the host's lead holds one engine batch on top of the one-way time since 2026-10-18; at x4 it is 7 steps or more, this
-- limit was 6).
function L.aheadFor(speed, lead)
	local a = 2 + math.max(1, math.min(4, speed or 1))
	if type(lead) == "number" and lead > 0 then a = math.max(a, math.ceil(lead / L.STEP)) end
	return a
end

-- The speed the engine should be set to right now.
--   not started          -> 0 (every game holds at the save time)
--   input barrier        -> 0 once the next step would pass L.barrier (a step a peer may still send an input for); never a
--                           batch that would cross it. Exact, unlike the rest: a game that holds here waits for a clock.
--   pacing               -> 0 once past slowest peer + ahead steps; never a batch that would cross it
--   pause                -> 0 at pauseAt exactly; the last batch is shortened to land on it
--   landing marks        -> `marks` (checksum stamps) never hold the game; the batch is shortened so that one lands
--                           exactly on the mark. At x3/x4 a batch is several steps, and games whose batches fall at
--                           different phases would otherwise hash their checksum at different sim times (the host then
--                           skips the round: ChecksumOutcome.time_skew).
function L.pace(s, peers, now, ahead, marks)
	if not s.started then return 0 end
	-- holdAt: every game holds at the place of a build in the pause order until its record came (the bridge's X.slotHold), the resume too
	local hold = type(s.holdAt) == "number" and s.holdAt or nil
	if hold and now >= hold then return 0 end
	local want = s.speed or 0
	-- standing at a pause, with an action of the pause the host stamped further on (pauseRun: one step past the furthest place a game
	-- stands, see the bridge's pause order): run on to it, one step a batch, the least the pause's world can move. The simulation applies
	-- it at its stamp, so overshooting it changes nothing.
	local run = s.pauseAt ~= nil and now >= s.pauseAt and type(s.pauseRun) == "number" and now < s.pauseRun
	if run then
		want = 1
	elseif want <= 0 then
		return 0
	end
	local bar = L.barrier(s, peers)
	if not run and L.catchUp(s, peers, now) then
		-- catching up: batches of up to L.CATCHUP_SPEED steps, as long as one more such batch still lies before the barrier (the interface
		-- may see the game time of the batch before, see marginTarget)
		local boost = L.CATCHUP_SPEED
		if bar then boost = math.min(boost, math.floor((bar - now) / L.STEP) - L.CATCHUP_SPEED) end
		if boost > want then want = boost end
	end
	if bar then
		-- no shortened last step here (the pause's math.max(room, 1) below): one step too far is the late apply itself
		local room = math.floor((bar - now) / L.STEP)
		if room < 1 then return 0 end
		if room < want then want = room end
	end
	local stopAt = run and s.pauseRun or s.pauseAt
	if hold and (stopAt == nil or hold < stopAt) then stopAt = hold end
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
	local land = L.nextMark(marks, now)
	if land then
		local room = math.floor((land - now) / L.STEP)
		if room < want then want = math.max(room, 1) end
	end
	return want
end

-- The earliest mark after `now` (nil: none). Marks is a list of times.
function L.nextMark(marks, now)
	local best = nil
	for _, m in ipairs(marks or {}) do
		if type(m) == "number" and m > now and (best == nil or m < best) then best = m end
	end
	return best
end

-- Remembers a checksum stamp as a landing mark (a short list; marks already passed are dropped).
function L.addMark(marks, at, now)
	for i = #marks, 1, -1 do
		if marks[i] <= now then table.remove(marks, i) end
	end
	if type(at) == "number" and at > now and #marks < 16 then marks[#marks + 1] = at end
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

-- ------------------------------------------------------------------ the input barrier
--
-- A late apply (an item reaching a game after that game simulated its stamp) forks the entity ids (runs n1-n6, e1-e7: 9 of 10
-- first late applies did); the measured margin made it rare, never impossible. The barrier makes it impossible by construction:
--   * every `clock` a game sends carries `safe`, its PROMISE: every item it ships after that line is stamped at or after `safe`
--     (L.promise; L.clampStamp holds every stamp to it). A game's clocks and its items travel one path, in order (its
--     from_game.log, the launcher, one reliable connection to the host, the host's relay in the order it handles them, one
--     connection to each other game, its to_game.log): when a game reads a peer's clock it has every item that peer shipped
--     before it;
--   * so a game may simulate every step before the smallest promise of its peers, and no further (L.barrier, L.pace). The
--     receiver keeps marginTarget(speed) steps of that to itself: the interface sees the game time of its last frame (up to one
--     batch behind the simulation) and an item reaches the simulation one script event later.
-- The measured margin and the host's lead then only decide how often a game waits for a clock (latency), not correctness.
-- A peer that sends no promise (an older mod) is paced as before. A peer that leaves (player_left, away or gone) is removed with its
-- promise: the barrier is released by the session's own disconnect and timeout path.

-- The steps of its own a promise adds on top of the lead, so that a receiver (which keeps marginTarget of the promise to itself)
-- can still run up to the sender's clock plus the lead: with less, two games would wait for each other's clocks most of the time.
function L.barrierExtra(speed)
	return L.marginTarget(speed)
end

-- The promise this game reports with its clock at `now`, its new floor (the largest promise it made outside a pause; no stamp it
-- gives outside a pause may be earlier, see L.clampStamp), and whether the promise is a pause stop. Stopped at a pause, this game
-- stamps everything with the pause time and every game applies such an item when it is stopped there (L.applyNow), so it promises
-- the pause time itself, flagged: a peer may reach the pause exactly (it lands there anyway) and not pass it until this game's next
-- clock, which follows every item this game shipped during the pause, even when the host's resume overtakes such an item.
-- `minLead`: as in L.stampFor.
function L.promise(s, now, floor, minLead)
	if s.pauseAt and now >= s.pauseAt then
		return s.pauseAt, floor, true
	end
	local p = L.roundUpStep(now + L.leadOf(s, minLead)) + L.barrierExtra(s.speed) * L.STEP
	p = math.max(floor or p, p)
	return p, p, false
end

-- The stamp `at` of an item shipped at `now`, held to this game's promise. At a pause (this game stopped at it) the pause time stays.
function L.clampStamp(s, now, at, floor)
	if s.pauseAt and now >= s.pauseAt then return at end
	if type(floor) == "number" and at < floor then return floor end
	return at
end

-- The last game time this game may simulate: the smallest promise of its peers, less the steps the receiver keeps (see above); a
-- pause stop (pc.pause) exactly, as the pause itself. nil: no peer has promised anything (alone, or peers that send no promise).
function L.barrier(s, peers)
	local keep = L.marginTarget(s.speed) * L.STEP
	local b = nil
	for _, pc in pairs(peers) do
		if type(pc) == "table" and type(pc.safe) == "number" then
			-- a pause promise: up to the pause, or to the stamp the host gave an action of the pause past it (pauseRun: every item that
			-- game ships while paused is such an action, stamped by the host past every game)
			local lim = pc.pause and math.max(pc.safe, s.pauseRun or pc.safe) or (pc.safe - keep)
			if b == nil or lim < b then b = lim end
		end
	end
	return b
end

-- The peer whose promise sets the barrier (for the log), or nil.
function L.barrierPeer(peers)
	local who, b = nil, nil
	for id, pc in pairs(peers) do
		if type(pc) == "table" and type(pc.safe) == "number" and (b == nil or pc.safe < b) then who, b = id, pc.safe end
	end
	return who, b
end

-- ------------------------------------------------------------------ smooth pacing
--
-- Bug 4: over the internet a game ran at full speed into its stop (the input barrier, or the pacing limit `ahead` steps past the
-- slowest peer) and then stood there until a peer's next clock moved the stop on: in the friend tests of 2026-10-16/17 a median 14 s of
-- standstill a minute, single holds up to 16.6 s. Instead, when the batches this game has left before the stop would run out before the
-- stop is expected to move again, it asks the native module to stretch the engine's batch interval (the real time between two simulation
-- batches; native pace.rs) by a factor from 1 to L.PACE_MAX, so that those batches last until then: it runs a little slower and keeps
-- moving. Only real time changes: every game runs the same steps at the same game times and applies the same items at the same stamps
-- (the full-game rule: no mechanic is touched), and the hard stop of L.pace stays as the safety net. The idea of scaling the batch
-- interval is TPF3-MP's (Juliansgith, MIT) by way of MPFever 0.3.15 (MIT); the measure is our own, from what this game observes:
--   * the stop (L.paceStop): the nearer of the two stops a peer's clock moves, the barrier and the pacing limit. A pause, a slot hold or
--     a checksum mark are stops of the session itself and never slow anyone;
--   * when the stop moves next (L.paceObserve, L.paceNext): the real-time gaps between its last L.PACE_GAPS moves are measured (the
--     clock cadence of whichever peer sets it, as this game sees it after the whole path: the peer's frames, its link, the host's relay,
--     so the round trip's jitter is in it); the next move is expected L.PACE_GAP_PCT percent of those gaps after the last one. A stop
--     already later than that is expected within its jitter (the 90th less the 50th percentile) or as late again as it is;
--   * this game's batch time (L.paceBatch): the real time between its own batches at factor 1, measured (the median of the last ones,
--     with the factor it ran at taken out); L.STEP ms until it is;
--   * the factor (L.paceTarget): the batch that would leave less than a step of room (the game stands still at once then) is timed to
--     come when the stop moves on: the batches up to it are stretched by the factor that makes them last until then, at most
--     L.PACE_MAX. A room that lasts longer than the stop's next move: 1;
--   * L.paceSmooth: the factor follows its target over L.PACE_UP_S / L.PACE_DOWN_S of real time, so the speed changes gradually.
-- No stretch is asked for when the room outlasts the stop's next move, so a game with ample room (the one behind, a fast link) runs at 1,
-- and the factor follows the stop as it is now (it does not average a slower past into the future), so it does not feed itself.
-- Both ways since 2026-10-19 (L.paceHurry): a game that is behind the furthest peer by more than a batch, with room before its stop, runs
-- its batches sooner (a factor below 1, down to L.PACE_MIN, L.PACE_CATCHUP_MIN while it is stalled and catches up) until it is level
-- again. Before, time a game lost (a slow frame, a stand at its barrier) was never made up: the others slowed down to it for good. The
-- game ahead never hurries (it sees the others behind it), and a peer's clock is a trip old, so two games level do not take turns.
L.PACE_MAX = 2
L.PACE_MIN = 0.8          -- the most a game behind hurries (TPF3-MP's two-way trim goes 0.8-1.25)
L.PACE_CATCHUP_MIN = 0.5  -- the most a stalled game hurries while it catches up (L.catchUp; the native module's limit)
L.PACE_HURRY_S = 3        -- a game behind makes up the steps over about this much real time
L.PACE_GAPS = 32          -- the stop's last moves kept
L.PACE_GAP_PCT = 50       -- the stop is expected to move after this percentile of its gaps
L.PACE_BIAS = 0.9         -- the batches are stretched a little less than to the stop's expected move (see L.paceTarget)
L.PACE_WAIT_MAX_MS = 3000 -- a longer gap is an outage, not a pace: the hard stop handles it
L.PACE_BATCHES = 16       -- this game's last batch times kept
L.PACE_UP_S = 0.03     -- the factor of the next batch is needed right after this one: follows at once
L.PACE_DOWN_S = 0.3

function L.paceNew()
	return { trim = 1, target = 1, gaps = {}, batches = {}, stop = nil, movedAt = nil, at = nil }
end

-- The nearer of the stops a peer's clock moves (game time), or nil when there is none (alone, no promises, not running).
function L.paceStop(s, peers, ahead)
	if not s.started or (s.speed or 0) <= 0 then return nil end
	local stop = L.barrier(s, peers)
	local slowest = L.slowestPeer(peers)
	if slowest then
		local limit = slowest + (ahead or L.aheadFor(s.speed)) * L.STEP
		if stop == nil or limit < stop then stop = limit end
	end
	return stop
end

local function keepLast(list, v, n)
	list[#list + 1] = v
	if #list > n then table.remove(list, 1) end
end

-- percentile p (0..100) of a list, nil when empty
local function pctOf(list, p)
	local n = #list
	if n == 0 then return nil end
	local v = {}
	for i = 1, n do v[i] = list[i] end
	table.sort(v)
	return v[math.max(1, math.min(n, math.floor(p / 100 * (n - 1) + 0.5) + 1))]
end

-- The stop at real time `clock` (seconds): a move forward ends a gap, which is kept (a stop that went back, a peer that left or lowered
-- its promise at a pause, starts the count again without a gap). Frames that read the same real time are one moment.
function L.paceObserve(P, stop, clock)
	if stop == nil then
		P.stop, P.movedAt = nil, nil
		return
	end
	if P.stop == nil or stop < P.stop then
		P.stop, P.movedAt = stop, clock
		return
	end
	if stop > P.stop then
		if P.movedAt and clock > P.movedAt then keepLast(P.gaps, math.min((clock - P.movedAt) * 1000, L.PACE_WAIT_MAX_MS), L.PACE_GAPS) end
		P.stop, P.movedAt = stop, clock
	end
end

-- The value at percentile p of the stop's recent gaps (ms), nil before any.
function L.paceGap(P, p)
	return pctOf(P.gaps, p)
end

-- The stop's jitter (ms): the 90th less the 50th percentile of its gaps; 0 before four gaps were measured.
function L.paceJitter(P)
	if #P.gaps < 4 then return 0 end
	return math.max(0, L.paceGap(P, 90) - L.paceGap(P, 50))
end

-- Real time (ms) until the stop is expected to move again, at `clock`; nil before four gaps were measured.
function L.paceNext(P, clock)
	if #P.gaps < 4 or not P.movedAt then return nil end
	local due = (L.paceGap(P, L.PACE_GAP_PCT) or 0) - (clock - P.movedAt) * 1000
	if due > 0 then return due end
	-- later than usual already: expected within its jitter, or as late again as it is now (a stop that is late tends to stay late a
	-- while: a spike on the link, a peer's stalled frame), so the game slows down into the hold instead of stopping dead
	return math.min(L.PACE_WAIT_MAX_MS, math.max(L.paceJitter(P), -due))
end

-- This game's own clock at real time `clock`: the real time between two of its batches, with `applied` (the factor the module was
-- asked for meanwhile) taken out, is kept when the engine ran all along (`ran`: not held at speed 0 since the last batch).
function L.paceOwn(P, now, clock, ran, applied)
	if P.ownNow == nil or now < P.ownNow then
		P.ownNow, P.ownAt, P.ownRan = now, clock, true
		return
	end
	P.ownRan = P.ownRan and ran
	if now > P.ownNow then
		if P.ownRan and clock > P.ownAt then keepLast(P.batches, (clock - P.ownAt) * 1000 / math.max(1, applied or 1), L.PACE_BATCHES) end
		P.ownNow, P.ownAt, P.ownRan = now, clock, true
	end
end

-- This game's batch time at factor 1 (ms): the median of its last batches, L.STEP before four were measured.
function L.paceBatch(P)
	if #P.batches < 4 then return L.STEP end
	return pctOf(P.batches, 50)
end

-- The factor wanted: `room` game ms before the stop (nil: no stop) at session speed `speed`, the stop expected to move in `waitMs` (nil:
-- not known yet), a batch every `batchMs` at factor 1, this game's last batch `sinceMs` ago. The game stands still as soon as its room
-- is below a step (L.pace sets speed 0 at once, mid-batch), so the batch to time is the one that would leave it so: n = room / batch - 1
-- batches can still be taken safely, and the (n + 1)-th one empties the room. Stretched by f, it comes (n + 1) * batchMs * f after the
-- last one; it should come when the stop moves on (sinceMs + waitMs from the last one), a little before (L.PACE_BIAS) so that the
-- session never settles slower than its stops allow. Returns the factor (1 .. L.PACE_MAX) and the real time (ms) until the batch that
-- empties the room at factor 1.
function L.paceTarget(room, speed, waitMs, batchMs, sinceMs)
	if room == nil or waitMs == nil then return 1, nil end
	local per = math.max(1, math.min(4, speed or 1))
	local b = math.max(1, batchMs or L.STEP)
	if room < L.STEP then return L.PACE_MAX, 0 end   -- standing still already (L.pace): the batches after the release come stretched
	local n = math.max(0, room / (per * L.STEP) - 1)
	local since = math.max(0, math.min(sinceMs or 0, b))
	local left = (n + 1) * b - since
	local f = (since + waitMs * L.PACE_BIAS) / ((n + 1) * b)
	if f <= 1 then return 1, left end
	return math.min(L.PACE_MAX, f), left
end
-- The factor below 1 of a game `behind` steps behind the furthest peer that runs, with `room` steps before its stop (nil: no stop), at
-- session speed `speed`; `catching`: it is stalled and catches up (L.catchUp). 1 within a batch (a peer's clock is a trip old: a game
-- level with it sees it that far behind or ahead) or with less than three batches of room (the slow-down of L.paceTarget is for that).
-- Otherwise the steps beyond a batch are made up over L.PACE_HURRY_S of real time, the factor at least L.PACE_MIN (L.PACE_CATCHUP_MIN).
function L.paceHurry(behind, room, speed, catching)
	local per = math.max(1, math.min(4, speed or 1))
	if type(behind) ~= "number" or behind <= per then return 1 end
	if type(room) == "number" and room < 3 * per then return 1 end
	local f = 1 / (1 + (behind - per) / (L.PACE_HURRY_S * 6 * per))
	return math.max(catching and L.PACE_CATCHUP_MIN or L.PACE_MIN, math.min(1, f))
end

-- Moves the factor towards `target` over the real time since the last call (`clock`, seconds): towards a slower one quickly (the
-- batch after this one needs it), towards a faster one gently.
function L.paceSmooth(P, target, clock)
	local dt = P.at and math.max(0, clock - P.at) or 0
	P.at, P.target = clock, target
	local tau = target > P.trim and L.PACE_UP_S or L.PACE_DOWN_S
	local a = 1 - math.exp(-dt / tau)
	local trim = P.trim + (target - P.trim) * a
	if target == 1 and math.abs(trim - 1) < 0.01 then trim = 1 end
	P.trim = math.max(L.PACE_CATCHUP_MIN, math.min(L.PACE_MAX, trim))
	return P.trim
end
-- ------------------------------------------------------------------ measured stamp margin
--
-- A build every game applies at its stamp (the native copy's deferred flow) must reach every game before that game's simulation passes
-- the stamp: one applied late takes other entity ids from the engine's free-id FIFO and forces a resync. The stamp is the fastest known
-- clock plus the host's lead (lead_for: half the slowest round trip and handling, in steps) plus an EXTRA, in steps, for what neither
-- covers: the age of the fastest clock report (a peer kept running while its report travelled) and the hand-off into the receiver's
-- simulation (a frame batches `speed` steps at x2-x4). The extra is measured, not a constant (owner, 2026-10-14):
--   * every receiver measures, for each build it gets, its slack = (stamp - its own game time at arrival) / STEP, and from it the extra
--     that build would have needed to arrive exactly `marginTarget(speed)` steps early: sample = extra used + target - slack, and
--     wants that plus marginHeadroom(speed);
--   * its need grows AT ONCE to any larger want, and on a late apply by the lateness plus two steps; it shrinks only after
--     MARGIN_WINDOW builds and MARGIN_SHRINK_S real seconds below it, to the largest want of that window (as the host's adapt_lead:
--     longer at once, shorter after eight seconds);
--   * it reports the need with its clock (`need`, relayed by the host as peerclock.need); the stamping game uses the largest need
--     of its peers (a peer that has measured nothing at this speed counts as marginDefault(speed)).
-- Floor 0: the stamp is still at least the fastest clock plus the host's lead, the stamp rule of every other action.
L.MARGIN_WINDOW = 4
L.MARGIN_SHRINK_S = 8
L.MARGIN_MAX = 24

-- The slack (steps between arrival and stamp) a receiver is content with: a frame's batch of `speed` steps (the interface sees the
-- game time of its last frame), one step for the script event that hands the build to the simulation, one step of margin.
function L.marginTarget(speed)
	return math.max(1, math.min(4, speed or 1)) + 2
end

-- Steps kept on top of every sample, for a clock phase the window did not see: one step per speed unit. Real run m5 (x3, 100 ms one way):
-- with the extra shrunk to the window's largest sample (3), the slack of builds at the same extra swung from 5 to 8 steps with the
-- peer's position inside its pacing room, and the next build arrived with 3 steps and was applied 1 step late. The swing in steps
-- grows with the speed (more steps per frame and per network millisecond), so the headroom does too.
function L.marginHeadroom(speed)
	return math.max(1, math.min(4, speed or 1))
end

-- The extra before anything is measured at a speed: the pacing room a peer may run past the slowest clock, plus one step.
function L.marginDefault(speed)
	return L.aheadFor(speed) + 1
end

function L.newMargin()
	return { bySpeed = {} }
end

local function marginSlot(m, speed)
	local k = math.max(1, math.min(4, speed or 1))
	local s = m.bySpeed[k]
	if not s then
		s = { need = L.marginDefault(k), measured = false, window = {} }
		m.bySpeed[k] = s
	end
	return s
end

-- A build this game received: stamped `at` with `xs` extra steps, arrived at this game's time `gRecv`; `wall` = real seconds.
-- Returns the sample, the slack, the need now and what happened ("grow", "shrink", "keep").
function L.marginSample(m, speed, xs, at, gRecv, wall)
	local s = marginSlot(m, speed)
	local slack = math.floor((at - gRecv) / L.STEP)
	local sample = math.max(0, math.min(L.MARGIN_MAX, math.floor(xs) + L.marginTarget(speed) - slack))
	local want = math.min(L.MARGIN_MAX, sample + L.marginHeadroom(speed))
	s.measured = true
	if want > s.need then
		s.need, s.window = want, {}
		return sample, slack, s.need, "grow"
	end
	s.window[#s.window + 1] = { v = want, t = wall }
	if #s.window >= L.MARGIN_WINDOW and wall - s.window[1].t >= L.MARGIN_SHRINK_S then
		local mx = 0
		for _, w in ipairs(s.window) do if w.v > mx then mx = w.v end end
		s.window = {}
		if mx < s.need then
			s.need = mx
			return sample, slack, s.need, "shrink"
		end
	end
	return sample, slack, s.need, "keep"
end

-- This game applied an action `late` steps after its stamp: never again; the need grows at once and the window starts over.
function L.marginLate(m, speed, late)
	local s = marginSlot(m, speed)
	s.measured = true
	s.need = math.min(L.MARGIN_MAX, s.need + math.max(1, math.ceil(late or 1)) + 2)
	s.window = {}
	return s.need
end

-- What this game reports with its clock at this speed.
function L.marginNeed(m, speed)
	return marginSlot(m, speed).need
end

-- TEST-ONLY switches (never set by a player) to measure what the margin buys (owner, 2026-10-14): the values of
-- FEVERLINK3_TEST_NO_MARGIN and FEVERLINK3_TEST_NO_LEAD. NO_MARGIN "1": the extra is 0 (no measured need, no headroom, no growth after a
-- late apply); "extra:N": a fixed extra of N steps instead of the measured one. NO_LEAD "1" (stage 2): also the host's lead is gone, the
-- stamp is one step past the fastest known clock (the floor of leadOf), with the extra of NO_MARGIN (0 when that is unset).
-- nil: neither is set, nothing changes.
function L.testMargin(noMargin, noLead)
	local t = nil
	local v = type(noMargin) == "string" and noMargin:match("^%s*(.-)%s*$") or ""
	if v == "1" then
		t = { extra = 0 }
	else
		local n = tonumber(v:match("^extra:(%d+)$") or "")
		if n then t = { extra = math.min(L.MARGIN_MAX, n) } end
	end
	if type(noLead) == "string" and noLead:match("^%s*1%s*$") then
		t = t or { extra = 0 }
		t.noLead = true
	end
	return t
end

-- The extra steps a build stamped now gets: the largest need of the peers (0 with no peer: nobody to wait for). `test`
-- (L.testMargin) replaces the measured extra by its fixed one.
function L.marginExtra(peers, speed, test)
	if type(test) == "table" and next(peers) ~= nil then return test.extra end
	local x = nil
	for _, pc in pairs(peers) do
		if type(pc) == "table" then
			local n = tonumber(pc.need) or L.marginDefault(speed)
			if x == nil or n > x then x = n end
		end
	end
	return math.max(0, math.min(L.MARGIN_MAX, x or 0))
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
-- The same for an item stamped at the pause that just ended when this game still sits at that time: its sender shipped it during
-- the pause and the host's resume overtook it. The input barrier holds this game at the pause until that sender's next clock (see
-- L.promise), so the item is applied as during the pause, where every other game applied it.
function L.applyNow(s, now, item)
	if type(item.at) ~= "number" then return false end
	if s.pauseAt ~= nil then return now >= s.pauseAt and item.at <= now end
	return s.lastPauseAt ~= nil and item.at == s.lastPauseAt and now == item.at
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
		-- a handler's localFor(param): this parameter makes the event the sender's own setting (the Local time-of-day mode)
		if h and h.localFor then
			local ok, own = pcall(h.localFor, args[4])
			if ok and own then return "local" end
		end
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

-- Window commands name an entity their own window command created a moment ago by a placeholder key "ui:<id>" (<id> is
-- the record number the UI hook gave that command): the bridge's table of keys reaches the hook a second or so after the
-- creation (a vehicle bought and put on a line within that second was sent as "s:<id>", an id no other game shares; seen in
-- a real game). The bridge, which knows the sequence number each record was shipped as, replaces the placeholders before the
-- body leaves. seqOf(id) -> sequence number | nil. Returns true, or false and the id that was never shipped.
function L.rewriteUiKeys(node, seqOf, from)
	if type(node) ~= "table" then return true end
	if node.__ref == "entity" and type(node.key) == "string" then
		local id = node.key:match("^ui:(%d+)$")
		if id then
			local seq = seqOf(tonumber(id))
			if not seq then return false, tonumber(id) end
			node.key = L.itemKey(from, seq)
		end
	end
	for _, v in pairs(node) do
		if type(v) == "table" then
			local ok, bad = L.rewriteUiKeys(v, seqOf, from)
			if not ok then return false, bad end
		end
	end
	return true
end

-- An entity a command created is found a step or more after the command ran (a watch, see the bridge), so an action stamped
-- for the same step or the next that names it cannot resolve it yet: that is waiting, not an error. Returns the first key in
-- the body that isPending(key) names, or nil.
function L.pendingRef(node, isPending)
	if type(node) ~= "table" then return nil end
	if node.__ref == "entity" and type(node.key) == "string" and isPending(node.key) then return node.key end
	for _, v in pairs(node) do
		if type(v) == "table" then
			local k = L.pendingRef(v, isPending)
			if k then return k end
		end
	end
	return nil
end

-- Whether `key` is (or is under) the key of an action whose entities are still being looked for: watches
-- { key = ... }, binders { key = ... } (their sub-keys are "<key>#...").
function L.keyIsPending(key, watches, binders)
	for _, w in ipairs(watches or {}) do
		if w.key == key then return true end
	end
	for _, b in ipairs(binders or {}) do
		if b.key == key or key:sub(1, #b.key + 1) == b.key .. "#" then return true end
	end
	return false
end

-- What a game that could not replicate something (a native build, an action whose entities were not all found) tells the
-- others about its own divergence: notes = { { at = game time, text = ... }, ... } newest last. An action it ships carries
-- the recent ones (body.diverged = { "text", ... }), so a refusal on another game names the first divergence, not only the
-- reference that came out wrong.
L.NOTE_WINDOW = 600000   -- game time (ms) a note stays relevant to the actions that follow
L.NOTE_MAX = 3

function L.recentNotes(notes, now)
	local out = {}
	for _, n in ipairs(notes or {}) do
		if type(n) == "table" and type(n.text) == "string" and type(n.at) == "number" and now - n.at <= L.NOTE_WINDOW and n.at <= now then
			out[#out + 1] = "t=" .. n.at .. " " .. n.text
		end
	end
	while #out > L.NOTE_MAX do table.remove(out, 1) end
	return out
end

function L.addNote(notes, at, text, keep)
	notes[#notes + 1] = { at = at, text = text }
	while #notes > (keep or 8) do table.remove(notes, 1) end
end

-- A refusal reason with what the sender said about itself, when the reason is an unresolved or missing reference.
function L.refusalDetail(err, body)
	err = tostring(err)
	local d = type(body) == "table" and body.diverged or nil
	if type(d) == "table" and #d > 0 and (err:find("unresolved", 1, true) or err:find("does not exist", 1, true)) then
		local texts = {}
		for i, t in ipairs(d) do
			if type(t) == "string" and i <= L.NOTE_MAX then texts[#texts + 1] = t end
		end
		if #texts > 0 then err = err .. " [the sender had not replicated: " .. table.concat(texts, "; ") .. "]" end
	end
	return err
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
		-- `false` from a function: nothing to do after all (the step checked the world and found it already as wanted)
		if nextArgs ~= false then
			if type(nextArgs) ~= "table" then error("a further command of the action could not be made") end
			send(nextArgs)
		end
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
			if ok and nextArgs == false then return done(data, success, entities) end   -- nothing further to do
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
-- every game. This is only the fallback for an entity that appears after its command's step: the engine does NOT number
-- new entities in creation order (freed ids are handed out again first), so "the n-th command gets the n-th smallest id"
-- is wrong whenever an id is reused; what a command made at its own send is taken from its own snapshot (L.claimOwn).
-- `claimed` is a set of entities or a function(e) -> true for an entity that is not this watch's to take.
function L.claimEntity(before, after, claimed)
	local seen = {}
	for _, e in ipairs(before) do seen[e] = true end
	local taken = claimed
	if type(claimed) == "table" then taken = function(e) return claimed[e] end end
	local found = nil
	for _, e in ipairs(after) do
		if not seen[e] and not (taken and taken(e)) and (found == nil or e < found) then found = e end
	end
	return found
end

-- The entity a command created during its own send (`sent`: what appeared among the component's entities between the
-- snapshot before the send and the one right after it; the simulation state runs a command as it is sent) that still
-- exists (`after`) and is not taken: the smallest such, or nil. Exact whatever ids the engine hands out (run F 2026-10-18:
-- two buses bought in one window order, one stamp; the first watch took the smaller new id, which was the second bus's, and
-- the second watch then found nothing: "action 1:9: nothing created after 10 steps" on both games, three vehicles in the
-- depot afterwards for two purchases bound).
function L.claimOwn(sent, after, claimed)
	if type(sent) ~= "table" or #sent == 0 then return nil end
	local now = {}
	for _, e in ipairs(after) do now[e] = true end
	local found = nil
	for _, e in ipairs(sent) do
		if now[e] and not (claimed and claimed(e)) and (found == nil or e < found) then found = e end
	end
	return found
end

-- Which pending record (watches and binders: { key, sent }) made each entity at its own send: entity -> key. A watch or a
-- binder never takes an entity another command made at its send.
function L.sentOwners(watches, binders)
	local by = {}
	for _, w in ipairs(watches or {}) do
		for _, e in ipairs(type(w.sent) == "table" and w.sent or {}) do by[e] = by[e] or w.key end
	end
	for _, b in ipairs(binders or {}) do
		for _, list in pairs(type(b.sent) == "table" and b.sent or {}) do
			for _, e in ipairs(type(list) == "table" and list or {}) do by[e] = by[e] or b.key end
		end
	end
	return by
end

-- The latest binding of each entity: entity -> { key, at } (`at`: the game time it was bound, st.bindAt; nil for a binding
-- older than that record). A binding made before a watch's command ran names an entity that was gone by then (it is not in
-- the watch's snapshot) whose id the engine handed out again: it does not make the new entity taken (L.isTakenAt).
function L.bindOwners(bind, bindAt)
	local by = {}
	for k, e in pairs(bind or {}) do
		local at = type(bindAt) == "table" and bindAt[k] or nil
		local cur = by[e]
		if cur == nil or (at or -math.huge) > (cur.at or -math.huge) then by[e] = { key = k, at = at } end
	end
	return by
end

-- Whether owner record `o` (L.bindOwners) holds its entity for a record applied at `since` (nil: an older record, which
-- counts every binding as it did before bindAt existed).
function L.isTakenAt(o, since)
	if o == nil then return false end
	if since == nil then return true end
	return o.at ~= nil and o.at >= since
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

-- A command's name in the player's words, for the chat: "makeLineUpdateCmd" -> "line update".
function L.commandWords(name)
	local s = tostring(name):gsub("^make", ""):gsub("Cmd$", "")
	return (s:gsub("(%l)(%u)", "%1 %2"):lower())
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

-- Wider lanes (idea and field choices from MPFever (MIT), mpfever_refs.lua contentHash: stations, edges, edgeObjects,
-- nodeConfigs, vehicleState, names). Each lane is a small function over an optional engine accessor of the same name that
-- returns plain data; a build or test without the accessor simply has no such part. No entity id enters a hash (ids are
-- not guaranteed equal between games): positions rounded to 0.1, models and names stand in for ids, lists are sorted.
local function r1(v) return string.format("%.1f", v or 0) end

local function sortedItems(keys)
	table.sort(keys)
	return { n = #keys, items = keys }
end

-- stations: every station group with its station count, terminal count and the position of each terminal's stop node
local function stationsLane(engine)
	local keys = {}
	for _, g in ipairs(engine.stations()) do
		local at = {}
		for _, p in ipairs(g.at or {}) do at[#at + 1] = r1(p.x) .. "," .. r1(p.y) end
		table.sort(at)
		keys[#keys + 1] = tostring(g.stations or 0) .. "/" .. tostring(g.terminals or 0) .. "@" .. table.concat(at, ";")
	end
	return sortedItems(keys)
end

-- vehicles (a drift part): per line key (name and stop count) the number of vehicles and their sorted coarse states
-- (state, stopped by the player, to be sold); positions are left out on purpose
local function vehiclesLane(engine)
	local byLine = {}
	for _, v in ipairs(engine.vehicles()) do
		local k = tostring(v.line or "-")
		local t = byLine[k] or {}
		byLine[k] = t
		t[#t + 1] = tostring(v.state) .. "/" .. tostring(v.stopped) .. "/" .. tostring(v.sell)
	end
	local keys = {}
	for k, t in pairs(byLine) do
		table.sort(t)
		keys[#keys + 1] = k .. "=" .. #t .. ":" .. table.concat(t, ",")
	end
	return sortedItems(keys)
end

-- names of station groups, lines, towns and players (engine.names() -> { stations = {...}, lines = {...}, ... }), itemised one
-- "<kind>|<name>" per entity like the other lanes, so the detail of a full round (detail_*.txt at a resync) says which entity's
-- name differs (the friend test 2026-10-16 could only say "names")
local function namesLane(engine)
	local all, keys = engine.names(), {}
	for _, kind in ipairs({ "stations", "lines", "towns", "players" }) do
		for _, nm in ipairs(all[kind] or {}) do keys[#keys + 1] = kind .. "|" .. tostring(nm) end
	end
	return sortedItems(keys)
end

-- entity ids (a heavy lane, the only one that hashes ids): edges, constructions and town buildings, each sorted. The engine takes every new
-- entity's id from one FIFO of free ids, so the ids are equal on every game only while every build is applied at the same step on every
-- game (the stamp rule); any path that misses it shifts all later ids (native id trace, 2026-10-14) and a later build naming such an
-- entity would touch another one. The content lanes cannot see that; this lane makes it a difference the host repairs.
local function idsLane(all)
	local items = {}
	for _, kind in ipairs({ "edges", "constructions", "townBuildings" }) do
		local list = all[kind] or {}
		local s = {}
		for i = 1, #list do s[i] = tostring(list[i]) end
		items[#items + 1] = kind .. "#" .. #list .. ":" .. table.concat(s, ",")
	end
	return { n = #items, items = items }
end

-- ---- the heavy lanes, read a chunk of entities at a time (a spread round)
--
-- Real runs 2026-10-15/16 (friend test, airport run): a full round read about 6500 entities (1600 edges, 1600 constructions, 1700 town
-- buildings, their nodes) in 370-480 ms of ONE simulation step, 1.0 s at worst, 165-200 ms on the fastest PC: the game stood still about
-- once a minute. A full round is now spread (ChecksumRequestMessage.spread): at its stamp every game hashes the light lanes and the
-- ids lane and takes the entity lists of the heavy lanes; then, one chunk every `spread` steps after it, it reads the next CHECKSUM_CHUNK
-- entities of those lists. The chunks are cut by COUNT, never by time, and fall at the same steps on every game (each is a landing
-- mark, L.pace): every game reads every entity at the same simulation step, so games that agree give the same hashes and a world that
-- differs still shows. What a round sees is the world of each chunk's step (an entity built or grown after the stamp is not in the
-- lists; one removed reads as nothing), the same on every game. Each entity's key is hashed as it is read and the lane is the
-- order-free sum of those hashes (a multiset hash, mod 2^32): no sort and no rehash of the whole lane at the end.
L.CHECKSUM_CHUNK = 200       -- entities a step reads (about 80 us each on the slowest PC measured: ~16 ms; owner's target <= 16-30 ms)
L.CHECKSUM_MAX_CHUNKS = 48   -- a larger world reads more entities a chunk, so a round never lasts longer than this many chunks
L.CHECKSUM_FRAME_BUDGET_MS = 30  -- what one step of a spread round should cost at most: a chunk above it is counted in the round's log line
L.TOWN_PART_LIMIT = 48       -- `townBuildings:<town>` parts for the first this many towns (the host accepts at most 128 parts)

-- the key of one entity of a heavy lane: the same text from a list accessor (mocks) and from the one-entity readers (the game)
local CS = {}
function CS.construction(c) return string.format("%s@%.2f,%.2f,%.2f", tostring(c.file), c.x or 0, c.y or 0, c.z or 0) end
-- road and rail edges: endpoints (canonical order), street type, template/style, decorations and the models of the edge
-- objects (signals, stops, ...) in a canonical order
function CS.edge(e)
	local a, b = r1(e.x0) .. "," .. r1(e.y0), r1(e.x1) .. "," .. r1(e.y1)
	if a > b then a, b = b, a end
	local objs = {}
	for _, m in ipairs(e.objects or {}) do objs[#objs + 1] = tostring(m) end
	table.sort(objs)
	return a .. "-" .. b .. "/" .. tostring(e.type) .. "/" .. tostring(e.typeIndex) .. "/" .. tostring(e.roadType)
		.. "/" .. tostring(e.template) .. "/" .. tostring(e.style) .. "/d" .. tostring(e.decorations or 0)
		.. "/o" .. table.concat(objs, ",")
end
-- lane connections, crosswalks and traffic lights of the nodes (a drift part: see ChecksumPolicy::DRIFT_PARTS)
function CS.nodeConfig(n)
	return r1(n.x) .. "," .. r1(n.y) .. "/" .. tostring(n.lanes) .. "/" .. tostring(n.crosswalks)
		.. "/" .. tostring(n.preference) .. "/" .. tostring(n.light) .. "/" .. tostring(n.slip)
end
-- what the towns have grown (idea from MPFever (MIT): a `townBuildings` part): model, place, level, depth, parcels, time built
function CS.townBuilding(b)
	return string.format("%s@%.1f,%.1f,%.1f/L%s/D%s/P%s/T%s", tostring(b.file), b.x or 0, b.y or 0, b.z or 0, tostring(b.level),
		tostring(b.depth), tostring(b.parcels or 0), tostring(b.timeBuilt))
end

-- Each heavy lane: `has` (the engine can give it), `ids` (the list taken at the stamp: entity ids, or the records of a list accessor) and
-- `read` (one entry -> a record, nil when the entity is gone). Read in this order, one cursor across all of them.
local function streetIds(engine, ctx)
	if ctx.street == nil then ctx.street = engine.streetIds() or {} end
	return ctx.street
end
L.SPREAD_LANES = {
	{ name = "constructions", key = CS.construction,
		has = function() return true end,   -- always a part (an engine without any accessor reports "error", as it always did)
		ids = function(e, ctx)
			if e.construction then return ctx.ids and ctx.ids.constructions or e.constructionIds() end
			return e.constructions()
		end,
		read = function(e, x) if e.construction then return e.construction(x) end return x end },
	{ name = "edges", key = CS.edge,
		has = function(e) return (e.edge and e.streetIds) or e.edges end,
		ids = function(e, ctx) if e.edge and e.streetIds then return streetIds(e, ctx).edges or {} end return e.edges() end,
		read = function(e, x, ctx) if e.edge and e.streetIds then return e.edge(x, ctx) end return x end },
	{ name = "nodeConfigs", key = CS.nodeConfig,
		has = function(e) return (e.nodeConfig and e.streetIds) or e.nodeConfigs end,
		ids = function(e, ctx) if e.nodeConfig and e.streetIds then return streetIds(e, ctx).nodes or {} end return e.nodeConfigs() end,
		read = function(e, x) if e.nodeConfig and e.streetIds then return e.nodeConfig(x) end return x end },
	{ name = "townBuildings", key = CS.townBuilding, towns = true,
		has = function(e) return e.townBuilding end,
		ids = function(e, ctx) return ctx.ids and ctx.ids.townBuildings or e.townBuildingIds() end,
		read = function(e, x) return e.townBuilding(x) end },
}

function CS.readRange(r, ln, from, to)
	local spec, engine, ctx, keyHash = ln.spec, r.engine, r.ctx, r.keyHash
	for i = from, to do
		local v = spec.read(engine, ln.ids[i], ctx)
		if v ~= nil then
			local key = spec.key(v)
			local h = keyHash(key)
			ln.n, ln.sum = ln.n + 1, (ln.sum + h) % 4294967296
			if spec.towns then
				local town = v.town or -1
				local t = ln.towns[town]
				if not t then t = { n = 0, sum = 0 }; ln.towns[town] = t end
				t.n, t.sum = t.n + 1, (t.sum + h) % 4294967296
				if ln.keys then ln.keys[#ln.keys + 1] = tostring(town) .. "|" .. key end
			elseif ln.keys then
				ln.keys[#ln.keys + 1] = key
			end
		end
	end
end

function CS.finishLane(r, ln)
	local name, parts, hash = ln.spec.name, r.parts, r.hash
	if ln.failed then
		parts[name] = "error"
		r.errors[name] = ln.failed
		return
	end
	if ln.spec.towns then
		-- `townBuildings` = "<total>/<hash of all towns>", `townBuildings:<town id>` = "<count>/<hash>" for the first TOWN_PART_LIMIT towns
		-- (the town entity id only names the part: towns come from the savegame, so it agrees)
		local towns, all = {}, {}
		for town in pairs(ln.towns) do towns[#towns + 1] = town end
		table.sort(towns)
		for i, town in ipairs(towns) do
			local t = ln.towns[town]
			all[i] = { town = town, n = t.n, sum = t.sum }
			if i <= L.TOWN_PART_LIMIT then parts[name .. ":" .. tostring(town)] = t.n .. "/" .. hash({ n = t.n, sum = t.sum }) end
		end
		parts[name] = ln.n .. "/" .. hash(all)
	else
		parts[name] = hash({ n = ln.n, sum = ln.sum })
	end
	if r.detail and ln.keys then r.detail[name] = ln.keys end
end

-- Parts over an engine accessor:
--   engine.gameTime()       -> number
--   engine.account()        -> balance, loan
--   engine.lines()          -> list of { stops = number }
--   heavy lanes (L.SPREAD_LANES): engine.constructionIds() + engine.construction(e) -> { file, x, y, z }, or engine.constructions() -> list of
--   those; engine.streetIds() -> { edges = {...}, nodes = {...} } + engine.edge(e, ctx) / engine.nodeConfig(n), or engine.edges() /
--   engine.nodeConfigs(); engine.townBuildingIds() + engine.townBuilding(e) -> { town, file, x, y, z, level, depth, parcels, timeBuilt }
-- Each part is hashed separately so the launcher can name the differing one. Entity ids enter only the `ids` lane:
-- constructions are compared by file and position, lines by their stop counts, both sorted.
-- `light` (true): a light round, the heavy lanes are left out (L.HEAVY_LANES; the host asks for a full round every Nth time,
-- see ChecksumRequestMessage.full). Each of those costs one component read per edge, node, construction or town building.
-- opts: detail (a table: filled with the itemised keys of every lane), clock (seconds; observation only, never part of the result),
-- keyHash (key -> 32-bit number; default: the hex of hashFn), chunk (entities a step reads), maxChunks.
-- What it does now: the time, the light lanes, the ids lane and the entity lists of the heavy lanes. L.checksumRoundStep reads the rest.
L.HEAVY_LANES = { constructions = true, edges = true, nodeConfigs = true, townBuildings = true, ids = true }
-- a round's own state, without the work: the engine, the hash functions, the clock (opts as L.newChecksumRound)
local function roundShell(engine, hashFn, light, opts)
	return {
		engine = engine, hash = hashFn, light = light == true, parts = {}, timing = {}, detail = opts.detail, errors = {}, lanes = {},
		li = 1, pos = 1, total = 0, ctx = {}, clock = opts.clock or function() return 0 end, steps = 0, frameMax = 0, frameLanes = {},
		keyHash = opts.keyHash or function(k) return tonumber(hashFn(k), 16) or 0 end,
	}
end
function L.newChecksumRound(engine, hashFn, light, opts)
	opts = opts or {}
	local r = roundShell(engine, hashFn, light, opts)
	local clock = r.clock
	local parts, timing, detail = r.parts, r.timing, r.detail
	local t0 = clock()
	local function part(name, fn)
		if r.light and L.HEAVY_LANES[name] then return end
		local tp = clock()
		local ok, v = pcall(fn)
		parts[name] = ok and hashFn(v) or "error"
		if ok and detail and type(v) == "table" and type(v.items) == "table" then detail[name] = v.items end
		timing[name] = clock() - tp
	end
	local okT, t = pcall(engine.gameTime)
	r.time = okT and t or -1
	parts.time = hashFn(r.time)
	part("balance", function() local balance = engine.account(); return balance end)
	part("loan", function() local _, loan = engine.account(); return loan end)
	part("lines", function()
		local list = engine.lines()
		local keys = {}
		for _, l in ipairs(list) do keys[#keys + 1] = tostring(l.stops or 0) end
		table.sort(keys)
		return { n = #list, items = keys }
	end)
	-- wider lanes: each is skipped when the engine has no accessor of that name
	local function lane(name, fn) if engine[name] then part(name, function() return fn(engine) end) end end
	lane("stations", stationsLane)
	lane("vehicles", vehiclesLane)
	lane("names", namesLane)
	if engine.entityIds then part("ids", function() local all = engine.entityIds(); r.ctx.ids = all; return idsLane(all) end) end
	if not r.light then
		for _, spec in ipairs(L.SPREAD_LANES) do
			if spec.has(engine) then
				local tl = clock()
				local ok, ids = pcall(spec.ids, engine, r.ctx)
				local ln = { spec = spec, ids = (ok and type(ids) == "table") and ids or {}, n = 0, sum = 0, towns = {}, keys = detail and {} or nil }
				if not ok then ln.failed = tostring(ids) end
				r.lanes[#r.lanes + 1] = ln
				r.total = r.total + #ln.ids
				timing[spec.name] = clock() - tl
			end
		end
	end
	r.chunk = math.max(opts.chunk or L.CHECKSUM_CHUNK, math.ceil(r.total / (opts.maxChunks or L.CHECKSUM_MAX_CHUNKS)))
	r.chunks = math.ceil(r.total / r.chunk)   -- the steps after the stamp that read entities (0: nothing to read)
	r.startCost = clock() - t0
	r.done = #r.lanes == 0
	return r
end

-- Reads the next chunk (r.chunk entities, across the lanes in order) and finishes every lane it completes. True once every lane is done:
-- r.parts is then final. r.frame is what this call cost (seconds, by lane); r.frameMax / r.frameLanes the costliest call so far.
function L.checksumRoundStep(r)
	local budget, clock, frame, total = r.chunk, r.clock, {}, 0
	local read = 0   -- the entities this call reads (the cost per entity the host sizes the chunks with: L.checksumCost)
	while r.li <= #r.lanes do
		local ln = r.lanes[r.li]
		local count = #ln.ids
		if budget <= 0 and r.pos <= count then break end
		local last = math.min(count, r.pos + budget - 1)
		local t0 = clock()
		if not ln.failed and last >= r.pos then
			local ok, err = pcall(CS.readRange, r, ln, r.pos, last)
			if not ok then ln.failed = tostring(err) end
		end
		if last >= r.pos then budget, read = budget - (last - r.pos + 1), read + (last - r.pos + 1) end
		if last >= count then
			CS.finishLane(r, ln)
			r.li, r.pos = r.li + 1, 1
		else
			r.pos = last + 1
		end
		local dt = clock() - t0
		local name = ln.spec.name
		frame[name] = (frame[name] or 0) + dt
		r.timing[name] = (r.timing[name] or 0) + dt
		total = total + dt
	end
	r.steps = r.steps + 1
	r.frame = frame
	if total > r.frameMax then r.frameMax, r.frameLanes, r.frameMaxN = total, frame, read end
	r.done = r.li > #r.lanes
	return r.done
end

-- What a finished spread round cost on this game, for the host to size the next rounds' chunks (core ChecksumPolicy::chunk_for):
-- `entityNs` the dearer of the costliest chunk's and the whole round's time per entity, `stepMaxUs` the costliest chunk, `chunk`
-- the entities a chunk read. nil when there is nothing to measure (no chunk of 20 entities or more, or a clock that never moved).
L.COST_MIN_ENTITIES = 20
function L.checksumCost(r)
	local n, work = r.frameMaxN or 0, r.work or 0
	if n < L.COST_MIN_ENTITIES or (r.frameMax or 0) <= 0 then return nil end
	local per = r.frameMax / n
	if (r.total or 0) > 0 and work / r.total > per then per = work / r.total end
	return { entityNs = math.floor(per * 1e9 + 0.5), stepMaxUs = math.floor(r.frameMax * 1e6 + 0.5), chunk = r.chunk }
end

-- The whole round at once (a light round, a paused one, a request without `spread`): parts and the time they were taken at.
-- `timing` (optional table): filled with the seconds each part took (observation only, never part of the result).
function L.checksumParts(engine, hashFn, timing, light, detail, keyHash)
	local clock = timing and function() local ok, v = pcall(L.clock); return ok and tonumber(v) or 0 end or nil
	local r = L.newChecksumRound(engine, hashFn, light, { detail = detail, clock = clock, keyHash = keyHash, chunk = math.huge })
	while not r.done do L.checksumRoundStep(r) end
	if timing then for k, v in pairs(r.timing) do timing[k] = v end end
	return r.parts, r.time
end

-- A `checksum_request` payload {n, at, paused, full?, spread?, chunk?, detail?} as a queue item. `chunk`: the entities a chunk of a
-- spread round reads, the same number on every game (the host sizes it from the games' measured cost; absent or out of range: the
-- game's own L.CHECKSUM_CHUNK). `detail`: the round keeps its itemised keys in the game-script state (the host asks for it after a
-- round that differed), so the dump at a resync is complete and the same round on every game.
L.CHECKSUM_CHUNK_MAX = 20000
function L.checksumItem(p)
	if type(p) ~= "table" or type(p.n) ~= "number" or type(p.at) ~= "number" then return nil end
	local spread = math.floor(tonumber(p.spread) or 0)
	if spread < 0 or spread > 16 then spread = 0 end
	local chunk = math.floor(tonumber(p.chunk) or 0)
	if chunk < 1 or chunk > L.CHECKSUM_CHUNK_MAX then chunk = nil end
	return { kind = "checksum", at = p.at, n = p.n, paused = p.paused == true, full = p.full ~= false, spread = spread, chunk = chunk,
		detail = p.detail == true }
end

-- The steps at which a spread round reads its chunks (each a landing mark, so every game reads it at exactly that step): the
-- CHECKSUM_MAX_CHUNKS steps `spread` apart after the stamp (a game knows how many it needs only at the stamp; the marks of the chunks a
-- round did not need are dropped when its report is sent). Empty for a round hashed whole.
function L.checksumMarks(it)
	local out = {}
	if type(it) == "table" and it.full and not it.paused and (it.spread or 0) > 0 then
		for k = 1, L.CHECKSUM_MAX_CHUNKS do out[k] = it.at + k * it.spread * L.STEP end
	end
	return out
end

-- The step at which chunk k (1-based) of round r is read: `spread` steps apart after the step the round started at.
function L.checksumChunkAt(r, k)
	return r.start + k * r.spread * L.STEP
end

-- ---- a spread round shared by the simulation Lua states
--
-- Real game 2026-10-18 (owner's live test of 0.2.0-pre5, and every lab run of that day since the rounds are spread): every spread round
-- logged "37 chunk(s) read after their step (+144400 ms game time)". The game runs the game scripts in more than one simulation Lua
-- state, and the steps of one game are shared between them: two states installed the session's math.random at the same t=12998600, and
-- round 24 (begun at t=13366600, one chunk a step) read its chunks only at t=13367200, 13367400, 13368000, 13368200, ..., two steps in
-- four, while action 0:2 was applied at its stamp t=13367800 in between, by the other state. A round kept in one Lua state's own module
-- table is read only on that state's steps. So its progress lives in the game-script state, which every simulation state reads and
-- writes (the stamped queue lives there for the same reason): L.checksumRoundSave writes it after a chunk, L.checksumRoundSync reads it
-- into a state's own copy before the next, and L.checksumRoundRestore makes that copy in a state that has none. The entity lists taken
-- at the stamp travel once, packed as text (L.packIds); each state unpacks them once a round.
local ROUND_FIELDS = { "time", "light", "li", "pos", "total", "chunk", "chunks", "steps", "done", "frameMax", "frameMaxN", "startCost" }

local function flatCopy(t)
	local out = {}
	for k, v in pairs(t or {}) do out[k] = v end
	return out
end

-- An entity list as text ("12,13,40"): one string in the game-script state instead of thousands of entries. The records of a list
-- accessor (the test engines) are kept as a list.
function L.packIds(list)
	local s = {}
	for i = 1, #list do
		local v = list[i]
		if type(v) ~= "number" or v ~= math.floor(v) or math.abs(v) >= 1e15 then return flatCopy(list) end
		s[i] = tostring(v)
	end
	return table.concat(s, ",")
end

function L.unpackIds(p)
	if type(p) == "table" then return p end
	local out = {}
	if type(p) == "string" then
		for w in p:gmatch("[^,]+") do out[#out + 1] = tonumber(w) end
	end
	return out
end

-- The progress of round r as plain data (into `rec`, or a new table): the counters, the parts so far, each lane's sums and towns.
-- Town sums go as a list ({ town, n, sum }), never as a table keyed by entity id.
function L.checksumRoundSave(r, rec)
	rec = rec or {}
	for _, k in ipairs(ROUND_FIELDS) do rec[k] = r[k] end
	rec.parts, rec.errors, rec.timing, rec.frameLanes = flatCopy(r.parts), flatCopy(r.errors), flatCopy(r.timing), flatCopy(r.frameLanes)
	rec.lanes = type(rec.lanes) == "table" and rec.lanes or {}
	for i, ln in ipairs(r.lanes) do
		local s = rec.lanes[i] or {}
		if s.ids == nil then s.ids = L.packIds(ln.ids) end
		s.name, s.n, s.sum, s.failed = ln.spec.name, ln.n, ln.sum, ln.failed
		local towns = {}
		for town, t in pairs(ln.towns) do towns[#towns + 1] = { town = town, n = t.n, sum = t.sum } end
		table.sort(towns, function(a, b) return tostring(a.town) < tostring(b.town) end)
		s.towns = towns
		rec.lanes[i] = s
	end
	return rec
end

-- Round r takes the progress `rec` holds (another Lua state may have read chunks since this one last did). The per-round cache of the
-- entity readers (r.ctx: node positions) starts empty at every step, so a value is never one this state read at an earlier step.
function L.checksumRoundSync(r, rec)
	for _, k in ipairs(ROUND_FIELDS) do r[k] = rec[k] end
	r.parts, r.errors, r.timing, r.frameLanes = flatCopy(rec.parts), flatCopy(rec.errors), flatCopy(rec.timing), flatCopy(rec.frameLanes)
	r.ctx = {}
	for i, s in ipairs(rec.lanes or {}) do
		local ln = r.lanes[i]
		if ln then
			ln.n, ln.sum, ln.failed = s.n or 0, s.sum or 0, s.failed
			ln.towns = {}
			for _, t in ipairs(s.towns or {}) do ln.towns[t.town] = { n = t.n, sum = t.sum } end
		end
	end
	return r
end

-- A Lua state's own copy of a round it did not begin (opts as L.newChecksumRound: detail, clock, keyHash).
function L.checksumRoundRestore(engine, hashFn, rec, opts)
	opts = opts or {}
	local r = roundShell(engine, hashFn, rec.light, opts)
	local spec = {}
	for _, s in ipairs(L.SPREAD_LANES) do spec[s.name] = s end
	for i, s in ipairs(rec.lanes or {}) do
		if not spec[s.name] then error("checksum round restore: unknown lane " .. tostring(s.name)) end
		r.lanes[i] = { spec = spec[s.name], ids = L.unpackIds(s.ids), n = 0, sum = 0, towns = {}, keys = opts.detail and {} or nil }
	end
	return L.checksumRoundSync(r, rec)
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
