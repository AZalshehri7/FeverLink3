-- FeverLink3 game-script registration.
--
-- A Transport Fever 3 game script declares its four entry points here; each `fileName` is "<file>@<function>"
-- without the .lua suffix, and the file's data() returns the function table (finding from MPFever (MIT):
-- mpfever.gs.lua). update/handleEvent run in the simulation Lua state, once per simulation step;
-- guiUpdate/guiHandleEvent run in the interface Lua state, once per frame.
function data()
	return {
		updateScript = {
			fileName = "feverlink3_bridge.script@update",
		},
		handleEventScript = {
			fileName = "feverlink3_bridge.script@handleEvent",
		},
		guiUpdateScript = {
			fileName = "feverlink3_bridge.script@guiUpdate",
		},
		guiHandleEventScript = {
			fileName = "feverlink3_bridge.script@guiHandleEvent",
		},
	}
end
