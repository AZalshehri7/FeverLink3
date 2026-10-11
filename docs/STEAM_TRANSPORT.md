# Steam transport

Players reach a host by its Steam ID. Steam punches through NATs and falls back to its own relays (Steam Datagram
Relay), so nobody forwards a port and no server of ours is involved. The Steam transport carries the same sealed
frames as the TCP transport; nothing in the protocol, the state machines or the mod changes.

**Status.** Built and tested against an in-memory Steam: 75 tests, the transport and session ones on the real code
paths (the real poller thread, the real `run_host_over` / `run_player_over`). The pieces the earlier probe used were
proven in the game on 2026-10-09; everything else is listed in [section 9](#9-needs-game-verification) with the steps
to check it. Nothing here has run between two real accounts yet.

## 1. Parts

| File | What it is |
|---|---|
| `launcher/src/steamnet.rs` | The Steam structure layouts as offsets with pure parsers, the ping protocol, and the **probe** (a service on the core, virtual port 1, the `Probe` / `Report` surface `live.rs` uses). |
| `launcher/src/steamnet/steam_core.rs` | **`SteamCore`**: the interfaces, the one global callback, the one poller thread. `CoreRef` is the handle services hold. |
| `launcher/src/steam_transport.rs` | **`SteamHostNet`** (virtual port 0), **`SteamClientNet`**, **`SteamEndpoint`**, and the helpers that start sessions over Steam. |
| `launcher/src/steam_transport/selfcheck.rs` | `loopback_check`: the transports against each other on one account, for the game (section 9, step A). |
| `launcher/src/steamnet/fake.rs`, `steam_transport/session_tests.rs` | The in-memory Steam and the end-to-end session tests (test code only). |
| `launcher/src/net.rs` | Additive: the `HostTransport` / `ClientTransport` traits (TCP and Steam implement them), `ConnectContext`, `LinkInfo`. |
| `launcher/src/session.rs` | `run_host` / `run_player` became wrappers over `run_host_over` / `run_player_over`; `Status` gained `links`. |
| `launcher/src/live.rs` | `host` / `join` over Steam (the default; `transport = "tcp"` keeps the developer transport), status keys (`transport`, `steamid`, `progress`, `peers`, `rate`, `ping`, `links`, `loop`) and the `steamloop` command. |

## 2. The core

Steam delivers connection events through one process-wide callback, and only while somebody calls `RunCallbacks`; a
connection that arrives on a listen socket must be accepted within a second or two. So one poller thread
(`feverlink3-steam`) does all of it for everybody:

1. find Steam's interfaces (until Steam is initialised; the game does that late), request relay access, register
   the global status callback, apply the tuning;
2. each cycle: `RunCallbacks`, accept connections that arrived on a listen socket some owner opened, mark
   connections open, read every open connection (up to 64 messages each per cycle), measure the route and ping of
   connections whose second is up, then hand everything to the owners with no lock held.

**Owners.** The probe, the host transport and the client transport each implement `Owner` (`opened`, `progress`,
`message`, `closed`). A connection belongs to exactly one; its callbacks come in order, `closed` exactly once and
last. An owner may call back into the core (send, close, connect) from inside a callback.

**Locks.** One mutex serialises every call into Steam (nothing relies on Steam's own threading rules); the registry
(handle to owner) is taken after it, never before. The game's own `SteamAPI_RunCallbacks` may also fire our
callback on a game thread; it only queues the event.

**Pace.** The latency the core adds is its sleep. With a connection it sleeps **2 ms**; with only listen sockets
(the probe listens all game long) **20 ms**; with nothing **100 ms**. A new listen socket, connection or close wakes
it at once. The probe's old controller polled every 100 ms at both ends, which is why its round trip never read
below 100 ms (a real relay test read 199 ms; a loopback test also read 100 ms). Measured on Windows 11 in a test of
the core (polls in 400 ms): 163 with a connection (2.5 ms per cycle, the sleep is a high-resolution one), 13 with
only a listen socket (about 30 ms: a condition-variable wait is rounded to the 15.6 ms system tick), 3 idle.

**Several cores.** The process has one (`steamnet::global()`, started on first use, retried if the game loads
`steam_api64.dll` late). The tests make one per fake machine; the callback finds its core by thread.

## 3. Frames over Steam

- **One Steam message is exactly one frame.** `FrameCodec::encode_frame` out, `FrameCodec::try_decode` in. A message
  that is not exactly one whole frame (trailing bytes, cut off, a length outside the protocol's bounds, empty) closes
  that connection with the reason; nothing else is touched.
- **Sizes.** Steam sends at most 512 KiB per message (`k_cbMaxSteamNetworkingSocketsMessageSizeSend`) and by default
  refuses to receive more. The protocol allows frames up to 1 MiB. The biggest frame that really occurs is a
  savegame chunk: 256 KiB of data, base64, JSON, tag, header, length is **349 704 bytes at its worst** (a test builds
  it from `ResyncLimits::CHUNK_SIZE` and the widest numbers), 67 % of the limit with 174 KB to spare. A save of 31 MB
  is 119 such chunks (the end-to-end test moves 31 MiB, 125 chunks, through a resync round). The biggest frame the
  transport takes has a body of 524 272 bytes; one byte more is refused by `send` before Steam sees it, the connection
  stays open. A test fails if a larger chunk size ever stops fitting.
- **Backpressure.** Steam queues at most 512 KiB per connection and answers more with "limit exceeded" where a TCP
  write would block. `send` waits for room (2 ms steps, up to 30 s), so `stream_save` is paced by the link; a chunk
  that cannot be queued for 30 s closes the connection (`write failed: ...`).
- **Order and loss.** Reliable messages (flag 8), in order, as TCP.
- **Nagle.** Steam batches small sends for up to 5 ms (its default). Not changed.

## 4. Connecting

`SteamClientNet::connect_with(core, endpoint, ConnectContext { conn, tx, timeout, cancelled, progress })`:

1. waits for Steam to be ready inside the timeout (progress text `waiting for Steam`);
2. `ConnectP2P` to the host's Steam ID on virtual port 0;
3. reports progress as it changes: `contacting the host`, `finding a route`, then `connected, direct` or
   `connected, through a Steam relay`;
4. returns when open (the first event on `tx` is `Opened`), when Steam gives up (`could not connect to Steam ID X:
   <Steam's reason with its code>`), when `timeout` passes (`no route to Steam ID X after 30.0 s (last status: finding
   a route)`) or when `cancelled()` says so. A failed attempt sends no event to the session.

The timeout is **30 s** (`CONNECT_TIMEOUT`) because a route through the relays can take 10 to 20 s. Steam's own limit
for a new connection (`TimeoutInitial`; the public header does not state the default, it is believed to be 10 s) would
end such a search early, so the core also sets it to **45 s** when Steam becomes ready.

In a session the first attempt's progress goes to the caller (`run_player_over(config, connect, &mut |text| ...)`);
reconnect attempts write theirs to the session log, and a stop request cancels an attempt that is waiting, so leaving
never waits 30 s.

## 5. Closing, and the host leaving

The rules at the top of `net.rs` hold: `Closed` exactly once and last; no frames after `close` or a failed write.

- **A close lingers.** `CloseConnection` is called with linger, so a frame written just before it (the host's
  `Reject`) still reaches the other side; a test with a slow wire proves the player learns `bad-proof` instead of a
  silent close.
- **The other side's close and Steam's problems** (timeout, no route, network down) end the connection the same way;
  what was queued before the end is still delivered first (Steam keeps the inbound queue readable after
  `ClosedByPeer` and `ProblemDetectedLocally`). The reason keeps Steam's own code and text: `closed by the other side:
  closed by the other side's program (1000)`, `connection problem: timed out (5003)`.
- **The host leaves** (stops the session, drops the transport, or its game ends): every player gets `Closed` and the
  player loop does what it does for TCP: tells its game `session started=false`, retries every 5 s for 5 minutes,
  joins again when the host is back on the same Steam ID. A host whose game or network dies without closing is noticed
  by Steam after its connected timeout (`timed out (5003)`; believed to be about 10 s by default, not stated in the
  header).
- **Turned away.** A host keeps at most 64 connections, handshakes pending included; the 65th is closed at once and
  the session never hears of it. The session itself holds eight games (`max_players` counts the host: the host and
  seven players); the next one is rejected `full (8 players)` and told so.

## 6. What the window can show

`Status.links` (host: one entry per connection, by connection id; player: one, to the host) and the probe `Report`
carry, per connection, **Steam's own ping** (`GetConnectionRealTimeStatus`, `m_nPing`), the **route** (`direct`,
`through a Steam relay`) and the **send rate**. The core measures again every second, so a relayed connection that
Steam later moves to a direct path shows as direct a moment later (the probe adds an event `route changed: ...`).
`live.rs` publishes them as `steam.test.{ping,rate,route}`, `steam.peers[]`, `links[]`.

## 7. Tuning

Set globally when Steam becomes ready, best effort (a refusal is reported in the status note
`tuning refused for: ...`, nothing else changes):

| Setting | Value | Why |
|---|---|---|
| `TimeoutInitial` (24) | 45 000 ms | Steam's own limit (believed 10 s) would end a slow route search; our connect timeout (30 s) is the limit |
| `SendRateMin` and `SendRateMax` (10, 11) | 1 048 576 B/s | Steam's default is 262 144 B/s, 2 minutes for a 31 MB save; at 1 MiB/s about 30 s. Steam wants both set to the same value |

Not changed: send buffer (512 KiB), Nagle (5 ms), receive limits (512 KiB per message). The tuned send rate is visible
as `rate` in the report; 262 144 means the tuning did not take effect.

**Changing the send rate for a test: `FEVERLINK3_STEAM_SENDRATE=<bytes per second>`.** Set it in the environment of the
game (before it starts) on both PCs. The numbers 10 (`SendRateMin`) and 11 (`SendRateMax`) were checked against
`steamnetworkingtypes.h` of GameNetworkingSockets (`k_ESteamNetworkingConfig_SendRateMin = 10`, `...SendRateMax = 11`),
and the header says to set both to the same value to fix the rate (the library does not estimate the bandwidth yet), which
is what the core does. The value is clamped to 32 KiB/s .. 64 MiB/s; unset, empty or not a number means 1 048 576. The
speed test's second line shows both the value asked for and the one Steam reports (`GetConnectionRealTimeStatus`,
`m_nSendRateBytesPerSecond`). **What is a good value?** Steam sends at exactly that rate whatever the line can carry, so
too high on a slow line means queuing and loss, too low wastes the line. Try 1 MiB/s (the default), then 2 097 152 and
4 194 304 and keep the highest one without a stall or a growing queue. An 80 MB save is 107 MB on the wire (base64), so at
1 MiB/s it takes about 102 s, at 4 MiB/s about 26 s.

## 7.1 The speed test

The window's **Run the speed test** (command `steamtest` with `{id, mb}`) answers: can a resync save (about 80 MB) cross
Steam, and how fast. It uses the probe's connection (virtual port 1), so both players need the window of a game with the
mod; the friend does nothing but wait. Code: `launcher/src/steamnet/throughput.rs` (messages, timing, result text) and
the probe in `launcher/src/steamnet.rs` (state machine).

Data: random bytes (incompressible), in messages of 349 524 bytes of payload, the size of one resync chunk on the wire
(256 KiB of save, base64). They go through `send_reliable` like resync chunks do, so Steam's 512 KiB buffer paces the
sender exactly as it does for a save.

Message flow (each line one Steam message; `d` is the data message):

```
tester                               friend
  tput up <id> <bytes> <chunk>  -->     starts timing
  tput d <id> <seq> + payload   -->  (N chunks, sender thread, blocks while the buffer is full)
                                <--  tput prog <id> <bytes>        every 500 ms
  tput end <id> <sent> <peak queued> <longest wait>  -->
                                <--  tput upres <id> <bytes> <ms> <longest gap> <bytes after first chunk>
  tput down <id> <bytes> <chunk> -->    starts a sender thread
                                <--  tput d ... (N chunks)
                                <--  tput end <id> <sent> <peak queued> <longest wait>
```

The receiver times from the first chunk to the last (so the route set-up does not count) and the rate is the bytes after
the first chunk over that time. Result, shown under the button and as one `steam speed test:` line in `session.log`:

- MB/s each direction (1 MB = 1 000 000 bytes) and total seconds;
- route (`direct` or `through a Steam relay`, from the connection info flags) and Steam's round trip (`m_nPing`);
- the send rate asked for and the one Steam reports;
- peak queued bytes (`m_cbPendingReliable`, measured by the core once a second, so a coarse peak, at most 512 KiB);
- a stall: the longest time without data (the receiver's longest gap, the sender's longest wait for buffer room); over
  5 s it says STALLED. The test is abandoned after 60 s without progress;
- the estimate for an 80 MB save (107 MB on the wire) at the measured rates: "if you host" uses your upload, "if your
  friend hosts" your download.

A friend can ask a probe to send at most 128 MB and two such streams at once; more is ignored.

The fake-Steam tests (two in-memory accounts) cover the messages, the math, a full run, a refusal, a friend who leaves
and the limits. They cannot say anything about a real line; see the steps in `docs/PLAYING.md`, section 5.

## 8. What is proven

**In the real game** (build 40420, 2026-10-09, by the probe; the code of these steps was kept):

- The functions come from the flat API exports of the `steam_api64.dll` the game already loaded (`GetModuleHandleA` +
  `GetProcAddress`, no SDK linked).
- The accessors `SteamAPI_SteamNetworkingSockets_SteamAPI_v012` and `SteamAPI_SteamUser_v023` return null in the game.
  What works is `SteamInternal_FindOrCreateUserInterface(SteamAPI_GetHSteamUser(), "SteamNetworkingSockets012" /
  "SteamNetworkingUtils004" / "SteamUser023")`, valid once the game has initialised Steam (late: the core polls).
- The global status callback (`SetGlobalCallback_SteamNetConnectionStatusChanged`) fires when
  `ISteamNetworkingSockets_RunCallbacks` is called, and accepting incoming connections needs it.
- A P2P listen socket accepts connections by Steam ID; reliable messages (flag 8) go both ways through
  `SendMessageToConnection` / `ReceiveMessagesOnConnection`, each message released with
  `SteamNetworkingMessage_t_Release`; ping and pong with the owner's own Steam ID worked in about 100 ms steps (the
  probe's polling).
- A friend's account connected to the owner's Steam ID through Steam, route "through a Steam relay", probe-measured
  round trip 199 ms (inflated by the 100 ms controller polling at both ends).

**In the public headers** (2026-10-10, see section 11): every structure layout and constant the code uses.

**Assumed, not proven**: everything in section 9 below, notably the tuning calls, `GetConnectionRealTimeStatus`,
`CloseListenSocket`, the 512 KiB behaviour of the send buffer against the real library, linger on close, and the
`SteamAPI_GetHSteamUser`-is-0-after-shutdown guard.

## 9. Needs game verification

Everything above is tested against the fake. The fake models Steam as the headers and the SDK documentation
describe it; it cannot prove how the real library behaves. Do these in order; each step says what proves it.

**A. One account, one PC** (regression of the probe on the new core, then the transport on the real library).

1. Build the native module, start Transport Fever 3 through Steam, open the FeverLink3 window, note your Steam ID.
2. *Test the connection* with your own Steam ID. Expect within 3 s `state = connected`, `pongs` rising every 2 s,
   `ping` 0 to 2 ms, `rtt` under 15 ms (it read 100 ms before the dedicated poller). `events` should start with
   `Steam is ready: tuned: initial timeout 45000 ms, send rate 1048576 bytes/s`, then
   `listening for Steam connections`.
   - `tuning refused for: ...`: `SetConfigValue` is not taking our values; a big resync will take minutes; report the
     exact note.
   - `rate` is Steam's own figure for the connection. For a connection to your own account Steam may use an internal
     loopback and report something else; the figure that counts is the one in step B (1048576 when the tuning took
     effect, 262144 when it did not).
3. Run the self-check: type `loop` into the Steam ID field of *Test the connection* and press it (the window has no
   other button for it; the command is also `steamloop`).
4. Within about 10 s `steam.loop` in `ctl_status.txt` lists nine checks, all `ok = true`: this account, listen on
   virtual port 0, connect (`detail` shows the progress texts and the time), both ends report the connection, round
   trips (`fastest`/`median` should be a few ms; a median near 100 ms means the poller is not running at its pace),
   40 chunk-sized frames in order (MB/s), the biggest frame and the limit, close does not overtake a frame, both ends
   report the end once. **A failure here is a bug in the transport or a wrong assumption about Steam; send the
   whole list.** While a session is being hosted the second check fails with "virtual port 0 ... already in use":
   run it before hosting.

**B. Two accounts, two PCs, game running on both** (the real network).

0. **Both PCs must run this build**: the probe listens on virtual port 1 now (it was 0), so a friend on the old build
   answers "no listen socket on that virtual port".
1. A notes the Steam ID shown in its window; B types it and presses *Test the connection*.
2. B expects `connected` within a few seconds (note the time and the `finding a route` phase), `route`, `ping`, `rtt`
   (should now agree with `ping` within about 10 ms; a gap of 100 ms means polling is still slow somewhere), `rate`.
3. A expects `peers` with B's Steam ID, route, ping, rate.
4. Leave both running a minute: if `route` was a relay, an event `route changed: through a Steam relay -> direct`
   shows whether Steam upgrades the path (not guaranteed; both outcomes are information).
5. Close B's game without leaving: A's `peers` empties when Steam's connected timeout runs out (about 10 s).
6. **Quit the game on A while B is connected and while the probe listens**: no crash dialog, no hang on exit (the
   poller must not call Steam after `SteamAPI_Shutdown`; the `Steam went away` guard relies on
   `SteamAPI_GetHSteamUser` returning 0 then, which is assumed, not proven). If the game crashes on exit, report it.

**C. The session over Steam** (the live controller hosts over Steam by default and joins a code that carries a Steam
ID over Steam; `transport = "tcp"` keeps the direct developer transport). Press *Host* on A (no port, no address
in the code), give the code to B, press *Join* on B.

1. B joins A: record the progress texts and the time from *Join* to *joined*; three tries. If a try fails after
   about 10 s with `timed out (5003)`, `TimeoutInitial` did not apply.
2. A wrong password: B must say `bad-proof`, not a silent close (proves linger on the real library).
3. A leaves: B shows reconnecting; A hosts again on the same account: B rejoins within about 5 s.
4. Resync with the real 31 MB save: the whole round, and the `Transferring` time; expect about 30 to 40 s at
   1 MiB/s. Anything near 2 minutes means the send rate stayed at 256 KiB/s.
5. During play `links` should show the host a ping per player and a player the ping to the host.

**Cannot be tested:** two game processes of the *same* account on one PC (Steam runs one instance per account), and
a full table of eight games (needs 8 accounts; the transport is tested with 8 clients and the session with a full table
on the fake).

## 10. Failure modes

| Situation | What happens |
|---|---|
| Steam not initialised yet | The core keeps looking every 250 ms. `listen` and `connect` wait (15 s / inside the connect timeout) and then fail with `Steam networking is not ready (Steam interfaces: hsteamuser 0, user null sockets null utils null)`. After 2 minutes the note says Steam networking is not available. |
| No `steam_api64.dll` in the process (game started without Steam, a test run) | `steamnet::global()` is `None`: `SteamHostNet::listen` fails with `Steam networking is not available (is the game running through Steam?)`; the controller looks again every 2 s (`looking for Steam...`, `not available` after about 5 minutes). |
| Steam shut down under the core (the game is quitting, the Steam client restarts) | Every poll cycle asks `SteamAPI_GetHSteamUser` whether Steam is still initialised (0 once it is shut down). When it is not: all connections end with `Steam went away`, listen sockets are forgotten, **no further call goes into the vanished interfaces**, and the core looks for Steam again (a restarted client brings new interfaces; the host transport must listen again). |
| A missing optional export (`CloseListenSocket`, `SetConfigValue`, `GetConnectionRealTimeStatus`) | A listen socket leaks until the process ends / no tuning / no Steam ping. A missing required export: no Steam at all. |
| No route to the host (firewall blocks Steam's UDP, relay network unreachable, host not running the game) | The attempt stays at `contacting the host` or `finding a route` until Steam gives up (45 s) or ours (30 s): `no route to Steam ID X after 30.0 s (last status: ...)`. Steam's reason codes are kept in the text (5003 timed out, 5009 NAT/firewall, 5006 no relay, 3001 offline mode, ...). |
| Nobody listens on the virtual port (host not hosting) | Fails fast: `could not connect to Steam ID X: closed by the other side ...`. |
| **The same account on both ends** | A connection to one's own Steam ID works inside one process (the probe and `steamloop` use it). Two game processes of the same account on one PC cannot be used to test between them. Two *different* accounts on two PCs are needed. |
| A stranger knows the host's Steam ID | Can open Steam connections, cannot pass the handshake (it needs the secret in the join code); at most 64 connections are kept. |
| A peer stops reading | `send` waits up to 30 s, then closes that connection. The host's session thread is blocked meanwhile (TCP blocks forever). A per-connection send queue would remove the stall; not built. |
| Another user of Steam networking in the process | We set the global connection callback; a second setter would replace ours. Events about connections we do not own are ignored, never closed. |
| Message above 512 KiB | Refused before Steam, the connection stays open (cannot happen with today's frames). |
| The player's IP | Steam's direct (ICE) route can show the players' addresses to each other. Relay only needs `P2P_Transport_ICE_Enable` (104) set to 0 or 1; not set. |

## 11. Testing without Steam

`steamnet/fake.rs` is `ISteamNetworkingSockets` as extern "C" functions over a global model, plugged into the same
`Api` table the game's exports fill. A test makes machines (own account, interfaces, callback, event queue), starts a
real `SteamCore` on each (real poller thread), and everything above runs unchanged.

It models: connection pairs and the Connecting / FindingRoute / Connected / ClosedByPeer / ProblemDetectedLocally
states as callbacks delivered by `RunCallbacks`; `AcceptConnection`; reliable ordered messages; a 512 KiB send buffer
per connection ("limit exceeded" until the peer reads); close with and without linger (data still on the wire is lost
without it); the structure layouts (written with literal offsets, independent of the parser's constants); message
objects handed out and released (a test fails on a leak or a double release); the tuning calls and their effect on the
reported send rate. Knobs: Steam not ready, unreachable account, route delay, wire delay, relay point, ping, a peer
that stops reading, a dead network, injected garbage, events about foreign connections.

It cannot prove: NAT punching and relays, Steam's real timing, the real send-rate behaviour, what Steam does with a
window of lost packets, thread-safety of the real library (every call goes through one lock for that reason).

The structure layouts and constants were checked against the public headers in
`ValveSoftware/GameNetworkingSockets` (`steamnetworkingtypes.h`, `isteamnetworkingsockets.h`,
`steamnetworkingsockets_flat.h`, `steamclientpublic.h`) on 2026-10-10: identity 136 bytes, connection info 696 with
the state at 176, end reason at 180, end debug at 184, flags at 440; the status callback (handle, info at 8, old state
after it); the message (data at 0, size at 8); the real-time status (state 0, ping 4, quality 8 and 12, send rate 32,
pending reliable 40); connection states, `EResult` values, config values, send flags, limits.

## 12. Using it

```rust
// host: the Steam ID joiners need is steamnet::global()?.status().steam_id (also steam.id in the controller's status)
let handle = steam_transport::run_steam_host(host_config)?;            // host_config.bind is not used

// player: 17 digits from the join code
let host = SteamEndpoint::parse(text).ok_or("not a Steam ID")?;
let config = steam_transport::steam_player_config(&name, key, link);   // 30 s connect timeout
let handle = steam_transport::run_steam_player(config, host, &mut |text| show(text))?;
```

`SteamEndpoint::format()` / `parse()` are the 17-digit text; `parse` also checks that the number is an individual
account of the public universe, so a typo is almost never accepted. `run_steam_host_on` / `run_steam_player_on` take
a `CoreRef` (the tests and the live controller's `Controller::with_core` give them a fake Steam).

**The live controller** (`live.rs`): `host` with `transport` = `steam` (or missing, `auto`) checks that Steam is
ready, starts `run_steam_host_on`, and publishes a code made by `JoinCode::create(&[], secret, None)` plus
`with_steam_id(id)`: no endpoints, the host's Steam ID in extension tag 2. The status says `transport = "steam"` and
`steamid`. `join` decodes the code; **a code with a `steam_id` is joined over Steam** (whatever endpoints it lists),
on a thread of its own with `steam_player_config` (30 s) and `progress` in the status while it connects ("contacting
the host", "finding a route", "connected, direct"); a code without one uses TCP as before. `transport = "tcp"` on
`host` is the developer transport. If Steam is not loaded or not ready, hosting fails at once with a message
(`Steam is not ready yet (...); try again in a moment`) rather than waiting; `leave` while *joining* does nothing
(as before), the attempt ends by itself within 30 s.

## 13. Decisions for the owner

1. **A code with both a Steam ID and addresses** (a developer could make one): the live controller joins over Steam
   whenever the code has a Steam ID. Alternatives: try Steam, then the addresses.
2. **Defaults**: send rate 1 MiB/s (a 10 Mbit/s uplink is about 1.2 MB/s; slower uplinks queue and lose more),
   initial timeout 45 s, connect timeout 30 s, 64 connections.
3. **IP privacy**: leave Steam's direct routes on (faster, shows addresses to the other players) or force relay.
4. **A stalled player** blocking the host's session thread for up to 30 s: accept it (TCP is worse) or give the
   host transport a per-connection send queue with a cap.
5. **The probe listens all game long** (a 20 ms poll, a few tenths of a percent of one core). Alternative: open the
   listen socket only while the Steam test is on screen.
