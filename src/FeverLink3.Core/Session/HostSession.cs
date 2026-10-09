using System.Security.Cryptography;
using System.Text;
using FeverLink3.Crypto;
using FeverLink3.Protocol;

namespace FeverLink3.Session;

public sealed class HostOptions
{
    public string Version { get; init; } = "0.1";
    public int MaxPlayers { get; init; } = 8;
    public double SessionBroadcastSeconds { get; init; } = 3;
    public double ChecksumSeconds { get; init; } = 10;
}

/// <summary>
/// The host side of a session, as a state machine with no I/O: connections are integers the caller owns, frames go
/// in through <see cref="FrameReceived"/> and out through <see cref="SendToConnection"/>, and the host's own game is
/// player 0, reached through <see cref="LocalGameMessage"/> and <see cref="DeliverToLocalGame"/>. Time comes from
/// the <c>now</c> function given to the constructor, so tests can drive it.
/// <para>Thread-safety: none. The caller serialises all calls (one lock around the host), which also gives every
/// game the same message order.</para>
/// </summary>
public sealed class HostSession : IDisposable
{
    public const ushort HostId = 0;
    public const ushort Unassigned = 0xFFFF;
    public const int ProtocolVersion = FrameHeader.CurrentVersion;

    sealed class Connection
    {
        public int Id;
        public byte[] Nonce = Array.Empty<byte>();
        public ushort? Player;
    }

    readonly SessionKey key;
    readonly HostOptions options;
    readonly Func<double> now;
    readonly Sealer sealer;
    readonly Opener opener;
    readonly Dictionary<int, Connection> connections = new();
    readonly Dictionary<ushort, PlayerInfo> players = new();
    readonly Dictionary<ushort, PlayerClock> clocks = new();
    readonly ChecksumRounds checksums = new();
    readonly HashSet<string> droppedOnce = new();
    ushort nextPlayerId = 1;
    int checksumN;
    double lastSessionBroadcast = double.NegativeInfinity, lastChecksum = double.NegativeInfinity;

    public HostSession(SessionKey key, string hostName, Func<double> now, HostOptions? options = null)
    {
        this.key = key ?? throw new ArgumentNullException(nameof(key));
        this.now = now ?? throw new ArgumentNullException(nameof(now));
        this.options = options ?? new HostOptions();
        sealer = new Sealer(key, HostId);
        opener = new Opener(key);
        HostName = Names.Sanitize(hostName);
        players[HostId] = new PlayerInfo(HostId, HostName, false);
        State = new SessionState(false, 0, null, Lockstep.LeadUnits(1));
    }

    public string HostName { get; }
    public SessionState State { get; private set; }
    public IReadOnlyCollection<PlayerInfo> Players => players.Values;
    public IReadOnlyDictionary<ushort, PlayerClock> Clocks => clocks;
    public int ActionsRelayed { get; private set; }
    public int Dropped { get; private set; }

    /// <summary>A frame to write to a connection, in this order.</summary>
    public event Action<int, Frame>? SendToConnection;
    /// <summary>The caller must close this connection (after a reject).</summary>
    public event Action<int>? CloseConnection;
    /// <summary>A message for the host's own game (kind, payload).</summary>
    public event Action<MessageKind, byte[]>? DeliverToLocalGame;
    /// <summary>Every message accepted from a player or the local game (from, kind, payload), for logging and for
    /// kinds the host only observes (save done, replay failed, action refused).</summary>
    public event Action<ushort, MessageKind, byte[]>? Received;
    public event Action<PlayerInfo>? PlayerJoined;
    public event Action<PlayerInfo>? PlayerLeft;
    public event Action<ChecksumOutcome>? ChecksumCompleted;
    public event Action<string>? Log;

    // ---------------------------------------------------------------- connections

    public void ConnectionOpened(int connectionId)
    {
        var c = new Connection { Id = connectionId, Nonce = RandomNumberGenerator.GetBytes(16) };
        connections[connectionId] = c;
        SendUnsealed(connectionId, MessageKind.Challenge, Codec.Encode(new ChallengeMessage(c.Nonce, ProtocolVersion, options.Version)));
    }

    public void ConnectionClosed(int connectionId)
    {
        if (!connections.Remove(connectionId, out var c)) return;
        if (c.Player is not ushort id || !players.Remove(id, out var info)) return;
        clocks.Remove(id);
        Log?.Invoke($"{info.Name} left");
        Broadcast(MessageKind.PlayerLeft, Codec.Encode(new PlayerLeftMessage(id, info.Name)), except: id);
        PlayerLeft?.Invoke(info);
    }

    public void FrameReceived(int connectionId, Frame frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        if (!connections.TryGetValue(connectionId, out var c)) return;
        var kind = (MessageKind)frame.Header.Kind;
        if (c.Player is null)
        {
            if (kind != MessageKind.Join) { Drop(connectionId, kind, "not introduced"); return; }
            HandleJoin(c, frame);
            return;
        }
        if (frame.Header.Sender != c.Player.Value) { Drop(connectionId, kind, $"sender id {frame.Header.Sender} on the connection of player {c.Player}"); return; }
        if (!opener.TryOpen(frame, out var plain, out var err)) { Drop(connectionId, kind, err.ToString()); return; }
        if (!MessageKinds.FromPlayer.Contains(kind)) { Drop(connectionId, kind, "host only"); return; }
        HandleMessage(c.Player.Value, kind, plain);
    }

    void HandleJoin(Connection c, Frame frame)
    {
        var join = Codec.TryDecode<JoinMessage>(frame.Body);
        if (join is null || join.Proof is null || join.Name is null) { Reject(c, "malformed"); return; }
        if (join.Version != options.Version) { Reject(c, "version", $"host runs {options.Version}, you run {join.Version}"); return; }
        var expected = Proof(key, c.Nonce, join.Name);
        if (!CryptographicOperations.FixedTimeEquals(expected, join.Proof)) { Reject(c, "bad-proof"); return; }
        if (players.Count - 1 >= options.MaxPlayers) { Reject(c, "full", $"{options.MaxPlayers} players"); return; }
        if (nextPlayerId == Unassigned) { Reject(c, "full", "no ids left"); return; }

        ushort id = nextPlayerId++;
        string name = Names.Unique(Names.Sanitize(join.Name), id, players.Values.Select(p => p.Name));
        var info = new PlayerInfo(id, name, false);
        players[id] = info;
        c.Player = id;
        Log?.Invoke($"{name} joined (connection {c.Id})");
        Send(c.Id, MessageKind.Welcome, Codec.Encode(new WelcomeMessage(id, HostName, options.Version, players.Values.OrderBy(p => p.Id).ToArray(), State)));
        Broadcast(MessageKind.PlayerJoined, Codec.Encode(new PlayerJoinedMessage(info)), except: id);
        PlayerJoined?.Invoke(info);
    }

    void Reject(Connection c, string reason, string? detail = null)
    {
        Log?.Invoke($"connection {c.Id} rejected: {reason} {detail}");
        SendUnsealed(c.Id, MessageKind.Reject, Codec.Encode(new RejectMessage(reason, detail)));
        connections.Remove(c.Id);
        CloseConnection?.Invoke(c.Id);
    }

    /// <summary>What a joiner must send back for a challenge.</summary>
    public static byte[] Proof(SessionKey key, byte[] nonce, string name)
    {
        ArgumentNullException.ThrowIfNull(key);
        ArgumentNullException.ThrowIfNull(nonce);
        var nameBytes = Encoding.UTF8.GetBytes(name ?? "");
        var data = new byte[nonce.Length + nameBytes.Length];
        nonce.CopyTo(data, 0);
        nameBytes.CopyTo(data, nonce.Length);
        return HMACSHA256.HashData(key.Key, data);
    }

    void Drop(int connectionId, MessageKind kind, string why)
    {
        Dropped++;
        if (droppedOnce.Add($"{connectionId}/{kind}/{why}")) Log?.Invoke($"connection {connectionId}: {kind} dropped ({why})");
    }

    // ---------------------------------------------------------------- the host's own game

    /// <summary>A message from the host's own game. It is trusted like the host itself, so every kind is accepted.</summary>
    public void LocalGameMessage(MessageKind kind, byte[] payload)
    {
        ArgumentNullException.ThrowIfNull(payload);
        HandleMessage(HostId, kind, payload);
    }

    // ---------------------------------------------------------------- messages

    void HandleMessage(ushort from, MessageKind kind, byte[] payload)
    {
        switch (kind)
        {
            case MessageKind.Hello:
                if (players.TryGetValue(from, out var p)) players[from] = p with { Ready = true };
                Log?.Invoke($"{Name(from)} ready {Encoding.UTF8.GetString(payload)}");
                if (from != HostId) Send(ConnectionOf(from), MessageKind.Session, Codec.Encode(State));
                break;

            case MessageKind.Clock:
                {
                    var m = Codec.TryDecode<ClockMessage>(payload);
                    if (m is null) break;
                    clocks[from] = new PlayerClock { T = m.T, Speed = m.Speed, At = now() };
                    Broadcast(MessageKind.PeerClock, Codec.Encode(new PeerClockMessage(from, m.T, m.Ahead)), except: from);
                    break;
                }

            case MessageKind.Action:
                {
                    var m = Codec.TryDecode<ActionMessage>(payload);
                    if (m is null) break;
                    Broadcast(MessageKind.Action, Codec.Encode(m with { From = from }), except: from);
                    ActionsRelayed++;
                    break;
                }

            case MessageKind.Chat:
                {
                    var m = Codec.TryDecode<ChatMessage>(payload);
                    if (m is null || string.IsNullOrWhiteSpace(m.Text)) break;
                    string text = m.Text.Length > 500 ? m.Text[..500] : m.Text;
                    Broadcast(MessageKind.Chat, Codec.Encode(new ChatMessage(text, from, Name(from))), except: from);
                    break;
                }

            case MessageKind.SpeedRequest:
                {
                    var m = Codec.TryDecode<SpeedRequestMessage>(payload);
                    if (m is null || !State.Started) break;
                    bool same = m.Speed > 0 && State.PauseAt is null && m.Speed == State.Speed || m.Speed <= 0 && State.PauseAt is not null;
                    if (same) break;
                    if (m.Speed <= 0) Pause(); else SetSpeed(m.Speed);
                    Log?.Invoke($"speed {(m.Speed <= 0 ? "pause" : "x" + m.Speed)} asked by {Name(from)}");
                    break;
                }

            case MessageKind.ChecksumReport:
                {
                    var m = Codec.TryDecode<ChecksumReportMessage>(payload);
                    if (m is null || m.Parts is null) break;
                    if (from == HostId && m.Authority is { Count: > 0 })
                        Broadcast(MessageKind.Authority, Codec.Encode(new AuthorityMessage(m.N, m.Authority)), except: HostId);
                    var outcome = checksums.Report(from, m, players.Values.Where(x => x.Ready).Select(x => x.Id).ToList());
                    if (outcome is not null) ChecksumCompleted?.Invoke(outcome);
                    break;
                }
        }
        Received?.Invoke(from, kind, payload);
    }

    // ---------------------------------------------------------------- session control (the launcher's host UI)

    public void Start()
    {
        var times = clocks.Values.Select(c => c.T).Distinct().ToList();
        State = State with { Started = true, Speed = 0, PauseAt = times.Count == 1 ? times[0] : null };
        Log?.Invoke($"session started with {players.Values.Count(p => p.Ready)} ready game(s) at {(times.Count == 1 ? times[0].ToString() : "different times")}");
        BroadcastSession();
        RequestChecksum();
    }

    public void SetSpeed(int speed)
    {
        if (!State.Started) return;
        State = State with { Speed = Math.Clamp(speed, 1, 4), PauseAt = null, Lead = Lockstep.LeadUnits(Math.Clamp(speed, 1, 4)) };
        BroadcastSession();
    }

    public void Pause()
    {
        if (!State.Started) return;
        State = State with { PauseAt = FutureStamp() };
        BroadcastSession();
    }

    public void RequestChecksum()
    {
        int n = ++checksumN;
        checksums.Open(n);
        long at;
        bool paused = State.PauseAt is long p && MaxClock() >= p;
        if (paused) at = Math.Max(State.PauseAt!.Value, clocks.Count == 0 ? 0 : clocks.Values.Max(c => c.T));
        else at = FutureStamp();
        Broadcast(MessageKind.ChecksumRequest, Codec.Encode(new ChecksumRequestMessage(n, at, paused)));
        lastChecksum = now();
    }

    /// <summary>Call regularly (every 250 ms or so): periodic session broadcast and checksum rounds.</summary>
    public void Tick()
    {
        double t = now();
        if (t - lastSessionBroadcast >= options.SessionBroadcastSeconds) BroadcastSession();
        if (State.Started && State.PauseAt is null && t - lastChecksum >= options.ChecksumSeconds) RequestChecksum();
    }

    public long MaxClock() => clocks.Count == 0 ? 0 : (long)clocks.Values.Max(c => c.Estimate(now()));

    /// <summary>A stamp every game reaches after receiving a message sent now.</summary>
    public long FutureStamp() => Lockstep.RoundUpToStep(MaxClock() + Lockstep.LeadUnits(Math.Max(1, State.Speed)));

    void BroadcastSession()
    {
        Broadcast(MessageKind.Session, Codec.Encode(State));
        lastSessionBroadcast = now();
    }

    // ---------------------------------------------------------------- sending

    /// <summary>To every player connection and the local game, except <paramref name="except"/>. One sealer, so
    /// the host's counter stream is shared by all connections; a receiver only ever sees it increase.</summary>
    void Broadcast(MessageKind kind, byte[] payload, ushort? except = null)
    {
        foreach (var c in connections.Values)
            if (c.Player is ushort id && id != except) SendToConnection?.Invoke(c.Id, sealer.Seal((byte)kind, payload));
        if (except != HostId) DeliverToLocalGame?.Invoke(kind, payload);
    }

    void Send(int connectionId, MessageKind kind, byte[] payload) => SendToConnection?.Invoke(connectionId, sealer.Seal((byte)kind, payload));

    void SendUnsealed(int connectionId, MessageKind kind, byte[] payload) =>
        SendToConnection?.Invoke(connectionId, new Frame(new FrameHeader(FrameHeader.CurrentVersion, (byte)kind, Unassigned, 0), payload));

    int ConnectionOf(ushort player) => connections.Values.First(c => c.Player == player).Id;
    string Name(ushort id) => players.TryGetValue(id, out var p) ? p.Name : $"#{id}";

    public void Dispose()
    {
        sealer.Dispose();
        opener.Dispose();
    }
}

public static class Names
{
    public const int MaxLength = 24;

    /// <summary>Letters, digits, '_' and '-' only, at most 24 characters; "player" when nothing is left.</summary>
    public static string Sanitize(string? name)
    {
        var t = new string((name ?? "").Where(ch => char.IsLetterOrDigit(ch) || ch == '_' || ch == '-').ToArray());
        if (t.Length > MaxLength) t = t[..MaxLength];
        return t.Length == 0 ? "player" : t;
    }

    /// <summary>The name itself when free, else <c>name#id</c>: two players never share a display name.</summary>
    public static string Unique(string name, ushort id, IEnumerable<string> taken)
    {
        ArgumentNullException.ThrowIfNull(taken);
        var set = new HashSet<string>(taken, StringComparer.OrdinalIgnoreCase);
        return set.Contains(name) ? $"{name}#{id}" : name;
    }
}
