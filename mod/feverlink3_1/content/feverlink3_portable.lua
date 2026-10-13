-- The portable text of a build command, as a table (B2).
--
-- The native module writes the held build command as lines `path TAB value` (native/src/portable.rs; for the mod the input level:
-- the members of the layout table only, native/src/input.rs). This module reads that text. Pure: no engine calls, so it can run
-- anywhere and is tested with real texts.
--
--   u8:xx u32:xxxxxxxx i32:xxxxxxxx f32:xxxxxxxx   hex bit patterns -> numbers (i32 signed, f32 decoded from its bits)
--   v3:a,b,c  m4:16 words                           f32 bit patterns -> {x,y,z} / an array of 16 numbers
--   ent:new#N                                       -> { new = N }       an entity the proposal creates (placeholder -N)
--   ent:old#ID[@bx,by,bz]                           -> { old = ID, pos = {x,y,z} }   an existing entity (pos when the proposal locates it)
--   str:"..." res:"..."                             -> a string (\" \\ \xNN escapes undone)
--   n:K                                             -> K
--   hash:... map:... raw:...                        -> not data of the input: skipped
--
-- A path is names joined by dots, with [i] (0-based) for the element of a vector: `proposal.addedNodes[0].position`. The result is a
-- tree: `tree.proposal.addedNodes[1].position`. A vector's table has the count in `.n` (from `path.#`) and its elements at 1..n; a
-- vector of entity ids has the ids at 1..n. `tree.kind`, `tree.ignoreErrors`... are the top-level members.

local M = {}

-- the f32 whose bit pattern is `bits` (an integer 0 .. 2^32-1)
function M.float(bits)
	local sign = (bits >= 0x80000000) and -1 or 1
	local e = math.floor(bits / 0x800000) % 256
	local m = bits % 0x800000
	if e == 255 then
		if m ~= 0 then return 0 / 0 end
		return sign * math.huge
	end
	if e == 0 then return sign * m * 2 ^ -149 end
	return sign * (1 + m / 0x800000) * 2 ^ (e - 127)
end

local function hex(s) return tonumber(s, 16) end

local function vec(words)
	local out, n = {}, 0
	for w in words:gmatch("[^,]+") do
		n = n + 1
		out[n] = M.float(hex(w))
	end
	return out
end

local function unescape(s)
	-- s is the quoted text: "..."
	local body, out, i = s:sub(2, -2), {}, 1
	while i <= #body do
		local c = body:sub(i, i)
		if c == "\\" then
			local h = body:match("^x(%x%x)", i + 1)
			if h then
				out[#out + 1] = string.char(hex(h))
				i = i + 4
			else
				out[#out + 1] = body:sub(i + 1, i + 1)
				i = i + 2
			end
		else
			out[#out + 1] = c
			i = i + 1
		end
	end
	return table.concat(out)
end

-- A value text as a Lua value; nil for the kinds that carry no input (hash, map, raw, unknown).
function M.value(v)
	local kind, rest = v:match("^(%a+%d*):(.*)$")
	if kind == "u8" or kind == "u32" then return hex(rest) end
	if kind == "i32" then
		local n = hex(rest)
		if n >= 0x80000000 then n = n - 0x100000000 end
		return n
	end
	if kind == "f32" then return M.float(hex(rest)) end
	if kind == "v3" then
		local t = vec(rest)
		return { x = t[1], y = t[2], z = t[3] }
	end
	if kind == "m4" then return vec(rest) end
	if kind == "n" then return tonumber(rest) end
	if kind == "str" or kind == "res" then return unescape(rest) end
	if kind == "ent" then
		local new = rest:match("^new#(%d+)$")
		if new then return { new = tonumber(new) } end
		local old, pos = rest:match("^old#(%d+)@?(.*)$")
		if old then
			local e = { old = tonumber(old) }
			if pos ~= "" then
				local t = vec(pos)
				e.pos = { x = t[1], y = t[2], z = t[3] }
			end
			return e
		end
	end
	return nil
end

-- "a.b[3].c" -> { "a", "b", 4, "c" } (indices become 1-based numbers); "#" stays a name
local function steps(path)
	local out = {}
	for name, idx in path:gmatch("([^%.%[%]]+)([%[%]%d]*)") do
		out[#out + 1] = name
		for i in idx:gmatch("%[(%d+)%]") do out[#out + 1] = tonumber(i) + 1 end
	end
	return out
end

-- Parses the text. Returns the tree and a header table { line = "=== portable ..." } or nil and why. Lines that carry no input are
-- skipped; a line that cannot be read makes the whole text unusable (nothing is guessed).
function M.parse(text)
	if type(text) ~= "string" or text == "" then return nil, "no text" end
	local tree, header, count = {}, nil, 0
	for line in text:gmatch("[^\r\n]+") do
		if line:sub(1, 4) == "=== " then
			if not header and line:find("portable", 1, true) then header = line end
		elseif line:sub(1, 2) ~= "@B" then
			local path, value = line:match("^([^\t]+)\t([^\t]*)")
			if not path then return nil, "bad line: " .. line:sub(1, 80) end
			local ok, v = pcall(M.value, value)
			if not ok then return nil, "bad value at " .. path .. ": " .. value:sub(1, 60) end
			if v ~= nil and not path:find(".~", 1, true) and path:sub(1, 1) ~= "~" then
				local st = steps(path)
				local node = tree
				local last = #st
				local isCount = st[last] == "#"
				if isCount then last = last - 1 end
				for i = 1, last - 1 do
					local k = st[i]
					if node[k] == nil then node[k] = {} end
					node = node[k]
					if type(node) ~= "table" then return nil, "path through a value: " .. path end
				end
				local k = st[last]
				if isCount then
					if node[k] == nil then node[k] = {} end
					node[k].n = v
				else
					-- a member that is a table already (a vector or record seen first through its elements) keeps its fields
					node[k] = v
				end
				count = count + 1
			end
		end
	end
	if count == 0 then return nil, "no members" end
	return tree, { line = header, members = count }
end

local function n(t) return type(t) == "table" and t.n or 0 end

-- The input counted the way the build record is counted (recCounts), for the shadow comparison.
function M.counts(tree)
	local p = tree.proposal or {}
	return {
		nodes = n(p.addedNodes), segs = n(p.addedSegments), rmNodes = n(p.removedNodes), rmSegs = n(p.removedSegments),
		ncfg = n(p.nodeConfigsToAdd), rmNcfg = n(p.nodeConfigsToRemove), objs = n(p.edgeObjectsToAdd), rmObjs = n(p.edgeObjectsToRemove),
		cons = n(tree.toAdd), rem = n(tree.toRemove), frozen = n(p.frozenNodes),
	}
end

function M.countsText(c)
	return string.format("cons=%d rem=%d nodes=%d segs=%d rmNodes=%d rmSegs=%d objs=%d rmObjs=%d ncfg=%d rmNcfg=%d frozen=%d", c.cons, c.rem, c.nodes, c.segs,
		c.rmNodes, c.rmSegs, c.objs, c.rmObjs, c.ncfg, c.rmNcfg, c.frozen)
end

-- ---- shadow comparison with the build record (B2.1)
-- What the record path carries against what the native module read from the held command, member by member. Observation only: it
-- says where the record is lossy (a construction's own street pieces have no geometry in it, the lane configurations are not in the
-- text), it changes nothing. Returns a list of difference lines (empty = the record has everything the text names).

local function near(a, b) return type(a) == "number" and type(b) == "number" and math.abs(a - b) < 0.02 end
local function nearPos(p, q) return type(p) == "table" and type(q) == "table" and near(p.x, q.x) and near(p.y, q.y) and near(p.z, q.z) end

function M.compare(tree, rec)
	local d = {}
	local function diff(fmt, ...) d[#d + 1] = string.format(fmt, ...) end
	local c = M.counts(tree)
	local rc = { nodes = #rec.nodes, segs = #rec.segs, rmNodes = #rec.rmNodes, rmSegs = #rec.rmSegs, ncfg = #rec.ncfg, rmNcfg = #rec.rmNcfg,
		objs = #rec.objs, rmObjs = #rec.rmObjs, cons = #rec.cons, rem = #rec.rem }
	for _, k in ipairs({ "cons", "rem", "nodes", "segs", "rmNodes", "rmSegs", "objs", "rmObjs", "ncfg", "rmNcfg" }) do
		if c[k] ~= rc[k] then diff("count %s: text %d, record %d", k, c[k], rc[k]) end
	end
	local p = tree.proposal or {}
	for i = 1, math.min(c.nodes, rc.nodes) do
		if not nearPos(p.addedNodes[i].position, rec.nodes[i].pos) then diff("node %d: position differs", i) end
	end
	local noGeometry = 0
	for i = 1, math.min(c.segs, rc.segs) do
		local ts, rs = p.addedSegments[i], rec.segs[i]
		local rcmp = rs.c or {}
		if ts.roadTemplate ~= rcmp.roadTemplate then diff("segment %d: template text %s, record %s", i, tostring(ts.roadTemplate), tostring(rcmp.roadTemplate)) end
		if ts.roadStyle ~= rcmp.roadStyle then diff("segment %d: style text %s, record %s", i, tostring(ts.roadStyle), tostring(rcmp.roadStyle)) end
		if rcmp.typeIndex ~= nil and ts.typeIndex ~= rcmp.typeIndex then diff("segment %d: typeIndex text %s, record %s", i, tostring(ts.typeIndex), tostring(rcmp.typeIndex)) end
		if rcmp.roadType ~= nil and ts.roadType ~= rcmp.roadType then diff("segment %d: roadType text %s, record %s", i, tostring(ts.roadType), tostring(rcmp.roadType)) end
		if rcmp.p0 then
			if not (nearPos(ts.position0, rcmp.p0) and nearPos(ts.position1, rcmp.p1)) then diff("segment %d: end positions differ", i) end
			if rcmp.t0 and not (nearPos(ts.tangent0, rcmp.t0) and nearPos(ts.tangent1, rcmp.t1)) then diff("segment %d: tangents differ", i) end
		else
			noGeometry = noGeometry + 1
		end
	end
	if noGeometry > 0 then diff("segments without geometry in the record: %d of %d (the text has positions, tangents and lane bytes for all)", noGeometry, c.segs) end
	for i = 1, math.min(c.cons, rc.cons) do
		local tc, rcn = tree.toAdd[i], rec.cons[i]
		if tc.fileName ~= rcn.file then diff("construction %d: file text %s, record %s", i, tostring(tc.fileName), tostring(rcn.file)) end
		local m = rcn.transf
		if type(m) == "table" and type(tc.transf) == "table" and not (near(tc.transf[13], m[13]) and near(tc.transf[14], m[14]) and near(tc.transf[15], m[15])) then
			diff("construction %d: placement differs", i)
		end
	end
	if (c.frozen or 0) > 0 then diff("frozenNodes in the text: %d (the record has no such member)", c.frozen) end
	local tags = p.segmentTags and p.segmentTags.bytes or 0
	if tags > 0 then diff("segmentTags in the text: %d bytes (the record has no such member)", tags) end
	return d
end

return M
