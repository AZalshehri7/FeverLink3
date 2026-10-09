-- FeverLink3 command schemas: operations (vehicles, lines, depots, finance, towns and industries).
--
-- Returns { commands = { <api.cmd function name> = spec, ... } }; the bridge registers them in L.COMMANDS
-- (feverlink3_lockstep.lua documents the spec fields, including the encode/decode hooks for engine userdata).
-- One module per area so the areas grow independently. Nothing registered yet.
return { commands = {} }
