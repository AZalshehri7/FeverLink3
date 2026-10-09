using System.Text;
using FeverLink3.Crypto;
using FeverLink3.Protocol;
using FeverLink3.Session;
using Xunit;

namespace FeverLink3.Tests;

/// <summary>A host and players wired together in memory, with a clock the test moves.</summary>
sealed class Harness : IDisposable
{
    public static readonly byte[] Secret = Enumerable.Range(7, 16).Select(i => (byte)i).ToArray();
    public readonly SessionKey Key = SessionKey.Derive(Secret);
    public double Now;
    public readonly HostSession Host;
    public readonly List<(MessageKind Kind, byte[] Payload)> HostGame = new();
    public readonly List<string> HostLog = new();
    public readonly List<int> Closed = new();
    public readonly List<ChecksumOutcome> Outcomes = new();
    readonly Dictionary<int, PlayerSession> byConnection = new();
    int nextConnection;

    public Harness(string hostName = "Hote", HostOptions? options = null)
    {
        Host = new HostSession(Key, hostName, () => Now, options);
        Host.SendToConnection += (c, f) => { if (byConnection.TryGetValue(c, out var p)) p.FrameReceived(f); };
        Host.CloseConnection += c => { Closed.Add(c); byConnection.Remove(c); };
        Host.DeliverToLocalGame += (k, p) => HostGame.Add((k, p));
        Host.Log += HostLog.Add;
        Host.ChecksumCompleted += Outcomes.Add;
    }

    public sealed class Player
    {
        public PlayerSession Session = null!;
        public int Connection;
        public readonly List<(MessageKind Kind, byte[] Payload)> Game = new();
        public readonly List<string> Log = new();
        public RejectMessage? Reject;
        public T Last<T>(MessageKind kind) where T : class => Codec.TryDecode<T>(Game.Last(g => g.Kind == kind).Payload)!;
        public int Count(MessageKind kind) => Game.Count(g => g.Kind == kind);
    }

    public Player Connect(string name, SessionKey? key = null, string version = "0.1")
    {
        var p = new Player { Connection = ++nextConnection };
        p.Session = new PlayerSession(key ?? Key, name, version);
        p.Session.SendToHost += f => Host.FrameReceived(p.Connection, f);
        p.Session.DeliverToLocalGame += (k, b) => p.Game.Add((k, b));
        p.Session.Log += p.Log.Add;
        p.Session.Rejected += r => p.Reject = r;
        byConnection[p.Connection] = p.Session;
        Host.ConnectionOpened(p.Connection);
        return p;
    }

    public void Disconnect(Player p)
    {
        byConnection.Remove(p.Connection);
        Host.ConnectionClosed(p.Connection);
    }

    public static byte[] Json<T>(T m) => Codec.Encode(m);

    public void Dispose() => Host.Dispose();
}

public class SessionTests
{
    [Fact]
    public void Player_with_the_key_is_welcomed_and_everyone_learns_about_it()
    {
        using var h = new Harness();
        var a = h.Connect("Alice");
        Assert.Equal(PlayerState.Joined, a.Session.State);
        Assert.Equal((ushort)1, a.Session.Id);
        Assert.Equal("Hote", a.Session.HostName);
        var w = a.Last<WelcomeMessage>(MessageKind.Welcome);
        Assert.Equal(new[] { "Hote", "Alice" }, w.Players.Select(p => p.Name));

        var b = h.Connect("Bob");
        Assert.Equal((ushort)2, b.Session.Id);
        Assert.Equal("Bob", a.Last<PlayerJoinedMessage>(MessageKind.PlayerJoined).Player.Name);
        Assert.Equal("Bob", Codec.TryDecode<PlayerJoinedMessage>(h.HostGame.Last(g => g.Kind == MessageKind.PlayerJoined).Payload)!.Player.Name);
        Assert.Equal(3, h.Host.Players.Count);
        Assert.Empty(h.Closed);
    }

    [Fact]
    public void Wrong_key_is_rejected_and_the_connection_closed()
    {
        using var h = new Harness();
        var intruder = h.Connect("Mallory", SessionKey.Derive(Harness.Secret, "guess"));
        Assert.Equal(PlayerState.Rejected, intruder.Session.State);
        Assert.Equal("bad-proof", intruder.Reject!.Reason);
        Assert.Equal(new[] { intruder.Connection }, h.Closed);
        Assert.Single(h.Host.Players);     // the host game only
    }

    [Fact]
    public void Other_version_is_rejected_with_both_versions_named()
    {
        using var h = new Harness();
        var old = h.Connect("Old", version: "0.0");
        Assert.Equal("version", old.Reject!.Reason);
        Assert.Contains("0.1", old.Reject.Detail);
        Assert.Contains("0.0", old.Reject.Detail);
    }

    [Fact]
    public void Session_is_full_at_max_players()
    {
        using var h = new Harness(options: new HostOptions { MaxPlayers = 2 });
        h.Connect("A"); h.Connect("B");
        var c = h.Connect("C");
        Assert.Equal("full", c.Reject!.Reason);
    }

    [Fact]
    public void Duplicate_and_dirty_names_become_unique_and_clean()
    {
        using var h = new Harness("Hote");
        var a = h.Connect("Hote");
        var b = h.Connect("Al ice\t\n!");
        var c = h.Connect("Alice");
        Assert.Equal("Hote#1", a.Session.AssignedName);
        Assert.Equal("Alice", b.Session.AssignedName);
        Assert.Equal("Alice#3", c.Session.AssignedName);
    }

    [Fact]
    public void Frame_carrying_another_players_sender_id_is_dropped()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        using var forgedAsB = new Sealer(h.Key, b.Session.Id!.Value);
        var frame = forgedAsB.Seal((byte)MessageKind.Chat, Harness.Json(new ChatMessage("I am B")));
        h.Host.FrameReceived(a.Connection, frame);       // sent over A's connection
        Assert.Equal(1, h.Host.Dropped);
        Assert.Equal(0, b.Count(MessageKind.Chat));
        Assert.DoesNotContain(h.HostGame, g => g.Kind == MessageKind.Chat);
    }

    [Fact]
    public void Host_only_kinds_from_a_player_are_dropped_and_never_relayed()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        int before = b.Game.Count;
        a.Session.LocalGameMessage(MessageKind.Session, Harness.Json(new SessionState(true, 4, null, 0)));
        a.Session.LocalGameMessage(MessageKind.ResyncLoad, Harness.Json(new ResyncLoadMessage(1, "evil", "00")));
        a.Session.LocalGameMessage(MessageKind.ChecksumRequest, Harness.Json(new ChecksumRequestMessage(9, 0, false)));
        Assert.Equal(3, h.Host.Dropped);
        Assert.Equal(before, b.Game.Count);
        Assert.False(h.Host.State.Started);
        Assert.Contains(h.HostLog, l => l.Contains("host only"));
    }

    [Fact]
    public void Replayed_frame_is_dropped()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        Frame? captured = null;
        a.Session.SendToHost += f => captured = f;
        a.Session.LocalGameMessage(MessageKind.Chat, Harness.Json(new ChatMessage("hi")));
        Assert.NotNull(captured);
        h.Host.FrameReceived(a.Connection, captured!);
        Assert.Equal(1, h.Host.Dropped);
        Assert.Equal(1, h.HostGame.Count(g => g.Kind == MessageKind.Chat));
    }

    [Fact]
    public void Action_is_relayed_to_everyone_but_the_sender_with_its_origin_filled_in()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        var c = h.Connect("C");
        a.Session.LocalGameMessage(MessageKind.Action, Harness.Json(new ActionMessage(30000, 1, "{road}")));
        a.Session.LocalGameMessage(MessageKind.Action, Harness.Json(new ActionMessage(30200, 2, "{buy}")));

        Assert.Equal(0, a.Count(MessageKind.Action));
        Assert.Equal(2, b.Count(MessageKind.Action));
        Assert.Equal(2, c.Count(MessageKind.Action));
        var hostActs = h.HostGame.Where(g => g.Kind == MessageKind.Action).Select(g => Codec.TryDecode<ActionMessage>(g.Payload)!).ToList();
        Assert.Equal(new long[] { 1, 2 }, hostActs.Select(x => x.Seq));
        Assert.All(hostActs, x => Assert.Equal(a.Session.Id!.Value, x.From));
        Assert.Equal("{buy}", b.Last<ActionMessage>(MessageKind.Action).Body);
        Assert.Equal(2, h.Host.ActionsRelayed);
    }

    [Fact]
    public void Host_game_action_reaches_every_player()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        h.Host.LocalGameMessage(MessageKind.Action, Harness.Json(new ActionMessage(1000, 5, "{x}")));
        var got = a.Last<ActionMessage>(MessageKind.Action);
        Assert.Equal(HostSession.HostId, got.From);
        Assert.DoesNotContain(h.HostGame, g => g.Kind == MessageKind.Action);   // not echoed to itself
    }

    [Fact]
    public void Clock_becomes_a_peer_clock_for_the_others_and_feeds_the_stamp()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        a.Session.LocalGameMessage(MessageKind.Clock, Harness.Json(new ClockMessage(50000, 1, 3)));
        var pc = b.Last<PeerClockMessage>(MessageKind.PeerClock);
        Assert.Equal(a.Session.Id!.Value, pc.From);
        Assert.Equal(50000, pc.T);
        Assert.Equal(0, a.Count(MessageKind.PeerClock));
        Assert.Equal(50000, h.Host.MaxClock());
        h.Now += 2;                                        // two real seconds at x1 = 2000 units
        Assert.Equal(52000, h.Host.MaxClock());
        Assert.Equal(Lockstep.RoundUpToStep(52000 + Lockstep.LeadUnits(1)), h.Host.FutureStamp());
    }

    [Fact]
    public void Start_speed_and_pause_are_broadcast_and_speed_requests_obeyed()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        h.Host.LocalGameMessage(MessageKind.Hello, Harness.Json(new HelloMessage(18600)));
        h.Host.LocalGameMessage(MessageKind.Clock, Harness.Json(new ClockMessage(18600, 0, 3)));
        a.Session.LocalGameMessage(MessageKind.Hello, Harness.Json(new HelloMessage(18600)));
        a.Session.LocalGameMessage(MessageKind.Clock, Harness.Json(new ClockMessage(18600, 0, 3)));

        a.Session.LocalGameMessage(MessageKind.SpeedRequest, Harness.Json(new SpeedRequestMessage(2)));
        Assert.False(h.Host.State.Started);                // ignored before start

        h.Host.Start();
        Assert.True(h.Host.State.Started);
        Assert.Equal(18600, h.Host.State.PauseAt);          // every game held at the common time
        Assert.True(a.Session.Session!.Started);
        Assert.Equal(1, a.Count(MessageKind.ChecksumRequest));

        a.Session.LocalGameMessage(MessageKind.SpeedRequest, Harness.Json(new SpeedRequestMessage(2)));
        Assert.Equal(2, h.Host.State.Speed);
        Assert.Null(h.Host.State.PauseAt);
        Assert.Equal(2, a.Session.Session!.Speed);
        Assert.Equal(Lockstep.LeadUnits(2), a.Session.Session.Lead);

        a.Session.LocalGameMessage(MessageKind.SpeedRequest, Harness.Json(new SpeedRequestMessage(2)));   // same speed: no echo
        int sessions = a.Count(MessageKind.Session);
        a.Session.LocalGameMessage(MessageKind.SpeedRequest, Harness.Json(new SpeedRequestMessage(0)));
        Assert.NotNull(h.Host.State.PauseAt);
        Assert.Equal(sessions + 1, a.Count(MessageKind.Session));
        Assert.Equal(0, h.Host.State.PauseAt!.Value % Lockstep.Step);
    }

    [Fact]
    public void Checksum_round_completes_when_every_ready_game_reported_and_names_the_differences()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        h.Host.LocalGameMessage(MessageKind.Hello, Harness.Json(new HelloMessage(0)));
        a.Session.LocalGameMessage(MessageKind.Hello, Harness.Json(new HelloMessage(0)));
        // B never says hello: it is not expected to report
        h.Host.Start();
        var req = a.Last<ChecksumRequestMessage>(MessageKind.ChecksumRequest);

        var hostParts = new Dictionary<string, string> { ["money"] = "1", ["roads"] = "r1", ["vehicles"] = "v" };
        var aParts = new Dictionary<string, string> { ["money"] = "2", ["roads"] = "r1", ["vehicles"] = "v", ["extra"] = "e" };
        h.Host.LocalGameMessage(MessageKind.ChecksumReport, Harness.Json(new ChecksumReportMessage(req.N, req.At, hostParts, new Dictionary<string, double> { ["balance"] = 1000 })));
        Assert.Empty(h.Outcomes);
        Assert.Equal(1000, a.Last<AuthorityMessage>(MessageKind.Authority).Values["balance"]);
        a.Session.LocalGameMessage(MessageKind.ChecksumReport, Harness.Json(new ChecksumReportMessage(req.N, req.At, aParts)));

        var o = Assert.Single(h.Outcomes);
        Assert.False(o.Identical);
        Assert.Equal(new[] { "extra", "money" }, o.DifferingParts);
        Assert.Equal(new ushort[] { 0, 1 }, o.Reporters);
        var money = o.Differences.Single(d => d.Part == "money");
        Assert.Equal("1", money.Values[0]);
        Assert.Equal("2", money.Values[1]);
        Assert.Null(o.Differences.Single(d => d.Part == "extra").Values[0]);
    }

    [Fact]
    public void Identical_reports_give_an_identical_outcome()
    {
        var parts = new Dictionary<string, string> { ["a"] = "1", ["b"] = "2" };
        var reports = new Dictionary<ushort, ChecksumReportMessage> { [0] = new(1, 5, parts), [1] = new(1, 5, new(parts)) };
        var o = ChecksumRounds.Compare(1, reports);
        Assert.True(o.Identical);
        Assert.Equal(5, o.Time);
    }

    [Fact]
    public void Tick_rebroadcasts_the_session_and_requests_checksums_while_running()
    {
        using var h = new Harness(options: new HostOptions { SessionBroadcastSeconds = 3, ChecksumSeconds = 10 });
        var a = h.Connect("A");
        h.Host.Tick();
        Assert.Equal(1, a.Count(MessageKind.Session));
        h.Now += 2; h.Host.Tick();
        Assert.Equal(1, a.Count(MessageKind.Session));
        h.Now += 1.5; h.Host.Tick();
        Assert.Equal(2, a.Count(MessageKind.Session));

        Assert.Equal(0, a.Count(MessageKind.ChecksumRequest));
        h.Host.Start();                                   // paused at start (no clocks): no periodic checksum yet
        h.Host.SetSpeed(1);
        int n0 = a.Count(MessageKind.ChecksumRequest);
        h.Now += 10.5; h.Host.Tick();
        Assert.Equal(n0 + 1, a.Count(MessageKind.ChecksumRequest));
        h.Host.Pause();
        h.Now += 20; h.Host.Tick();
        Assert.Equal(n0 + 1, a.Count(MessageKind.ChecksumRequest));   // paused: none
    }

    [Fact]
    public void Disconnect_removes_the_player_and_tells_the_others()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        var b = h.Connect("B");
        a.Session.LocalGameMessage(MessageKind.Clock, Harness.Json(new ClockMessage(5, 1, 3)));
        h.Disconnect(a);
        Assert.Equal(2, h.Host.Players.Count);
        Assert.Empty(h.Host.Clocks);
        Assert.Equal("A", b.Last<PlayerLeftMessage>(MessageKind.PlayerLeft).Name);
        Assert.Equal("A", Codec.TryDecode<PlayerLeftMessage>(h.HostGame.Last(g => g.Kind == MessageKind.PlayerLeft).Payload)!.Name);
    }

    [Fact]
    public void Player_ignores_frames_not_from_the_host_and_messages_before_welcome()
    {
        using var h = new Harness();
        var a = h.Connect("A");
        using var other = new Sealer(h.Key, 2);
        a.Session.FrameReceived(other.Seal((byte)MessageKind.Session, Harness.Json(new SessionState(true, 4, null, 0))));
        Assert.Equal(1, a.Session.Dropped);
        Assert.False(a.Session.Session!.Started);

        using var fresh = new PlayerSession(h.Key, "F", "0.1");
        fresh.LocalGameMessage(MessageKind.Chat, Harness.Json(new ChatMessage("too early")));
        Assert.Equal(1, fresh.Dropped);
    }

    [Fact]
    public void Chat_is_relayed_with_the_senders_name_and_trimmed()
    {
        using var h = new Harness();
        var a = h.Connect("Alice");
        var b = h.Connect("Bob");
        a.Session.LocalGameMessage(MessageKind.Chat, Harness.Json(new ChatMessage(new string('x', 600))));
        var got = b.Last<ChatMessage>(MessageKind.Chat);
        Assert.Equal("Alice", got.Name);
        Assert.Equal(a.Session.Id!.Value, got.From);
        Assert.Equal(500, got.Text.Length);
    }

    [Fact]
    public void Codec_round_trips_records_and_rejects_garbage()
    {
        var m = new ChecksumReportMessage(3, 1200, new Dictionary<string, string> { ["roads"] = "abc" }, new Dictionary<string, double> { ["balance"] = -5.5 });
        var back = Codec.TryDecode<ChecksumReportMessage>(Codec.Encode(m));
        Assert.Equal(m.N, back!.N);
        Assert.Equal("abc", back.Parts["roads"]);
        Assert.Equal(-5.5, back.Authority!["balance"]);
        Assert.Null(Codec.TryDecode<ChecksumReportMessage>(Encoding.UTF8.GetBytes("{not json")));
        Assert.Null(Codec.TryDecode<ChecksumReportMessage>(Encoding.UTF8.GetBytes("[1,2]")));
        var json = Encoding.UTF8.GetString(Codec.Encode(new ActionMessage(1, 2, "b")));
        Assert.Contains("\"stamp\":1", json);
        Assert.DoesNotContain("null", json);
    }

    [Fact]
    public void Proof_depends_on_key_nonce_and_name()
    {
        var key = SessionKey.Derive(Harness.Secret);
        var nonce = new byte[16];
        var p = HostSession.Proof(key, nonce, "A");
        Assert.Equal(32, p.Length);
        Assert.NotEqual(p, HostSession.Proof(key, nonce, "B"));
        Assert.NotEqual(p, HostSession.Proof(key, new byte[] { 1 }, "A"));
        Assert.NotEqual(p, HostSession.Proof(SessionKey.Derive(Harness.Secret, "pw"), nonce, "A"));
    }
}
