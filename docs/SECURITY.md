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
| The join code and the lobby password | The code carries the 16-byte session secret and the host's addresses. Whoever holds it (and the password, if one is set) can join. |
| The player's files and PC | The mod writes savegames received from the host into the save folder; the launcher installs a DLL into the game folder and updates itself. The host opens a TCP port to the internet (UPnP). |
| Privacy | IP addresses, display names, what the players build. |

## 2. Attackers

| Attacker | Can | Cannot |
|---|---|---|
| **A. Stranger** who finds the host's port (a scan, a leaked address) | Open connections, send bytes, never complete a handshake | Make a game do anything: every frame past the handshake is sealed with a key derived from the code |
| **B. Network observer or relay** (ISP, Wi-Fi, a relay server) | Read headers (kind, sender, counter, length), drop, delay, reorder, cut connections, inject an unsealed `Reject` | Read or alter a payload, forge a frame, replay a frame inside a connection |
| **C. Joiner** with the code (and the password) | Send every message kind a player may send (section 3), with any content, as fast as the host will read | Pass as the host or another player, change the session state or permissions, send a host-only kind |
| **D. Malicious host**, seen by a joiner | Everything: it decides the state, relays the actions, sends the savegame, and a savegame runs code when it loads | (nothing is contained: section 4) |
| **E. Device on the LAN** | Answer the UPnP discovery with URLs of its choosing | Reach beyond the private network through the launcher (section 3, row "UPnP") |
| **F. Steam user** who knows an account's Steam ID | Open a Steam connection to the probe and send `ping` | Reach the session (the probe carries none) |
| **G. Same-user program** | Anything the user can do, including the mod's files and the game folder | (out of scope) |

## 3. What is protected, and how

| Threat | Mechanism | Test |
|---|---|---|
| Joining without the code | The key is `HKDF(secret [‖ PBKDF2(password)])`. A Join carries `HMAC(key, nonce ‖ name)` over a per-connection random nonce; the check is constant time (`verify_slice`). A password adds 600 000 PBKDF2 iterations for anyone who has the code but not the password. | `session_tests`, `sealed_channel_tests` |
| Reading or altering a frame | AES-256-GCM; the 12 header bytes are the additional data, so kind, sender and counter cannot change either | `sealed_channel_tests`, `robustness_tests::the_opener_accepts_exactly_what_a_sealer_made...` |
| Replaying a frame | 64-frame replay window per sender; a counter is recorded only after the tag verified, and a sender gets a window only then, so forged frames can neither burn a counter nor grow the opener | `sealed_channel_tests`, `wire_limits_tests::forged_frames_leave_the_opener_empty`, `robustness_tests::the_replay_window_agrees_with_a_model...` |
| Nonce reuse | Sender ids are assigned once and never taken back, a reconnecting player is a new sender, every session has a new secret, a `Sealer` that would run out of counters refuses instead of wrapping, and a second Welcome cannot replace a player's sealer | `hardening_tests::no_nonce_is_used_twice...`, `a_second_welcome_does_not_restart...`, `wire_limits_tests::the_sealer_refuses...` |
| Speaking as another player or the host | The host checks the frame's sender id against the connection before it opens the frame; it fills `from`, `name`, `category` and `allowed` itself; a player may send only kinds 30 to 36, 50 and 51 | `session_tests`, `hardening_tests`, `robustness_tests::a_hostile_joined_player...` |
| A stranger holding the host's resources | 64 connections, 16 per address, 32 waiting to join at once, a join within 15 s, a first frame of at most 4 KiB, a write that must finish in 10 s plus 1 s per 32 KiB, sockets that are polled so that a close takes effect in 5 ms (a blocked read on a silent peer outlived every shutdown, and a read with a timeout failed with `ERROR_IO_PENDING` about once in 3000 timeouts when data arrived as it expired: both measured) | `net::tests`, `hardening_tests::the_host_closes_connections_that_never_join...` |
| A player flooding the host | 1000 frames and 1 MiB per second per connection (the reader waits, TCP slows the sender); messages that are too big are dropped: Action body 256 KiB, checksum report 128 parts of 128 characters | `net::tests::a_peer_that_sends_faster...`, `hardening_tests` |
| A player lying about time | A Clock outside `0..=2^50`, or with a speed outside `0..=16`, is dropped; the stamp arithmetic saturates | `hardening_tests::clocks_that_are_not_game_times_are_dropped` |
| Memory that grows with what a peer chooses | Log-once memory is cleared at 1024 keys and keyed by reasons, not by the peer's values; peer text is clipped before it reaches a log line (120 to 2000 characters); the host keeps 20 checksum rounds | `hardening_tests::the_log_does_not_grow...`, `robustness_tests` (tracking allocator) |
| An injected `Reject` ending a session | A Reject is unsealed, so a player honours it only while it is still joining | `hardening_tests::an_injected_reject...` |
| Code in an action body | The mod `load`s the body in an empty environment; that still runs calls and loops (`("x"):rep(1e9)`, `while true do end`). The launcher gives the game an action only if its body is a Lua *literal* (tables, strings, numbers, booleans, nil; the mod's NaN and infinity spellings) | `luatext::tests::a_literal_is_what_the_mods_serialiser_writes...`, `session::tests::lines_translate_both_ways`, `robustness::an_action_body_reaches_the_game_only_as_a_literal` |
| A file the game writes | The tail reads at most 8 MiB per poll, drops a line over 4 MiB whole, takes `seq` only up to 2^53, and reads invalid UTF-8 as replacement characters; the Lua text parser stops at 4 MiB, 250 000 values and 48 levels | `gamelink::tests`, `robustness` |
| A device answering UPnP discovery | URLs from the router must be `http://` to a private IPv4 address (no names, no users, no loopback), contain no space or control character, and one HTTP exchange takes at most 10 s and 256 KiB | `upnp::tests`, `robustness::router_answers_of_any_shape...` |
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
- **Frames of a recorded connection can be replayed into a new connection by someone on its path.** Sealed frames
  are bound to the session key, not to the connection: a player's fresh state has an empty replay window, so a
  recorded Welcome (which carries a sender id) or an old host frame is accepted again when an attacker who controls
  the new connection (a relay, a man in the middle) feeds it in. The attacker still cannot read or forge anything;
  what it can do is make a player believe an old session state, and, with a replayed Welcome, make that player seal
  with a nonce it used before, which exposes the XOR of two of that player's messages to whoever recorded both.
  Closing it needs one change to the handshake: the player contributes a random value that goes into the Join and
  into the key of that connection (protocol version 2). It is deferred on purpose, because the roster and relay work
  changes the same code; see F13 in section 7.
- **The host is trusted by its players.** The host decides the session state, orders every action, can send any
  action as any player (`from` is its to fill), and sends the savegame that every game loads, and a savegame is code
  (`.sav.lua` runs on load). Joining a session is trusting its host as one trusts the author of a mod.
- **A joiner with the code can send every allowed kind.** It can flood within the rates above, send actions that the
  permissions mark `allowed: false` (every game then asks for a resync), report checksums that differ (a resync after
  two rounds), send `ReplayFailed` (a resync, at most one per 120 s), request speed changes when the `speed`
  permission allows it, and join and leave again to start a *hot-join* resync round each time (that round ignores the
  cooldown), or to use up the 65 534 sender ids of a session. The remedy is to share the code with people you trust,
  and to stop the session when one of them is not.
- **No forward secrecy:** the key lives as long as the session. A secret is used for one session only; it is made
  fresh for every `host`.
- **The lobby password and the join code pass through files** in `%LOCALAPPDATA%\FeverLink3\live` (the window's
  commands); they stay there until the next game start. The folder is the user's own.
- **A program running as the same user** can do everything the mod can, and more.
- **Public-address lookups go out over plain HTTP** to three services, to put the right address in the join code. A
  man in the middle can make the code point nowhere; it learns nothing it did not know.

## 5. The game process: panics, stacks and the unwind decision

`TransportFever3.exe` loads `winhttp.dll` (the native module), which starts the session controller on a thread, and
the controller starts a host or player session with a thread per connection. Everything on those threads is Rust
reading bytes that come from outside: frames, join codes, the mod's files, the router, Steam's structures.

**Policy: a panic reachable from outside input is a bug that is fixed, not contained.** What was done:

1. Every `unwrap`, `expect`, `panic!`, index, slice, `as` conversion, arithmetic assignment and lock on a path
   reachable from `DllMain`, the controller, the session threads, `steamnet` and `upnp` was read. What can still
   panic is in the table below; none of it depends on input.
2. Mutexes are taken with `unwrap_or_else(PoisonError::into_inner)`, so a thread that died holding a lock does not
   take the others with it.
3. Randomised tests (`core/tests/robustness_tests.rs`, `launcher/tests/robustness.rs`,
   `native/tests/robustness.rs`) feed random, mutated and oversized input to every parser and to the real host and
   player state machines, with a tracking allocator that turns "no absurd allocations" into assertions. The review
   found, and the tests now pin: a clock of `i64::MAX` from a player overflowed the stamp arithmetic of the host
   (debug builds panic, release builds wrap to a negative stamp); `Opener` created a replay window for every sender
   id of a forged frame; a file of nested `{` overflowed the stack in the VDF reader. The tests themselves found:
   the default JSON float parser can be one unit in the last place off for some 17-digit doubles, so a host's
   Authority value could arrive changed (`float_roundtrip` is on now); `Pattern::matches_at` and `find_near` added
   an offset and a length without checking for overflow. Run the suites longer in a debug build, which is the one
   that checks arithmetic: `FEVERLINK3_FUZZ_SCALE=50 cargo test -p feverlink3_core --test robustness_tests` (and
   `-p feverlink3_launcher --test robustness`, `-p feverlink3_native --test robustness`); another seed with
   `FEVERLINK3_FUZZ_SEED=n`. Ten to fifteen times the default, with two other seeds, ran clean.
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

**The unwind measurement.** Cargo's `panic` setting belongs to a profile, not to a crate, and it is not allowed in a
per-package override, so trying it for the DLL alone needs either a dedicated profile (and the packaging in
`tools/release` and `launcher/src/install.rs` pointed at its output folder) or a change of the workspace's release
profile. The workspace one is simpler, and affects only the launcher executable's size besides. Measured on
`x86_64-pc-windows-msvc`, rustc 1.99.0, `cargo build --release -p feverlink3_native`:

| | `panic = "abort"` | `panic = "unwind"` | difference |
|---|---:|---:|---:|
| Before this review | 791 040 bytes | 1 078 272 bytes | +287 232 (+36.3 %) |
| After this review | 819 200 bytes | 1 116 672 bytes | +297 472 (+36.3 %) |

The `.text` section grows by 130 KB (landing pads for every destructor, and the C++ exception runtime that
`panic_unwind` links), `.rdata` by 134 KB (exception tables), `.pdata` by 22 KB. That is about twice the 150 KB the
owner allowed, so **the profile stays `panic = "abort"`**. The hardening itself cost 28 KB (the exact float parser,
the checks, the hook). CI prints both sizes in the job summary.

What was done instead: the panics that input can reach were removed (above); every thread root that matters runs
through `launcher::guard::guarded` (the controller's step, the host and player session threads; `DllMain` has its
own `catch_unwind`), which does nothing under `abort` and, with `unwind`, turns a panic into a log line, a stopped
session (`Controller::recover`, `Status::error`) and a controller that carries on; it is tested, because tests
always unwind. Switching to unwind is one line (`panic = "unwind"` in the root `Cargo.toml`); the cost is the table
above. One loose end for that day: the window shows a player session's `Status::error`, but not yet that a host
session's thread ended.

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
| Time to join; connections waiting to join | 15 s; 32 | `HostSession::HANDSHAKE_SECONDS`, `MAX_PENDING` |
| Connections open; per address | 64; 16 | `NetLimits` |
| Time to write one frame | 10 s, plus 1 s per 32 KiB of it | `NetLimits::write_timeout` |
| Rate per connection | 1000 frames and 1 MiB per second, burst of two seconds | `NetLimits` |
| Action body; chat text; display name | 256 KiB; 500 characters; 24 characters | `HostSession::MAX_ACTION_BODY`, `Names` |
| Checksum report | 128 parts, 128 characters each, 20 rounds kept | `ChecksumRounds` |
| Game clock | `0..2^50`, speed `0..16` | `PlayerClock::plausible` |
| Savegame | 2048 chunks of 256 KiB (512 MiB) | `ResyncLimits` |
| Join code | 1024 characters of text, 8 endpoints | `JoinCode` |
| File-link line; bytes read per poll; `seq` | 4 MiB; 8 MiB; 2^53 | `gamelink` |
| Lua text | 4 MiB, 250 000 values, 48 levels | `luatext` |
| Log line; log lines kept; log-once memory | 2000 characters; 500; 1024 keys | `session`, `HostSession` |
| `native.log` | 2 MiB, then silent; a larger old one is set aside at start | `native::log` |
| UPnP exchange | 10 s, 256 KiB, private IPv4 URLs only | `upnp` |
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
| F11 | UPnP followed any URL a LAN device gave | Fixed |
| F12 | serde_json's default float parser can be 1 ulp off | Fixed (`float_roundtrip`) |
| F13 | Frames and a Welcome can be replayed into a new connection by someone on its path (section 4) | **Open: protocol version 2, owner's decision** |
| F14 | Hot-join resync rounds ignore the cooldown, and ids are not recycled: a code holder can disrupt a session by joining repeatedly | Accepted; the host needs a way to remove a player first |
| F15 | The window's commands (with the password) stay in `ctl_cmd.log` until the next game start | Accepted; the folder is the user's own |
| F16 | Found while fixing F2: the first fix used `SO_RCVTIMEO` to wake the reader, and on Windows a read that times out as data arrives can fail with `ERROR_IO_PENDING` (997), which ends a healthy connection (measured: twice in about 6400 timeouts, in four runs of 30 to 60 s with data landing as the timeout expired) | Fixed before it shipped: non-blocking sockets, polled; a test sends 1200 frames of up to 300 KB both ways at once and checks every byte |
| F17 | `to_game.log` is append-only for the whole session and never rotated: tens of megabytes an hour in play, and a hostile joiner adds up to 1 MiB a second (the connection rate) of relayed actions to the host's disk | Open: needs a rotation both sides of the file link agree on (a change in the mod); the rate limit bounds it meanwhile |

Not found: timing differences in the proof check (constant time), id spoofing (the sender is checked against the
connection before the frame is opened and again against the allow-list), nonce reuse across reconnects (ids are
never reused; guarded by a test that collects every `(sender, counter)` of a scenario with 40 joins and leaves),
replay-window edge cases (a model of the rules is compared with the implementation on random sequences that include
0, `u64::MAX` and the 64-frame boundary).

## 8. Reporting

Open an issue on GitHub for anything that is not exploitable, or message `azalshehri` on Discord for something that
is. Please include the version (`docs/RELEASING.md`) and, for a crash, `native.log`.
