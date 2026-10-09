# Connecting players without port forwarding

Research note, 2026-10-10. No production code. Tags: **[V]** read in the cited public source; **[I]** my inference or estimate; **[U]** unverified (secondary source, or the original was unreadable). Sources S1 to S31 are listed at the end.

## Decision

Owner policy: **no port forwarding in any form, UPnP not even as a requirement.** Ranked by "works with zero router configuration":

1. **Steam P2P / Steam Datagram Relay first.** Free, no server of ours, no router setup, being proven. Valve's API "automatically establishes NAT-traversing or Steam relay server connections" [V, S6] and, for peer-to-peer, "Steam will take care of everything else" [V, S8]. It is also all that Seamless Co-op has.
2. **Our own outbound-only TCP relay second, built before any hole punching.** The only option that works behind every NAT, carrier-grade NAT included, and it reuses the sealed TCP framing unchanged: the relay forwards ciphertext it cannot read. About **EUR 4-5 per month**, about **500 lines of Rust** for the server plus 300 in the launcher [I].
3. **UDP hole punching with our own rendezvous server last**, to save relay bandwidth only. Published success is 64 to 82 percent [V, S16, S17], so it cannot replace the relay.
4. VPN overlays (Tailscale, ZeroTier, Radmin): a manual escape hatch, no code.
5. Endpoints already in the join code (LAN, VPN, IPv6, public IPv4) stay as free parallel candidates. UPnP, NAT-PMP and PCP: demote to a silent bonus or delete `launcher/src/upnp.rs` (owner's call).

**When Steam's own path fails, Seamless Co-op does nothing that I could find** (section 1).

## 1. Elden Ring Seamless Co-op (ersc)

Limits: the Nexus page returned HTTP 403 [S5]; the mod is closed source and I touched no binary. Evidence: the mod's FAQ, its 2.0 release notes reposted on Steam forums, Valve's docs, and unseamless-coop, a third-party Rust re-implementation that publishes its own observations.

- **Transport.** Steam networking, no server run by the mod: it "doesn't connect to the matchmaking servers and does not use FromSoftware's online services" [V, S2]. The game's co-op transport is Steam P2P addressed by SteamID64 (legacy `ISteamNetworking`); unseamless-coop reports an ersc session as "real Steam P2P, keyed by SteamID64", joined by password with no Steam friendship (PC host, Steam Deck joiner) [V as their observation, S4]. When direct traversal fails, Steam relays [V, S6]. ersc's error text `k_ESteamNetworkingAvailability_Failed` is a Valve status value, so the mod seems to need Steam's relay network reachable [I, S1, S3].
- **Discovery.** The host uses an in-game item (Tiny Great Pot), joiners another (Effigy of Malenia); same password in the .ini, same game and mod version; "Everyone must be connected to Steam's Friends network" [V, S1]. How the password becomes a lookup (lobby metadata, hash) is **not documented anywhere I could read** [U]. unseamless-coop guesses a password-derived Steam-lobby key plus a hashed proof; that is its own design, not a finding about ersc [V, S4].
- **Ports and NAT.** The FAQ never mentions ports, NAT type or forwarding [V, S1]. Its one network setting: Steam, Settings, In-Game, Steam Networking = "Always", which "can improve the connection quality by sharing your IP directly" [V, S1]. Valve: NAT traversal "is not firewall traversal"; outbound UDP 3478, 4379 and 4380 must be open [V, S6].
- **Strict NAT, and when Steam fails.** Valve's relay absorbs strict NAT (more ping, still connected) [I]. If Steam itself fails, ersc shows an error and stops: no direct-IP mode, no own relay, no fallback documented [V for S1-S3; U for the closed Nexus text and the Discord].
- **Known failures and fixes.** Mostly not NAT: one unique Steam copy per player, same versions and password (restart after editing), Steam online, anti-cheat such as Vanguard off, firewall and antivirus allow-listing, do not block players on Steam, restart the router, Ethernet over Wi-Fi, VPN on or off, "Steam timed out" (Steam's servers, Tuesday maintenance) [V, S1]. The July 2024 1.7.8 relay-error thread has no reply from the author [V, S3]. I found **no ersc-specific advice to use Hamachi, Radmin or ZeroTier**.
- **Lesson.** Delegating rendezvous and NAT to Valve costs the mod nothing to run, but every failure is then outside its control; the FAQ is dominated by firewall, antivirus and Steam problems, not NAT.

## 2. Three other layers

**Factorio** (lockstep over UDP 34197, closest to our model). Behind NAT "you must set up port forwarding" or use a VPN such as Hamachi [V, S10]. Since 0.13, two "pingpong" helper servers tell the game server its public address (trusted only if both agree), it keeps a keepalive open, and a client's request travels via pingpong so the server's first outbound packet opens its NAT: "a specialised version of STUN and ICE", expected success "about 70%", no relay fallback described [V, S11]. Lesson: a tiny rendezvous is cheap but strands about 30 percent.

**PlayFab Party** (under Valheim's crossplay join codes). Party "will automatically leverage a transparent cloud relay service"; direct links are optional, best effort, fall back to the relay, and are discouraged because they disclose IP addresses [V, S12]. Valheim's six-digit code comes from PlayFab at server start [U, hosting guides, S13]. Lesson: relay first, direct optional, a short code resolved by a backend.

**Tailscale and iroh.** Tailscale: "all connections start out with DERP preselected" and upgrade within seconds; two hard NATs have a 0.01 percent chance in 20 s of probing; carrier-grade NAT defeats port mapping but not hole punching [V, S14]. iroh: "roughly 9 out of 10 network configurations allow a direct connection", otherwise the relay "simply forwards encrypted packets" (figure not measured [U]) [V, S15]. Lesson: start on the relay, upgrade in the background, so nobody waits for a timeout.

## 3. Options compared

| Rank | Option | Zero router config | Success (published) | We run | Rust crates, MSVC build, licence, size | Effort |
|---|---|---|---|---|---|---|
| 1 | **Steam P2P / SDR** (`ConnectP2P` through the game's `steam_api64.dll`) | yes | No rate published by Valve [U]. Relay fallback built in [V, S6]. ersc's FAQ failures are Steam, firewall and block-list problems, not NAT [V, S1] | nothing (Valve's) | None: flat API looked up in the loaded DLL (`steamnet.rs`). One message at most 512 KiB; only owners of the app can connect [V, S7, S9] | probe done; glue medium |
| 2 | **Own TCP relay**, outbound-only, token-paired | yes | About 100% where outbound TCP works (use port 443) [I] | 1 VPS, EUR 4-5/month | Client: std only. Server: std threads or Tokio; `bore` is about 400 lines, MIT, 0.67 MB zipped MSVC release [V, S25] | low, about 800 lines [I] |
| 3 | **UDP hole punching + our rendezvous** (STUN-style) | yes | UDP 82% of 380 NATs, TCP 64% [V, S16]. 70% +/- 7.1 of 4.4 M attempts, TCP equals QUIC [V, S17]. Symmetric NAT about 40% [V, S18]. Factorio expects about 70% [V, S11] | same VPS (UDP echo + mailbox), or free public STUN | STUN client by hand (RFC 8489, about 150 lines [I]); reliable layer `rusty_enet` 0.4 (MIT, one dependency) or `kcp` 0.6 (MIT) [V, S26]; no C code | medium, 1,000-1,500 lines [I] |
| 4 | **ICE / STUN / TURN stack** | yes | Row 3 plus TURN: relayed pairs always connect [I] | Free STUN; TURN via coturn on the VPS, or Cloudflare ($0.05/GB after 1,000 GB, UDP relay only) [V, S23] | `libjuice`: C, MPL-2.0, no dependencies, NMake/MSVC build, Rust binding `juice-rs`, size unpublished [V, S27]. `webrtc-ice` 0.17: MIT/Apache, Tokio, pulls `turn` and `ring` (C) [V, S26]. `str0m` 0.24: sans-I/O, no runtime, ICE yes, **no TURN**, `wincrypto` backend uses Windows CNG [V, S28] | medium, heavy dependencies |
| 5 | **QUIC (`quinn`) + hole punching, or `iroh`** | yes | iroh: "roughly 9 out of 10" configurations direct [V claim, not measured, S15]. libp2p: QUIC about equal to TCP, about 70% [V, S17] | same VPS, or `iroh-relay` (MIT/Apache) | `quinn` 0.11 (MIT/Apache, Tokio) defaults to rustls + `ring`: MSVC C compiler, assembly pre-generated, no NASM [V, S26, S29]; the `aws-lc-rs` option needs a C compiler plus NASM or its prebuilt objects [V, S30]. `iroh` 1.3: 40 direct dependencies incl. Tokio, reqwest, rustls [V, S26]. Tokio tools ship at 0.6-1.6 MB zipped, so expect +1-3 MB in the DLL [I]. QUIC's TLS duplicates our AES-GCM sealing | medium, biggest footprint |
| 6 | **VPN overlay** (Tailscale, ZeroTier, Radmin, Hamachi) | yes, but every player installs an app | Tailscale's author estimates over 90% direct, relay otherwise (estimate, not measured) [V, S14] | nothing. Free: Tailscale 6 users, ZeroTier 10 devices [V, S31] | None. The join code already lists VPN addresses | docs only |
| 7 | **IPv6 direct** | partly: RFC 6092 tells home gateways to drop unsolicited inbound traffic, so a punch is still needed [V, S21] | Needs IPv6 at both ends. Saudi Arabia 36% in 2021 [V, S20] | nothing | std only; codes already carry IPv6 | none |
| 8 | **Direct TCP + UPnP / NAT-PMP / PCP** | **no** (owner policy) | Carrier-grade NAT in over 90% of cellular ASes and 17-18% of non-cellular eyeball ASes [V, S19]; CGN ISPs tend to switch port mapping off [V, S14] | nothing | `igd-next` 0.18 (MIT), `natpmp` 0.5, `crab_nat` 0.8 (MIT) [V, S26] | exists (`upnp.rs`); demote |

Hole-punching figures: Ford et al. 2005 used a self-selected sample [S16]; the 2025 libp2p study counts only attempts that reached the punch stage (the IMC version reportedly drops about 29 percent of records, mostly failed relay reservation or address discovery [U, second-hand]) [S17]; per class, port-restricted cone 82.9% (n=3,669) and symmetric 39.7% (n=536, "in doubt") [S18]. Tailscale's "over 90%" is its author's estimate. No source I found measures Saudi ISPs' carrier-grade NAT; assume the worst [U].

## 4. Recommended chain, build order, infrastructure

**Chain.** Race the paths; do not wait for failures (RFC 8305 staggering [S22]).

1. t = 0 s, in parallel, zero setup: (a) Steam `ConnectP2P` to the host's SteamID (a new optional join-code field), cut off at 8 s (Valve's legacy API reports failure only after 20 s [S6]); (b) the code's endpoints (LAN, VPN, IPv6, IPv4), 250 ms apart (RFC 8305's recommended delay), 2.5 s each; this works only if the host happens to be reachable.
2. t = 3 s: also dial the relay (5 s to connect, 5 s for the handshake) without cancelling the others.
3. The first path to finish the sealed Challenge/Join handshake wins (Steam preferred if within 500 ms). Typical join 1 to 3 s, worst case about 10 s; the handshake proves the endpoint is the host.
4. All failed: name the failing step and point to the VPN escape hatch.
5. Host: Steam virtual port and LAN TCP as today, plus **one outbound control connection** to the relay under a room id; per joiner the relay signals and the host opens one more outbound connection (the `bore` pattern [S25]). No inbound port, so no firewall prompt.
6. Later: after joining on the relay, try a hole punch in the background and switch at a frame boundary.

Relay latency is acceptable [I, from `docs/PROTOCOL.md`]: lead is `200 x (4 + 2 x speed)` game-time units and 1000 units pass per second at 1x, so 1.2 s at 1x and 0.6 s of real time at 4x. A relay round trip under about 300 ms does not stall lockstep.

**Build first.** (1) A transport seam in `launcher/src/net.rs`, candidate list and racer, about 150 lines [I]. (2) `feverlink3-relay` plus the client connector, about 500 + 300 lines [I], no new client crate; the server may adapt `bore` (MIT; add its notice to `THIRD_PARTY_NOTICES.md`). (3) The Steam transport (orchestrator); design for Valve's limits: messages of at most 512 KiB, so 350 KB resync chunks fit but the 1 MiB frame ceiling does not [V, S9], and the host must answer an incoming attempt within "a second or two" [V, S7]. (4) Measure which path wins; only then hole punching (rendezvous about 300 lines, client about 1,000, plus `rusty_enet` or `kcp`) [I].

**Smallest infrastructure.** One small VPS, one binary, no database: a TCP relay on port 443 that pairs two outbound connections by room id and copies bytes; no TLS, because frames are sealed. Room id = `HKDF(secret, "relay room")`, so the relay cannot derive the session key; pairing needs a MAC proof; per-room byte and idle limits prevent open-proxy abuse [I]. A later UDP echo and mailbox for punching lives in the same process, or use free public STUN (`stun.cloudflare.com` [V, S23]).

- **Cost: about EUR 4-5 per month (US$5).** Hetzner entry plans include 20 TB of EU traffic [V, S24]; price trackers show EUR 3.99 plus 0.50 for IPv4 after April 2026, but the official page listed no prices [U, S24]. A hole-punching rendezvous runs on the same server, so cost does not choose between them; effort and success rate do.
- **Traffic.** Commands and clocks are tiny; resync saves dominate: 500 MB to each of 3 players is 1.5 GB per round, so 20 TB covers about 13,000 rounds a month [I]. Saudi-to-EU latency is probably 100-150 ms [U].
- **Zero-server alternative.** Cloudflare TURN (row 4) needs a Worker to mint credentials plus an ICE client in the DLL: more code than the VPS relay, so not first.

**Not verified.** Valve publishes no P2P or SDR success rate; whether TF3's app id gets SDR for P2P is for the probe to show. Unread: the Nexus text, the ersc Discord, ersc's password-to-lobby scheme, Steam's help page on the Steam Networking setting. Estimates: Hetzner price, Saudi carrier-grade NAT, size increases, all line counts.

## Sources

ersc and Steam
- S1 ersc FAQ: https://ersc-docs.github.io/faq/
- S2 Seamless Co-op 2.0 release notes repost (Steam forums): https://steamcommunity.com/app/1245620/discussions/0/3395163747104259416/
- S3 "1.7.8 Steam datagram relay error" thread: https://steamcommunity.com/app/1245620/discussions/0/4406291873012987095
- S4 unseamless-coop (README; docs `COOP-CONNECTION.md`, `ERSC-LIVE-CAPTURE-FINDINGS.md`, `FROMNET-LINK-FINDINGS.md`, `ARCHITECTURE.md`): https://github.com/micthiesen/unseamless-coop
- S5 Nexus page (HTTP 403, not read): https://www.nexusmods.com/eldenring/mods/510
- S6 ISteamNetworking (legacy P2P): https://partner.steamgames.com/doc/api/ISteamNetworking
- S7 ISteamNetworkingSockets: https://partner.steamgames.com/doc/api/ISteamNetworkingSockets
- S8 Steam Datagram Relay: https://partner.steamgames.com/doc/features/multiplayer/steamdatagramrelay
- S9 GameNetworkingSockets and `steamnetworkingtypes.h`: https://github.com/ValveSoftware/GameNetworkingSockets

Other layers
- S10 Factorio wiki, Multiplayer: https://wiki.factorio.com/Multiplayer
- S11 Factorio FFF-143: https://factorio.com/blog/post/fff-143
- S12 PlayFab Party direct peer connectivity: https://learn.microsoft.com/en-us/gaming/playfab/features/multiplayer/networking/concepts-direct-peer-connectivity
- S13 Valheim join codes (secondary): https://low.ms/blog/valheim-crossplay-join-codes and https://pixelwebdesign.se/portfolio/CorporateWebsites/piktiv/case/valheim/index.html
- S14 Tailscale, how NAT traversal works: https://tailscale.com/blog/how-nat-traversal-works
- S15 iroh NAT traversal: https://docs.iroh.computer/concepts/nat-traversal

Numbers and standards
- S16 Ford, Srisuresh, Kegel 2005: https://bford.info/pub/net/p2pnat/
- S17 libp2p DCUtR measurement 2025 (arXiv 2510.27500): https://arxiv.org/abs/2510.27500
- S18 ProbeLab, per NAT class: https://probelab.io/blog/can-libp2p-punch-through-nats/
- S19 Richter et al., carrier-grade NAT (IMC 2016): https://ar5iv.labs.arxiv.org/html/1605.05606
- S20 APNIC, Saudi Arabia IPv6: https://blog.apnic.net/2021/06/15/understanding-saudi-arabias-ipv6-boom/
- S21 RFC 6092, IPv6 home gateway security: https://www.rfc-editor.org/rfc/rfc6092.html
- S22 RFC 8305, Happy Eyeballs v2: https://www.rfc-editor.org/rfc/rfc8305.html

Infrastructure and crates
- S23 Cloudflare TURN and STUN: https://developers.cloudflare.com/realtime/turn/faq/
- S24 Hetzner plans: https://www.hetzner.com/cloud/regular-performance/ ; price trackers: https://www.vpsbenchmarks.com/hosters/hetzner/plans/cx23 and https://costgoat.com/pricing/hetzner
- S25 `bore` https://github.com/ekzhang/bore (release v0.6.0 asset sizes via the GitHub API); `rathole` https://github.com/rathole-org/rathole (v0.5.0)
- S26 crates.io metadata, queried 2026-10-10: https://crates.io/crates/quinn , quinn-proto, str0m, webrtc-ice, rusty_enet, kcp, iroh, igd-next, natpmp, crab_nat
- S27 libjuice: https://github.com/paullouisageneau/libjuice
- S28 str0m: https://github.com/algesten/str0m
- S29 ring build notes: https://github.com/briansmith/ring/blob/main/BUILDING.md
- S30 aws-lc-rs on Windows: https://aws.github.io/aws-lc-rs/requirements/windows.html
- S31 Tailscale pricing https://tailscale.com/pricing ; ZeroTier pricing https://www.zerotier.com/pricing/
