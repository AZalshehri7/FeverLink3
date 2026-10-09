using System.Text.Json;
using System.Text.Json.Serialization;

namespace FeverLink3.Session;

/// <summary>Payloads are UTF-8 JSON, camelCase, nulls omitted. Byte arrays are base64. Unknown fields are ignored so
/// a newer peer can add fields without breaking an older one; a changed meaning needs a new <see cref="MessageKind"/>.</summary>
public static class Codec
{
    public static readonly JsonSerializerOptions Options = new(JsonSerializerDefaults.Web)
    {
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    public static byte[] Encode<T>(T message) => JsonSerializer.SerializeToUtf8Bytes(message, Options);

    /// <summary>Null for anything that is not a well-formed message of the expected shape; never throws on input
    /// from the network.</summary>
    public static T? TryDecode<T>(ReadOnlySpan<byte> bytes) where T : class
    {
        try { return JsonSerializer.Deserialize<T>(bytes, Options); }
        catch (JsonException) { return null; }
        catch (NotSupportedException) { return null; }
    }
}

// ---- unsealed

/// <summary>Host to a new connection: prove you hold the session key by answering with an HMAC of this nonce.</summary>
public sealed record ChallengeMessage(byte[] Nonce, int Protocol, string Version);

/// <summary>Connection to host: display name and <c>HMAC-SHA256(sessionKey, nonce ‖ UTF8(name))</c>.</summary>
public sealed record JoinMessage(string Name, string Version, byte[] Proof);

/// <summary>Host to a connection it will close. Reasons: <c>bad-proof</c>, <c>version</c>, <c>full</c>, <c>malformed</c>.</summary>
public sealed record RejectMessage(string Reason, string? Detail = null);

// ---- session model

/// <summary>What every game must agree on. Broadcast by the host on every change and every few seconds.</summary>
/// <param name="Started">The session runs (games may leave the loaded time).</param>
/// <param name="Speed">0 while paused or not started, else 1..4.</param>
/// <param name="PauseAt">Game time every game stops at exactly; null when running.</param>
/// <param name="Lead">Game-time units a local action is stamped ahead of now.</param>
public sealed record SessionState(bool Started, int Speed, long? PauseAt, long Lead);

public sealed record PlayerInfo(ushort Id, string Name, bool Ready);

// ---- host -> player

public sealed record WelcomeMessage(ushort You, string HostName, string Version, PlayerInfo[] Players, SessionState Session);
public sealed record PlayerJoinedMessage(PlayerInfo Player);
public sealed record PlayerLeftMessage(ushort Id, string Name);
public sealed record PeerClockMessage(ushort From, long T, int Ahead);
public sealed record ChecksumRequestMessage(int N, long At, bool Paused);
public sealed record AuthorityMessage(int N, Dictionary<string, double> Values);
public sealed record ResyncSaveMessage(int Id, string Name);
public sealed record ResyncChunkMessage(int Id, int Index, int Count, string Sha256, byte[] Data);
public sealed record ResyncLoadMessage(int Id, string Name, string Sha256);

// ---- player -> host

/// <summary>The game has loaded a savegame and runs the mod. <paramref name="Time"/> is its game time.</summary>
public sealed record HelloMessage(long Time, string? Save = null);
public sealed record ClockMessage(long T, int Speed, int Ahead);
/// <summary>0 asks for a pause.</summary>
public sealed record SpeedRequestMessage(int Speed);
public sealed record ChecksumReportMessage(int N, long Time, Dictionary<string, string> Parts, Dictionary<string, double>? Authority = null);
public sealed record SaveDoneMessage(int Id, bool Ok, string? Detail = null);
public sealed record ReplayFailedMessage(string Detail);
public sealed record ActionRefusedMessage(long Seq, string Detail);

// ---- relayed

/// <summary>A player action, already stamped by its originator. <paramref name="Body"/> is opaque to the protocol:
/// the mod's own serialisation. <paramref name="From"/> is filled in by the host on relay.</summary>
public sealed record ActionMessage(long Stamp, long Seq, string Body, ushort From = 0);
public sealed record ChatMessage(string Text, ushort From = 0, string? Name = null);
