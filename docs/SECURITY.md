# Security

What FeverLink3 protects, against whom, and what it does not. The design is `docs/ARCHITECTURE.md` section 3; the
wire format is `docs/PROTOCOL.md`. This document is the review of both against an attacker, the panic policy for the
code that runs inside the game, and the limits that bound what a stranger can cost. Every rule below has a test; the
tests are named at the end of the rule.

FeverLink3 is a co-op mod for friends. The goal is that nobody outside the group can join, read or change a session,
that nobody inside it can crash another player's game or the host's PC with a malformed message, and that nothing the
mod listens to on the network or reads from a file can take the game down. It is not a defence against a host who
means harm (section 4), and not a defence against a program already running as the same Windows user.

## 1. Assets

| Asset | Why it matters |
|---|---|
| The player's game process | The native module and the session controller run inside `TransportFever3.exe`. A panic, a stack overflow or an endless loop there ends or freezes the game and loses unsaved play. |
| The shared world | Every game applies the same actions; an action nobody should be able to send, or a clock nobody should be able to lie about, changes everyone's world. |
| The join code and the lobby password | The code carries the 16-byte session secret and how to reach the host (Steam ID, relays, or direct addresses). Whoever holds it (and the password, if one is set) can join. |
| The player's files and PC | The mod writes savegames received from the host into the save folder; the launcher installs a DLL into the game folder and updates itself. On the direct developer transport the host listens on a TCP port; the product's transports (Steam peer-to-peer, relay) open no port on the host's PC. |
| Privacy | IP addresses, display names, what the players build. |

## 2. Attackers

| Attacker | Can | Cannot |
|---|---|---|
| **A. Stranger** who finds the host's port (a scan, a leaked address) | Open connections, send bytes, never complete a handshake | Make a game do anything: every frame past the handshake is sealed with a key derived from the code |
| **B. Network observer or relay** (ISP, Wi-Fi, a relay server) | Read headers (kind, sender, counter, length), drop, delay, reorder, cut connections, inject an unsealed `Reject` | Read or alter a payload, forge a frame, replay a frame inside a connection |
| **C. Joiner** with the code (and the password) | Send every message kind a player may send (section 3), with any content, as fast as the host will read | Pass as the host or another player, change the session state or permissions, send a host-only kind |
| **D. Malicious host**, seen by a joiner | Everything: it decides the state, relays the actions, sends the savegame, and a savegame runs code when it loads | (nothing is contained: section 4) |
| **E. Relay operator, or someone who connects to a relay** | The operator is attacker B for every room (headers, IP addresses, the room name, cutting and delaying); anyone can send the relay's messages to it | Read or alter a payload (the relay never has the key; the room name is `HKDF(secret, "relay room")`, which says nothing about the key). Make a *game* do anything: a relay message is parsed by `RelayCodec`, which allocates nothing for an announced length and refuses over 1 MiB + 16 bytes |
| **F. Steam user** who knows an account's Steam ID | Open a Steam connection to the probe (16 at a time) or to the host's session listener (64 at a time) and send frames; Steam says whose account it is | Get past the handshake without the code, reach more than the session's limits allow, or reach the session through the probe (it carries none) |
| **G. Same-user program** | Anything the user can do, including the mod's files and the game folder | (out of scope) |

## 3. What is protected, and how

| Threat | Mechanism | Test |
|---|---|---|
| Joining without the code | The key is `HKDF(secret [‖ PBKDF2(password)])`. A Join carries `HMAC(key, nonce ‖ name)` over a per-connection random nonce; the check is constant time (`verify_slice`). A password adds 600 000 PBKDF2 iterations for anyone who has the code but not the password. | `session_tests`, `sealed_channel_tests` |
| Reading or altering a frame | AES-256-GCM; the 12 header bytes are the additional data, so kind, sender and counter cannot change either | `sealed_channel_tests`, `robustness_tests::the_opener_accepts_exactly_what_a_sealer_made...` |
| Replaying a frame | 64-frame replay window per sender; a counter is recorded only after the tag verified, and a sender gets a window only then, so forged frames can neither burn a counter nor grow the opener. A player refuses host frames numbered at or below its Welcome's, and a Welcome that does not carry the nonce of this connection's challenge | `sealed_channel_tests`, `wire_limits_tests::forged_frames_leave_the_opener_empty`, `robustness_tests::the_replay_window_agrees_with_a_model...`, `reconnect_tests` |
| Nonce reuse | Every session has a new secret; a sender id given out again (a player coming back inside the host's 330 s window) starts a new epoch, and its counter starts at `epoch << 40`, above everything an earlier use of the id can reach; epochs run to 2^24 and then the id is not reused; a `Sealer` that would run out of counters refuses instead of wrapping; a second Welcome cannot replace a player's sealer. Open: a replayed Challenge and Welcome (section 4) | `hardening_tests::no_nonce_is_used_twice...`, `a_second_welcome_does_not_restart...`, `reconnect_tests::every_return_uses_a_later_epoch`, `wire_limits_tests::the_sealer_refuses...` |
| Taking over a held slot | Only the holder of the player's resume token: the claim is `HMAC(token, "FeverLink3 resume" ‖ nonce ‖ id ‖ name)` over this connection's nonce, checked in constant time; the token is 16 random bytes sent once, sealed inside the Welcome; a wrong or missing claim is a plain join under a new id and never touches the held slot | `reconnect_tests`, `robustness_tests::a_join_that_claims_an_old_slot_gets_it_only_with_the_token` |
| Speaking as another player or the host | The host checks the frame's sender id against the connection before it opens the frame; it fills `from`, `name`, `category` and `allowed` itself; a player may send only kinds 30 to 36, 50 and 51 | `session_tests`, `hardening_tests`, `robustness_tests::a_hostile_joined_player...` |
| A stranger holding the host's resources | 64 connections (TCP: 16 per address; Steam: 64 in all, the next one is closed), 32 waiting to join at once (`HostSession::MAX_PENDING`), a join within 10 s (`handshake_timeout_seconds`; the host does not count a stall of its own as a stranger's silence), a first TCP frame of at most 4 KiB, a write that must finish in 10 s plus 1 s per 32 KiB, TCP sockets that are polled so that a close takes effect in 5 ms (a blocked read on a silent peer outlived every shutdown, and a read with a timeout failed with `ERROR_IO_PENDING` about once in 3000 timeouts when data arrived as it expired: both measured). One Steam message is one frame, cut at Steam's 512 KiB | `net::tests`, `steam_transport::tests`, `hardening_tests::the_host_closes_connections_that_never_join...`, `liveness_tests` |
| A player flooding the host | Per connection, on every transport: 500 frames a second (burst 2000) and 2 MiB a second (burst 8 MiB, a frame costs its header and body), then the connection is refused with `flood`; on TCP also the reader waits at 1000 frames and 1 MiB a second. Messages that are too big are dropped: Action body 480 KiB, checksum report 128 parts of 128 characters. The byte budget is what stops 480 KiB actions at 500 a second (240 MB/s into the host's disk) over the Steam transport, which has no throttle of its own | `limits_tests`, `hardening_tests::a_connection_over_its_byte_budget...`, `net::tests::a_peer_that_sends_faster...` |
| A player lying about time | A Clock outside `0..=2^50`, or with a speed outside `0..=16`, is dropped; the stamp arithmetic saturates | `hardening_tests::clocks_that_are_not_game_times_are_dropped` |
| Memory that grows with what a peer chooses | Log-once memory is cleared at 1024 keys and keyed by reasons, not by the peer's values; peer text is clipped before it reaches a log line (120 to 2000 characters); the host keeps 20 checksum rounds | `hardening_tests::the_log_does_not_grow...`, `robustness_tests` (tracking allocator) |
| An injected `Reject` ending a session | A Reject is unsealed, so a player honours it only while it is still joining | `hardening_tests::an_injected_reject...` |
| Code in an action body | The mod `load`s the body in an empty environment; that still runs calls and loops (`("x"):rep(1e9)`, `while true do end`). The launcher gives the game an action only if its body is a Lua *literal* (tables, strings, numbers, booleans, nil; the mod's NaN and infinity spellings) | `luatext::tests::a_literal_is_what_the_mods_serialiser_writes...`, `session::tests::lines_translate_both_ways`, `robustness::an_action_body_reaches_the_game_only_as_a_literal` |
| A file the game writes | The tail reads at most 8 MiB per poll, drops a line over 4 MiB whole, takes `seq` only up to 2^53, and reads invalid UTF-8 as replacement characters; the Lua text parser stops at 4 MiB, 250 000 values and 48 levels | `gamelink::tests`, `robustness` |
| A crafted join code | At most 2048 characters of text (the longest valid code is 976) before anything is decoded; 8 endpoints, 8 extensions, 4 relays, relay names of 1 to 100 characters (letters, digits, `-`, `.`); unknown extensions are skipped by their length, never interpreted; the base-32 decoder never allocates more than the text allows; the checksum is checked before the structure is read | `join_code_relay_tests`, `wire_limits_tests::the_longest_possible_code_still_fits...`, `robustness_tests::join_code_structure_after_a_valid_checksum...`, `join_codes_with_relays_and_a_steam_id_round_trip` |
| A relay (or a stranger pretending to be one) sending garbage | `RelayCodec` refuses a length over 1 MiB + 16 bytes before it allocates; `RelayReader` reads at most 256 KiB at a time, so an announced length costs memory only as the bytes arrive; an error text over 1024 bytes is refused (200 are written); a data message must carry a plausible frame header and not more than one frame; unknown message types and error codes are errors or `error-<n>`, never a panic | `robustness_tests::relay_messages_of_any_shape...`, `relay_messages_round_trip_whole_and_through_a_reader_that_stutters` |
| A relay learning the key | The room name is `HKDF(secret, "relay room")` truncated, a different expansion from the session key (the HKDF info differs), so knowing the room name tells nothing about the key; the relay sees sealed frames only | `robustness_tests::relay_rooms_depend_on_the_secret_alone...` |
| The native control link (`native_ctl.log`, `native_events.log`) | The module reads the mod's file in the game process: a line over 8 KiB is thrown away in full, at most 64 KiB are read per poll, a `seq` not above the last is dropped, the payload goes through the Lua-literal reader (no code), numbers must be whole and at most 2^53, a `hold_on` names 1 to 16 lower-case identifiers; the hold ring holds 64 commands, each for 30 s at most, and holding switches itself off after 10 s without a line from the mod; the events file stops at 8 MiB and the module's own queue at 512 events; errors to the mod are rate limited (6 per 10 s) | `hook_robustness` (every test), `link::tests`, `hold::tests` |
| The counters and the watch the hooks use | Fixed arrays of atomics, no allocation, no lock, no file in the simulation thread; a site index past the table is counted as `tool` only; the watch has 64 slots, entries expire after 60 s, a full watch refuses instead of waiting, and a payload equal to the slot marker is refused (a slot would have stayed taken for good: found by the model test) | `hook_robustness::the_watch_matches_a_simple_model`, `the_watch_hands_every_verdict_over_once_across_threads` |
| A Steam user connecting to the probe | At most 16 connections are accepted, messages are cut at 4096 bytes, counters saturate | `steamnet::tests`, `robustness::steam_structures...` |
| A corrupt `libraryfolders.vdf` | The VDF reader goes 32 levels deep and no further (a file of ten thousand `{` used to overflow the stack, which no panic setting survives) | `steam::tests::nesting_without_end...` |
| A tampered update | Ed25519 signature over the manifest, SHA-256 of the zip against the manifest, safe relative paths (no `..`, drive letters or absolute paths) | `update::tests` |
| Savegame corruption in transfer | SHA-256 announced by the host, verified by the player before it writes anything; chunk bounds checked; 512 MiB at most | `resync_tests`, `robustness_tests::a_savegame_assembles...` |

## 4. What is not protected

- **The join code is the credential.** Anyone who has it can join; a code that leaked can be used until the host
  stops. The 4-byte check in the code detects typing mistakes, not forgery. A code names addresses: joining a code
  from someone you do not trust means connecting to an address they chose.
- **A password is only as strong as it is long.** Anyone with the code and a recording of one handshake can test
  passwords offline, 600 000 PBKDF2 iterations at a time.
- **A network observer or relay can drop, delay or cut anything**, and can read the headers (who sends which kind
  how often). It cannot read or change a payload.
- **A recorded connection can still be replayed, in full, into a new one by someone on its path.** Sealed frames
  are bound to the session key. Since the roster work a player ties its Welcome to the nonce of this connection's
  challenge and refuses host frames from before the Welcome, so a recorded Welcome alone no longer works. What does:
  an attacker who controls the new connection (a relay, a man in the middle) replays the recorded *Challenge and*
  *Welcome* together (the player's Join answers the recorded nonce, and the recorded Welcome carries that nonce). The
  attacker still cannot read or forge anything; it can make a player believe an old session state and, because the
  Welcome also carries the epoch the id had then, make that player seal with counters it used before, which exposes
  the XOR of two of that player's messages to whoever recorded both. Closing it needs one change to the handshake:
  the player contributes a random value that goes into the Join and into the key of that connection (protocol
  version 2); see F13 in section 7.
- **The host is trusted by its players.** The host decides the session state, orders every action, can send any
  action as any player (`from` is its to fill), and sends the savegame that every game loads, and a savegame is code
  (`.sav.lua` runs on load). Joining a session is trusting its host as one trusts the author of a mod.
- **A joiner with the code can send every allowed kind.** It can flood within the rates above, send actions that the
  permissions mark `allowed: false` (every game then asks for a resync), report checksums that differ (a resync after
  two rounds), send `ReplayFailed` (a resync, at most one per 120 s), request speed changes when the `speed`
  permission allows it, and join and leave again to start a *hot-join* resync round each time (that round ignores the
  cooldown), or to fill the table with strangers whose slots the host then holds as away for 330 s each. The remedy
  is to share the code with people you trust, to remove a player (the host's `kick`), and to stop the session when
  one of them is not.
- **No forward secrecy:** the key lives as long as the session. A secret is used for one session only; it is made
  fresh for every `host`.
- **The lobby password and the join code pass through files** in `%LOCALAPPDATA%\FeverLink3\live` (the window's
  commands); they stay there until the next game start. The folder is the user's own.
- **A program running as the same user** can do everything the mod can, and more.
- **A relay sees who connects to it and how much they send.** It never has the key, but it knows both players' IP
  addresses, the room name and the size and timing of every frame. Choosing a relay (the code names up to four) is
  choosing whom to show that. Steam peer-to-peer shows the same to Valve and, to the players, each other's address.
- **A relay server must limit its own input.** `RelayCodec` bounds what FeverLink3 reads from a relay; the other
  direction (the relay reading a stranger's first message, rooms and connections per address) is the relay's job. The
  protocol has the error codes for it (`too-many-rooms`, `ip-limit`, `rate-limited`, `timeout`). A relay should
  read no more than a few hundred bytes before it has seen a valid `Register` or `Join`, and should time out a
  connection that has not sent one: the same handshake rule as section 3, row "A stranger holding the host's
  resources".

## 5. The game process: panics, stacks and the unwind decision

`TransportFever3.exe` loads `winhttp.dll` (the native module), which starts the session controller on a thread, and
the controller starts a host or player session with a thread per connection. Everything on those threads is Rust
reading bytes that come from outside: frames, join codes, relay messages, the mod's files (the game link and the
native control link), Steam's structures.

**Policy: a panic reachable from outside input is a bug that is fixed, not contained.** What was done:

1. Every `unwrap`, `expect`, `panic!`, index, slice, `as` conversion, arithmetic assignment and lock on a path
   reachable from `DllMain`, the controller, the session threads, `steamnet`, the relay codec and the native hook
   modules (control link, hold machine, counters, plan; the code that follows pointers into the game's own memory
   checks them with `VirtualQuery` first) was read. What can still panic is in the table below; none of it depends
   on input.
2. Mutexes are taken with `unwrap_or_else(PoisonError::into_inner)`, so a thread that died holding a lock does not
   take the others with it.
3. Randomised tests (`core/tests/robustness_tests.rs`, `launcher/tests/robustness.rs`,
   `native/tests/robustness.rs`, `native/tests/hook_robustness.rs`) feed random, mutated and oversized input to every
   parser (frames, join codes with their extensions, relay messages, the Lua text subset, the game link, the native
   control link, the PE header and the byte patterns) and to the real host and player state machines, the hold
   machine, the counters and the hook plan, with a tracking allocator that turns "no absurd allocations" into
   assertions. The review
   found, and the tests now pin: a clock of `i64::MAX` from a player overflowed the stamp arithmetic of the host
   (debug builds panic, release builds wrap to a negative stamp); `Opener` created a replay window for every sender
   id of a forged frame; a file of nested `{` overflowed the stack in the VDF reader. The tests themselves found:
   the default JSON float parser can be one unit in the last place off for some 17-digit doubles, so a host's
   Authority value could arrive changed (`float_roundtrip` is on now); `Pattern::matches_at` and `find_near` added
   an offset and a length without checking for overflow; and, on the hook side, `AppliedWatch::arm` took `usize::MAX`
   (the marker of a slot being filled) as a payload, which would have held the slot for good. Run the suites longer in
   a debug build, which is the one that checks arithmetic: `FEVERLINK3_FUZZ_SCALE=50 cargo test -p feverlink3_core
   --test robustness_tests` (and `-p feverlink3_launcher --test robustness`, `-p feverlink3_native --test robustness
   --test hook_robustness`); another seed with `FEVERLINK3_FUZZ_SEED=n`. Ten times the default, with four other seeds,
   ran clean on the merged tree.
4. A panic leaves its message in `%LOCALAPPDATA%\FeverLink3\native.log` before anything else happens
   (`native::log::install_panic_hook`), whatever the panic strategy.

**What can still panic, and why it cannot be reached:**

| Where | Why not reachable |
|---|---|
| `SessionKey::derive` (HKDF expand), `cipher()` (AES key), `proof_mac` (HMAC key) | Fixed lengths that the primitives accept for every value |
| `codec::encode` | Every message type serialises to JSON; no non-string map keys, and non-finite floats become `null` |
| `Sealer::seal`, `SessionKey::new_secret`, `FrameHeader::read` and `write` | The panicking forms for tests and tools. The state machines use `try_seal`, `try_new_secret`, `try_read`, and `Sealer::try_seal` also refuses instead of wrapping its counter |
| `steamnet::symbol` | The size check is made when the code is compiled |
| `update.rs`, `selftest.rs`, `main.rs` | The launcher executable, not the game process; an embedded constant and the self-test's thread joins |

**The unwind decision (reversed on 2026-10-11).** The profile was `panic = "abort"` for size. A real crash showed that it
cannot stay: the hooks sit between the game and its own C++ functions, and a C++ exception passing through a Rust
frame built with `abort` hits a terminate pad, even in a `system-unwind` function (`docs/NATIVE_LINK.md`, section 10).
The workspace profile is now `panic = "unwind"`. Measured on `x86_64-pc-windows-msvc`, rustc 1.99.0, `cargo build
--release -p feverlink3_native`, the same code: 1 132 544 bytes with `abort`, 1 662 464 with `unwind` (+529 920, +46.8 %;
the earlier measurement was 1 088 512 against 1 580 032). That is far over the 150 KB once allowed, and it is the price
of a DLL that lives inside another program's exception handling. CI prints both sizes in the job summary and runs
`tools/dll_unwind_check.py` on the shipped DLL.

What unwinding buys besides: every thread root that matters runs through `launcher::guard::guarded` (the controller's
step, the host and player session threads, the connection readers, each cycle of the shared Steam poller, the loopback
check; `DllMain` has its own `catch_unwind`), which now turns a panic into a log line, a stopped session
(`Controller::recover`, `Status::error`) and a controller that carries on; the hooks' bookkeeping and the service
thread's pump run through `native::log::guarded` and fail open (the game's call is made all the same). One loose end:
the window shows a player session's `Status::error`, but not yet that a host session's thread ended.

The roadmap's plan to shrink the DLL to tens of kilobytes with `no_std` has no unwinding at all, which makes the
removal of panics the only containment that carries over.

A stack overflow is not a panic and no setting contains it: recursion on outside input is bounded (JSON by
`serde_json` at 128 levels, the Lua text parser at 48, the VDF reader at 32, the join code and the codec do not
recurse).

## 6. Limits in one place

| What | Limit | Where |
|---|---|---|
| Frame (after the length field) | 1 MiB | `FrameCodec::MAX_FRAME_LENGTH` |
| First frame of a connection that has not joined | 4096 bytes | `FrameCodec::HANDSHAKE_MAX_LENGTH`, `NetLimits::first_frame` |
| Time to join; connections waiting to join | 10 s; 32 | `HostOptions::handshake_timeout_seconds`, `HostSession::MAX_PENDING` |
| Silence of a joined connection | 30 s (pings every 2 s) | `HostOptions::idle_timeout_seconds`, `ping_seconds` |
| Connections open; per address (TCP) | 64; 16 | `NetLimits`, `steam_transport::MAX_CONNECTIONS` |
| Time to write one frame (TCP) | 10 s, plus 1 s per 32 KiB of it | `NetLimits::write_timeout` |
| Rate per connection, every transport | 500 frames a second (burst 2000); 2 MiB a second (burst 8 MiB) | `HostOptions::max_frames_per_second`, `frame_burst`, `max_bytes_per_second`, `byte_burst` |
| Rate per connection, TCP reader | 1000 frames and 1 MiB per second, burst of two seconds | `NetLimits` |
| Steam message | 512 KiB, one frame each | `steamnet::MAX_MESSAGE` |
| Held slot of a player who left | 330 s; epochs per id 2^24 | `HostOptions::reconnect_window_seconds`, `Sealer::MAX_EPOCH` |
| Action body; chat text; display name | 480 KiB; 500 characters; 24 characters | `HostSession::MAX_ACTION_BODY`, `Names` |
| Checksum report | 128 parts, 128 characters each, 20 rounds kept | `ChecksumRounds` |
| Game clock | `0..2^50`, speed `0..16` | `PlayerClock::plausible` |
| Savegame | 2048 chunks of 256 KiB (512 MiB) | `ResyncLimits` |
| Join code | 2048 characters of text (the longest valid code is 976), 8 endpoints, 4 relays, 8 extensions | `JoinCode` |
| Relay message | 1 MiB + 16 bytes; read in steps of at most 256 KiB; error text 1024 bytes in, 200 out | `RelayCodec`, `RelayReader` |
| Native control link | line 8 KiB; 64 KiB read per poll; `hold_on` 16 kinds; `seq` 2^53 | `native::link` |
| Hold ring | 64 commands, 30 s each, 10 s of silence from the mod; 512 queued events; 6 error events per 10 s | `native::hold` |
| `native_events.log` | 8 MiB, then the module stops writing it | `native::link::MAX_EVENTS_BYTES` |
| Applied-command watch | 64 slots, 64 verdicts queued, entries expire after 60 s | `native::counters` |
| File-link line; bytes read per poll; `seq` | 4 MiB; 8 MiB; 2^53 | `gamelink` |
| Lua text | 4 MiB, 250 000 values, 48 levels | `luatext` |
| Log line; log lines kept; log-once memory | 2000 characters; 500; 1024 keys | `session`, `HostSession` |
| `native.log` | 2 MiB, then silent; a larger old one is set aside at start | `native::log` |
| Steam probe | 16 connections, 4096 bytes per message | `steamnet` |
| VDF depth | 32 | `steam` |

## 7. Findings of this review

| | Finding | Status |
|---|---|---|
| F1 | A mod `load` of an action body from another player runs calls and loops; one hostile action freezes every game | Fixed in the launcher (literal check). The mod's comment on `C.parse` ("no function calls") is wrong and the mod should not rely on the launcher alone: validate in Lua too, or `load` with an instruction-count hook |
| F2 | Handshake DoS: unlimited connections, no timeout before the Join, `vec![0; announced length]` for a stranger, a blocked reader thread that a close did not wake | Fixed (section 3) |
| F3 | A peer that stops reading blocks the session thread, which writes for everybody | Fixed (write timeout) |
| F4 | `Opener` made a window per forged sender id | Fixed |
| F5 | `dropped_once` grew with the peer's choices; Hello, ReplayFailed and the joiner's version went into logs and rejects unbounded | Fixed |
| F6 | Clock values from a player were trusted: a stamp in the far future, an overflow | Fixed |
| F7 | An unsealed Reject ended a joined session for good (no reconnect after a reject) | Fixed |
| F8 | A second Welcome would have replaced the sealer and restarted its counter | Fixed |
| F9 | `Tail` read whole files and copied an unfinished line on every poll; a `seq` of `u64::MAX` ended the numbering | Fixed |
| F10 | VDF parser recursion overflowed the stack: a file of 200 000 `{` ended the process (confirmed on the original code) | Fixed |
| F11 | The UPnP client followed any URL a LAN device gave (and public-address lookups went over plain HTTP) | Gone: the product no longer does port forwarding or address lookups (`upnp.rs` was deleted on `main`) |
| F12 | serde_json's default float parser can be 1 ulp off | Fixed (`float_roundtrip`) |
| F13 | A recorded connection can be replayed into a new one by someone on its path (section 4). `main` closed the half of it that needs only a recorded Welcome (the Welcome carries the challenge's nonce, host frames from before it are refused); a recorded Challenge together with its Welcome still works | **Open: protocol version 2, owner's decision** |
| F14 | Hot-join resync rounds ignore the cooldown: a code holder can disrupt a session by joining repeatedly, and a table full of away strangers locks friends out for 330 s | Mitigated: the host can kick (and a kicked player is gone for good); the hot-join round is still not rate limited |
| F15 | The window's commands (with the password) stay in `ctl_cmd.log` until the next game start | Accepted; the folder is the user's own |
| F16 | Found while fixing F2: the first fix used `SO_RCVTIMEO` to wake the reader, and on Windows a read that times out as data arrives can fail with `ERROR_IO_PENDING` (997), which ends a healthy connection (measured: twice in about 6400 timeouts, in four runs of 30 to 60 s with data landing as the timeout expired) | Fixed before it shipped: non-blocking sockets, polled; a test sends 1200 frames of up to 300 KB both ways at once and checks every byte |
| F17 | `AppliedWatch::arm` accepted `usize::MAX`, the marker of a slot being filled, as a payload: the slot stayed taken for good and, 64 of those later, the watch would have refused every released command (the payload is a pointer from the engine, so this cannot happen with real pointers, but a hook must not rely on that) | Fixed; found by the model test |
| F18 | `to_game.log` is append-only for the whole session and never rotated: tens of megabytes an hour in play, and a hostile joiner adds up to 2 MiB a second (the byte budget) of relayed actions to the host's disk | Open: needs a rotation both sides of the file link agree on (a change in the mod); the rate limit bounds it meanwhile |
| F19 | The Steam transport (`main`, after the review began) had no byte throttle: the frame limit alone allows 256 KiB actions at 500 a second, 128 MB/s into the host's disk and the relays to every game | Fixed: a byte budget per connection in the host state machine, so it holds for every transport |
| F20 | The Steam probe kept every connection that was opened to it, and its counters could overflow | Fixed: 16 at a time, counters saturate; the Steam poller thread runs each cycle through `guard` |

Not found: timing differences in the proof check (constant time), id spoofing (the sender is checked against the
connection before the frame is opened and again against the allow-list), nonce reuse across reconnects (a returning
id gets a new epoch, whose counters start above everything the old one can reach; guarded by a test that collects
every `(sender, counter)` of a scenario with 40 joins and leaves, and by `reconnect_tests`), timing differences in the
resume claim (constant time), a stranger taking a held slot (the claim needs the token),
replay-window edge cases (a model of the rules is compared with the implementation on random sequences that include
0, `u64::MAX` and the 64-frame boundary).

## 8. Reporting

Open an issue on GitHub for anything that is not exploitable, or message `azalshehri` on Discord for something that
is. Please include the version (`docs/RELEASING.md`) and, for a crash, `native.log`.
