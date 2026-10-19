# Third-party notices

FeverLink3 itself is licensed under the PolyForm Noncommercial License 1.0.0 (see LICENSE). The MIT licences below apply only to the third-party material they cover; anything adapted from those projects keeps the MIT notice that goes with it.

No source file of the projects below is copied into FeverLink3. Their published designs, documentation, code and findings shaped it, though, and several mechanisms are re-implemented after them in our own words and structure: the capture of native builds from the engine's build event and their replay on the other games (with the replay marks that keep a replay from being sent back), holding a construction tool's command, cancelling it and applying the build at the stamp on every game, the originator's included (strict mode, `mod/feverlink3/content/feverlink3_bridge.script.lua`), the hook that wraps the engine's command functions in the window's Lua state, the single-worker script pool patch, deterministic `math.random`, the savegame resync as the repair, simulation seeds from the step count instead of the frame count, stamping an action past the fastest clock, and the idea of correcting money from the host. The place in the code where one of these follows a project says so in its comment, and everything adapted keeps the MIT licence reproduced here.

## TpF2 Multiplayer

https://github.com/silver2127/tpf2-multiplayer

```
MIT License

Copyright (c) 2026 silver2127

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## MPFever

https://github.com/dkdknight/MPFever

Also from MPFever: the idea of pinning Lua's string-hash seed (`makeseed`, called by `lua_newstate`) to a constant so
that `pairs` order over string keys is the same in every process, and the location of that function in build 40420.
The code is our own; the site was located with their findings. The pin is on by default (`FEVERLINK3_LUA_SEED=0` turns it off) and is skipped, with the reason logged, on a build or bytes it does not know.

Also by way of MPFever (0.3.15, which ported it from TPF3-MP, below): smooth pacing, stretching the engine's batch interval after
`CGame::Sync` so that a game near the input barrier slows down instead of standing still.

```
MIT License

Copyright (c) 2026 arthur

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Transport Fever 3 Multiplayer Mod (TPF3-MP)

https://github.com/Juliansgith/Transport-Fever-3-Multiplayer-Mod

From TPF3-MP, as ported by MPFever: the finding that the engine seeds simulation draws from `GameTime.tickCount`, which
also counts the frames a game stands still, and that these seeds should come from the simulation's step count instead.
Our version is our own design: one patch on the engine's tickCount getter itself (`native/src/build_table.rs`,
`tick_count_getter`), so every caller reads `updateCount` and the field stays as it is; the callers were found by our
own scan of build 40420. On by default (`FEVERLINK3_STEP_SEED=0` turns it off) and skipped, with the reason logged, on a
build or bytes it does not know.

The idea of smooth pacing and the place it works on: the engine's batch interval (`guiFrameTime`, the interval at which
`CGame::Step` calls `CGame::Sync` to hand the simulation its next batch) can be stretched right after a `Sync` that handed one
out, so a game slows down without changing the steps it runs (its "batch interval", `docs/HOOKS.md`). FeverLink3 found the call again
in build 40420 and re-implemented it in its own form (`native/src/pace.rs`, the factor from the measured room and stop cadence in
`mod/feverlink3/content/feverlink3_lockstep.lua`, "smooth pacing"). On by default; `FEVERLINK3_SMOOTH_PACE=0` turns it off.

From TPF3-MP, as ported by MPFever 0.3.14: the finding that `ecs::Engine::EndModification` appends the ids a modification
removed to the FIFO free-id queue in the order the engine visited the entities, and that sorting each batch first makes the ids
handed out independent of that order. FeverLink3 found the place again in build 40420 and re-implemented it in its own form: a
redirect of the one call that appends the batch (`native/src/build_table.rs`, `free_order`; `native/src/freeorder.rs`). On by
default; `FEVERLINK3_FREE_ORDER=0` turns it off.

```
MIT License

Copyright (c) 2026 Julian Cooper

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
