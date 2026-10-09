using System.Buffers.Binary;

namespace FeverLink3.Protocol;

/// <summary>
/// The 12 header bytes of every frame. They travel in clear and are authenticated as the GCM additional data,
/// so a receiver can route on <see cref="Kind"/> and <see cref="Sender"/> before decrypting, yet neither can be
/// altered without detection.
/// </summary>
/// <param name="Version">Protocol version; <see cref="FrameHeader.CurrentVersion"/> today.</param>
/// <param name="Kind">Message kind (see <c>docs/PROTOCOL.md</c>).</param>
/// <param name="Sender">Sender id assigned by the host at join; 0 is the host itself.</param>
/// <param name="Counter">Per-sender counter, strictly increasing. Nonce material and replay protection.</param>
public readonly record struct FrameHeader(byte Version, byte Kind, ushort Sender, ulong Counter)
{
    public const byte CurrentVersion = 1;
    public const int Size = 1 + 1 + 2 + 8;

    public void Write(Span<byte> dst)
    {
        if (dst.Length < Size) throw new ArgumentException("header needs 12 bytes", nameof(dst));
        dst[0] = Version;
        dst[1] = Kind;
        BinaryPrimitives.WriteUInt16BigEndian(dst[2..], Sender);
        BinaryPrimitives.WriteUInt64BigEndian(dst[4..], Counter);
    }

    public static FrameHeader Read(ReadOnlySpan<byte> src)
    {
        if (src.Length < Size) throw new ArgumentException("header needs 12 bytes", nameof(src));
        return new FrameHeader(src[0], src[1], BinaryPrimitives.ReadUInt16BigEndian(src[2..]), BinaryPrimitives.ReadUInt64BigEndian(src[4..]));
    }

    public byte[] ToBytes()
    {
        var b = new byte[Size];
        Write(b);
        return b;
    }
}

/// <summary>A frame as read from the wire: header plus body. The body is ciphertext and tag for a sealed frame,
/// plaintext for the two unsealed kinds (join, reject).</summary>
public sealed class Frame
{
    public Frame(FrameHeader header, byte[] body)
    {
        Header = header;
        Body = body ?? throw new ArgumentNullException(nameof(body));
    }

    public FrameHeader Header { get; }
    public byte[] Body { get; }
}

/// <summary>Raised when the bytes on the wire are not a frame of this protocol. The connection must be closed.</summary>
public sealed class ProtocolException : Exception
{
    public ProtocolException(string message) : base(message) { }
}

/// <summary>
/// Wire format: <c>u32 length</c> (big-endian, counts everything after itself) then header then body.
/// The length is bounded by <see cref="MaxFrameLength"/>: a peer cannot make the reader allocate without limit.
/// </summary>
public static class FrameCodec
{
    /// <summary>Largest frame after the length field: header plus body. 1 MiB holds a 256 KiB savegame chunk
    /// with room to spare and is far above any control message.</summary>
    public const int MaxFrameLength = 1 << 20;
    public const int LengthFieldSize = 4;

    public static byte[] Encode(in FrameHeader header, ReadOnlySpan<byte> body)
    {
        int length = FrameHeader.Size + body.Length;
        if (length > MaxFrameLength) throw new ProtocolException($"frame of {length} bytes exceeds the {MaxFrameLength} byte limit");
        var buf = new byte[LengthFieldSize + length];
        BinaryPrimitives.WriteUInt32BigEndian(buf, (uint)length);
        header.Write(buf.AsSpan(LengthFieldSize));
        body.CopyTo(buf.AsSpan(LengthFieldSize + FrameHeader.Size));
        return buf;
    }

    public static byte[] Encode(Frame frame) => Encode(frame.Header, frame.Body);

    /// <summary>Decodes one complete length-prefixed frame from a buffer. Returns null when <paramref name="buffer"/>
    /// does not yet hold a whole frame; <paramref name="consumed"/> is then 0.</summary>
    public static Frame? TryDecode(ReadOnlySpan<byte> buffer, out int consumed)
    {
        consumed = 0;
        if (buffer.Length < LengthFieldSize) return null;
        uint length = BinaryPrimitives.ReadUInt32BigEndian(buffer);
        if (length < FrameHeader.Size) throw new ProtocolException($"frame length {length} is shorter than a header");
        if (length > MaxFrameLength) throw new ProtocolException($"frame length {length} exceeds the {MaxFrameLength} byte limit");
        int total = LengthFieldSize + (int)length;
        if (buffer.Length < total) return null;
        var header = FrameHeader.Read(buffer.Slice(LengthFieldSize, FrameHeader.Size));
        var body = buffer.Slice(LengthFieldSize + FrameHeader.Size, (int)length - FrameHeader.Size).ToArray();
        consumed = total;
        return new Frame(header, body);
    }

    public static async ValueTask WriteAsync(Stream stream, Frame frame, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(stream);
        ArgumentNullException.ThrowIfNull(frame);
        await stream.WriteAsync(Encode(frame), ct).ConfigureAwait(false);
    }

    /// <summary>Reads one frame. Returns null on a clean end of stream (no bytes of a next frame read); throws
    /// <see cref="ProtocolException"/> on a bad length or a stream that ends inside a frame.</summary>
    public static async ValueTask<Frame?> ReadAsync(Stream stream, CancellationToken ct = default)
    {
        ArgumentNullException.ThrowIfNull(stream);
        var lenBuf = new byte[LengthFieldSize];
        int got = await ReadUpToAsync(stream, lenBuf, ct).ConfigureAwait(false);
        if (got == 0) return null;
        if (got < LengthFieldSize) throw new ProtocolException("stream ended inside a frame length");
        uint length = BinaryPrimitives.ReadUInt32BigEndian(lenBuf);
        if (length < FrameHeader.Size) throw new ProtocolException($"frame length {length} is shorter than a header");
        if (length > MaxFrameLength) throw new ProtocolException($"frame length {length} exceeds the {MaxFrameLength} byte limit");
        var rest = new byte[length];
        await stream.ReadExactlyAsync(rest, ct).ConfigureAwait(false);
        return new Frame(FrameHeader.Read(rest), rest.AsSpan(FrameHeader.Size).ToArray());
    }

    static async ValueTask<int> ReadUpToAsync(Stream stream, Memory<byte> buf, CancellationToken ct)
    {
        int total = 0;
        while (total < buf.Length)
        {
            int n = await stream.ReadAsync(buf[total..], ct).ConfigureAwait(false);
            if (n == 0) break;
            total += n;
        }
        return total;
    }
}
