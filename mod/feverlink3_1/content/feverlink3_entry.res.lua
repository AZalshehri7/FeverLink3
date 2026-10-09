-- Puts the FeverLink3 button in the game bar (the bottom bar's info display), which opens the window of
-- feverlink3_ui.script.lua. The plugin format is the one of the game's own bar plugins
-- (gui/game_bar/game_bar_display_transported_plugin); the earlier ModEntryPointExtension is internal and hidden.
function data()
	return {
		type = "react-plugin ::GameBarInfoDisplayExtension",
		data = {
			filePath = "feverlink3_1::/feverlink3_ui.script@FeverLink3Bar",
			priority = -2, -- negative: left of the default entries
		},
	}
end
