namespace FeverLink3.Session;

/// <summary>Frame kinds. The byte value travels in the frame header. See <c>docs/PROTOCOL.md</c>.</summary>
public enum MessageKind : byte
{
    // ---- unsealed: the only frames sent before a player holds a sender id
    Challenge = 1,
    Join = 2,
    Reject = 3,

    // ---- host -> player, sealed
    Welcome = 10,
    Session = 11,
    PeerClock = 12,
    PlayerJoined = 13,
    PlayerLeft = 14,
    ChecksumRequest = 15,
    Authority = 16,
    ResyncSave = 17,
    ResyncChunk = 18,
    ResyncLoad = 19,

    // ---- player -> host, sealed
    Hello = 30,
    Clock = 31,
    SpeedRequest = 32,
    ChecksumReport = 33,
    SaveDone = 34,
    ReplayFailed = 35,
    ActionRefused = 36,

    // ---- player -> host, relayed by the host to everyone else with the origin filled in
    Action = 50,
    Chat = 51,
}

public static class MessageKinds
{
    /// <summary>What a player may send. The host drops anything else, so a player can never pass itself off as
    /// the host (session state, checksum requests, authority values, resynchronisation).</summary>
    public static readonly IReadOnlySet<MessageKind> FromPlayer = new HashSet<MessageKind>
    {
        MessageKind.Hello, MessageKind.Clock, MessageKind.SpeedRequest, MessageKind.ChecksumReport, MessageKind.SaveDone,
        MessageKind.ReplayFailed, MessageKind.ActionRefused, MessageKind.Action, MessageKind.Chat,
    };

    public static bool IsUnsealed(MessageKind k) => k is MessageKind.Challenge or MessageKind.Join or MessageKind.Reject;
}
