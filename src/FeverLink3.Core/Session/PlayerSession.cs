using FeverLink3.Crypto;
using FeverLink3.Protocol;

namespace FeverLink3.Session;

public enum PlayerState { Connecting, Joined, Rejected }

/// <summary>
/// The player side of a session: answers the host's challenge, then seals everything its game sends and opens
/// everything the host sends. No I/O: frames in through <see cref="FrameReceived"/>, out through
/// <see cref="SendToHost"/>; the local game through <see cref="LocalGameMessage"/> and <see cref="DeliverToLocalGame"/>.
/// Every sealed frame a player receives comes from the host (sender 0); other players' actions arrive relayed, with
/// their origin inside the payload.
/// </summary>
public sealed class PlayerSession : IDisposable
{
    readonly SessionKey key;
    readonly string name, version;
    readonly Opener opener;
    Sealer? sealer;

    public PlayerSession(SessionKey key, string name, string version)
    {
        this.key = key ?? throw new ArgumentNullException(nameof(key));
        this.name = Names.Sanitize(name);
        this.version = version ?? throw new ArgumentNullException(nameof(version));
        opener = new Opener(key);
    }

    public PlayerState State { get; private set; } = PlayerState.Connecting;
    public ushort? Id { get; private set; }
    public string? AssignedName { get; private set; }
    public string? HostName { get; private set; }
    public SessionState? Session { get; private set; }
    public int Dropped { get; private set; }

    public event Action<Frame>? SendToHost;
    public event Action<MessageKind, byte[]>? DeliverToLocalGame;
    public event Action<WelcomeMessage>? Welcomed;
    public event Action<RejectMessage>? Rejected;
    public event Action<string>? Log;

    public void FrameReceived(Frame frame)
    {
        ArgumentNullException.ThrowIfNull(frame);
        var kind = (MessageKind)frame.Header.Kind;
        if (kind == MessageKind.Reject && frame.Header.Sender == HostSession.Unassigned)
        {
            var r = Codec.TryDecode<RejectMessage>(frame.Body) ?? new RejectMessage("malformed");
            State = PlayerState.Rejected;
            Log?.Invoke($"rejected by the host: {r.Reason} {r.Detail}");
            Rejected?.Invoke(r);
            return;
        }
        if (State == PlayerState.Connecting && kind == MessageKind.Challenge && frame.Header.Sender == HostSession.Unassigned)
        {
            var ch = Codec.TryDecode<ChallengeMessage>(frame.Body);
            if (ch is null || ch.Nonce is null) { Drop(kind, "malformed challenge"); return; }
            if (ch.Protocol != HostSession.ProtocolVersion) { Drop(kind, $"protocol {ch.Protocol}"); return; }
            var join = new JoinMessage(name, version, HostSession.Proof(key, ch.Nonce, name));
            SendToHost?.Invoke(new Frame(new FrameHeader(FrameHeader.CurrentVersion, (byte)MessageKind.Join, HostSession.Unassigned, 0), Codec.Encode(join)));
            return;
        }
        if (frame.Header.Sender != HostSession.HostId) { Drop(kind, $"sender {frame.Header.Sender} is not the host"); return; }
        if (!opener.TryOpen(frame, out var plain, out var err)) { Drop(kind, err.ToString()); return; }

        if (kind == MessageKind.Welcome)
        {
            var w = Codec.TryDecode<WelcomeMessage>(plain);
            if (w is null) { Drop(kind, "malformed welcome"); return; }
            Id = w.You;
            AssignedName = w.Players.FirstOrDefault(p => p.Id == w.You)?.Name ?? name;
            HostName = w.HostName;
            Session = w.Session;
            sealer?.Dispose();
            sealer = new Sealer(key, w.You);
            State = PlayerState.Joined;
            Log?.Invoke($"joined {w.HostName}'s session as {AssignedName} (#{w.You})");
            Welcomed?.Invoke(w);
            DeliverToLocalGame?.Invoke(kind, plain);
            return;
        }
        if (State != PlayerState.Joined) { Drop(kind, "before welcome"); return; }
        if (kind == MessageKind.Session) Session = Codec.TryDecode<SessionState>(plain) ?? Session;
        DeliverToLocalGame?.Invoke(kind, plain);
    }

    /// <summary>A message from this player's game, sealed and sent to the host. Dropped (and counted) before the
    /// welcome: the game should not be running before the host has accepted us.</summary>
    public void LocalGameMessage(MessageKind kind, byte[] payload)
    {
        ArgumentNullException.ThrowIfNull(payload);
        if (State != PlayerState.Joined || sealer is null) { Drop(kind, "not joined"); return; }
        SendToHost?.Invoke(sealer.Seal((byte)kind, payload));
    }

    void Drop(MessageKind kind, string why)
    {
        Dropped++;
        Log?.Invoke($"{kind} dropped ({why})");
    }

    public void Dispose()
    {
        opener.Dispose();
        sealer?.Dispose();
    }
}
