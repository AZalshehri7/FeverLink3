-- FeverLink3 game-script registration.
--
-- A Transport Fever 3 game script declares its entry points here; each `fileName` is "<file>@<function>"
-- without the .lua suffix, and the file's data() returns the function table (finding from MPFever (MIT):
-- mpfever.gs.lua). update/postUpdate/handleEvent run in the simulation Lua state, update and postUpdate once per
-- simulation step: update "may not write the state itself" and postUpdate "is executed after all the update scripts
-- are done" (official wiki, modding:scripting:gamescripts), so the bridge's update only reads and its postUpdate
-- applies the step's stamped actions. guiUpdate/guiHandleEvent run in the interface Lua state, once per frame.
function data()
	return {
		updateScript = {
			fileName = "feverlink3_bridge.script@update",
		},
		postUpdateScript = {
			fileName = "feverlink3_bridge.script@postUpdate",
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
