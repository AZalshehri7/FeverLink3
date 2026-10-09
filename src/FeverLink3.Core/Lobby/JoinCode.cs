using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;

namespace FeverLink3.Lobby;

/// <summary>
/// What a host gives their friends. It carries where to connect and the session secret, so the code is the
/// credential: anyone who has it can join (unless the host also set a password).
/// <para>Binary layout, then base32 (RFC 4648 alphabet, no padding, case-insensitive, dashes and spaces ignored):</para>
/// <code>
/// u8   version (1)
/// u32  created, unix seconds
/// 16   secret
/// u8   endpoint count (0..8)
/// per endpoint: u8 family (4 | 6), address bytes, u16 port
/// 4    check = first 4 bytes of SHA-256 of everything before
/// </code>
/// A code with one IPv4 endpoint is 33 bytes, 53 characters.
/// </summary>
public sealed record JoinCode(byte Version, DateTimeOffset Created, IReadOnlyList<IPEndPoint> Endpoints, byte[] Secret)
{
    public const byte CurrentVersion = 1;
    public const int MaxEndpoints = 8;
    const int CheckSize = 4;
    const string Alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    public static JoinCode Create(IEnumerable<IPEndPoint> endpoints, byte[] secret, DateTimeOffset? created = null)
    {
        ArgumentNullException.ThrowIfNull(endpoints);
        ArgumentNullException.ThrowIfNull(secret);
        if (secret.Length != Crypto.SessionKey.SecretSize) throw new ArgumentException("secret must be 16 bytes", nameof(secret));
        var list = endpoints.ToList();
        if (list.Count > MaxEndpoints) throw new ArgumentException($"at most {MaxEndpoints} endpoints", nameof(endpoints));
        foreach (var ep in list)
            if (ep.AddressFamily != AddressFamily.InterNetwork && ep.AddressFamily != AddressFamily.InterNetworkV6)
                throw new ArgumentException("IPv4 or IPv6 endpoints only", nameof(endpoints));
        var when = created ?? DateTimeOffset.UtcNow;
        return new JoinCode(CurrentVersion, DateTimeOffset.FromUnixTimeSeconds(when.ToUnixTimeSeconds()), list, (byte[])secret.Clone());
    }

    public string Encode()
    {
        var ms = new MemoryStream();
        ms.WriteByte(Version);
        Span<byte> u32 = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(u32, (uint)Created.ToUnixTimeSeconds());
        ms.Write(u32);
        ms.Write(Secret);
        ms.WriteByte((byte)Endpoints.Count);
        Span<byte> u16 = stackalloc byte[2];
        foreach (var ep in Endpoints)
        {
            ms.WriteByte(ep.AddressFamily == AddressFamily.InterNetwork ? (byte)4 : (byte)6);
            ms.Write(ep.Address.GetAddressBytes());
            BinaryPrimitives.WriteUInt16BigEndian(u16, (ushort)ep.Port);
            ms.Write(u16);
        }
        var body = ms.ToArray();
        var check = SHA256.HashData(body).AsSpan(0, CheckSize);
        var all = new byte[body.Length + CheckSize];
        body.CopyTo(all, 0);
        check.CopyTo(all.AsSpan(body.Length));
        return Base32Encode(all);
    }

    /// <summary>Parses a code typed or pasted by a player. Returns null for anything that is not a valid code of a
    /// version this build understands; never throws on bad input.</summary>
    public static JoinCode? TryDecode(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return null;
        var bytes = Base32Decode(text);
        if (bytes == null || bytes.Length < 1 + 4 + Crypto.SessionKey.SecretSize + 1 + CheckSize) return null;
        int bodyLen = bytes.Length - CheckSize;
        var expect = SHA256.HashData(bytes.AsSpan(0, bodyLen)).AsSpan(0, CheckSize);
        if (!expect.SequenceEqual(bytes.AsSpan(bodyLen, CheckSize))) return null;
        var r = bytes.AsSpan(0, bodyLen);
        int i = 0;
        byte version = r[i++];
        if (version != CurrentVersion) return null;
        uint created = BinaryPrimitives.ReadUInt32BigEndian(r[i..]); i += 4;
        var secret = r.Slice(i, Crypto.SessionKey.SecretSize).ToArray(); i += Crypto.SessionKey.SecretSize;
        int count = r[i++];
        if (count > MaxEndpoints) return null;
        var eps = new List<IPEndPoint>(count);
        for (int k = 0; k < count; k++)
        {
            if (i >= r.Length) return null;
            byte family = r[i++];
            int addrLen = family == 4 ? 4 : family == 6 ? 16 : -1;
            if (addrLen < 0 || i + addrLen + 2 > r.Length) return null;
            var addr = new IPAddress(r.Slice(i, addrLen)); i += addrLen;
            int port = BinaryPrimitives.ReadUInt16BigEndian(r[i..]); i += 2;
            eps.Add(new IPEndPoint(addr, port));
        }
        if (i != r.Length) return null;
        return new JoinCode(version, DateTimeOffset.FromUnixTimeSeconds(created), eps, secret);
    }

    public bool Equals(JoinCode? other) =>
        other is not null && Version == other.Version && Created == other.Created
        && Secret.AsSpan().SequenceEqual(other.Secret) && Endpoints.SequenceEqual(other.Endpoints);

    public override int GetHashCode() => HashCode.Combine(Version, Created, Endpoints.Count, BitConverter.ToInt32(Secret, 0));

    // ---- base32

    internal static string Base32Encode(ReadOnlySpan<byte> data)
    {
        var sb = new StringBuilder((data.Length * 8 + 4) / 5);
        int buffer = 0, bits = 0;
        foreach (byte b in data)
        {
            buffer = (buffer << 8) | b;
            bits += 8;
            while (bits >= 5)
            {
                bits -= 5;
                sb.Append(Alphabet[(buffer >> bits) & 31]);
            }
        }
        if (bits > 0) sb.Append(Alphabet[(buffer << (5 - bits)) & 31]);
        return sb.ToString();
    }

    internal static byte[]? Base32Decode(string text)
    {
        var clean = new StringBuilder(text.Length);
        foreach (char ch in text)
        {
            if (ch == '-' || ch == ' ' || ch == '=') continue;
            clean.Append(char.ToUpperInvariant(ch));
        }
        var result = new List<byte>(clean.Length * 5 / 8);
        int buffer = 0, bits = 0;
        foreach (char ch in clean.ToString())
        {
            int v = Alphabet.IndexOf(ch, StringComparison.Ordinal);
            if (v < 0) return null;
            buffer = (buffer << 5) | v;
            bits += 5;
            if (bits >= 8)
            {
                bits -= 8;
                result.Add((byte)((buffer >> bits) & 0xFF));
            }
        }
        // the leftover bits of the last character must be padding zeros
        if (bits >= 5 || (buffer & ((1 << bits) - 1)) != 0) return null;
        return result.ToArray();
    }
}
