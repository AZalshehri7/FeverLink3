-- Registers the in-game window (feverlink3_ui.lua) on the game's mod entry point, which is mounted once for the
-- whole UI session (format and finding from MPFever (MIT): mpfever_entry.res.lua). Untested in a game.
function data()
	return {
		type = "react-plugin ::ModEntryPointExtension",
		data = {
			filePath = "feverlink3_1::/feverlink3_ui@FeverLink3Entry",
			order = 1000,
		},
	}
end
