using System.Buffers.Binary;
using System.Security.Cryptography;
using FeverLink3.Protocol;

namespace FeverLink3.Crypto;

/// <summary>Why a frame was refused by <see cref="Opener.TryOpen"/>.</summary>
public enum OpenError
{
    None = 0,
    /// <summary>Header version is not the one this build speaks.</summary>
    BadVersion,
    /// <summary>Body shorter than a GCM tag.</summary>
    TooShort,
    /// <summary>Counter already accepted from this sender, or older than the replay window.</summary>
    Replay,
    /// <summary>Authentication failed: wrong key, altered header or body.</summary>
    Tamper,
}

/// <summary>
/// Seals outgoing frames for one sender: AES-256-GCM, 16-byte tag, nonce = <c>u32 sender ‖ u64 counter</c>.
/// The counter starts at 1 and only grows, so a nonce is never reused under a key as long as every sender id is
/// unique in the session, which the host guarantees by assigning them.
/// </summary>
public sealed class Sealer : IDisposable
{
    public const int NonceSize = 12;
    public const int TagSize = 16;

    readonly AesGcm aes;
    readonly ushort sender;
    ulong counter;

    public Sealer(SessionKey key, ushort sender)
    {
        ArgumentNullException.ThrowIfNull(key);
        aes = new AesGcm(key.Key, TagSize);
        this.sender = sender;
    }

    public ushort Sender => sender;
    public ulong LastCounter => counter;

    public Frame Seal(byte kind, ReadOnlySpan<byte> plaintext)
    {
        ulong c = ++counter;
        var header = new FrameHeader(FrameHeader.CurrentVersion, kind, sender, c);
        Span<byte> nonce = stackalloc byte[NonceSize];
        WriteNonce(nonce, sender, c);
        var body = new byte[plaintext.Length + TagSize];
        Span<byte> aad = stackalloc byte[FrameHeader.Size];
        header.Write(aad);
        aes.Encrypt(nonce, plaintext, body.AsSpan(0, plaintext.Length), body.AsSpan(plaintext.Length, TagSize), aad);
        return new Frame(header, body);
    }

    internal static void WriteNonce(Span<byte> nonce, ushort sender, ulong counter)
    {
        BinaryPrimitives.WriteUInt32BigEndian(nonce, sender);
        BinaryPrimitives.WriteUInt64BigEndian(nonce[4..], counter);
    }

    public void Dispose() => aes.Dispose();
}

/// <summary>
/// Opens incoming frames from any sender and refuses replays. A frame is authenticated before its counter is
/// recorded, so a forged frame cannot poison the window.
/// </summary>
public sealed class Opener : IDisposable
{
    readonly AesGcm aes;
    readonly Dictionary<ushort, ReplayWindow> windows = new();

    public Opener(SessionKey key)
    {
        ArgumentNullException.ThrowIfNull(key);
        aes = new AesGcm(key.Key, Sealer.TagSize);
    }

    public bool TryOpen(Frame frame, out byte[] plaintext, out OpenError error)
    {
        ArgumentNullException.ThrowIfNull(frame);
        plaintext = Array.Empty<byte>();
        var h = frame.Header;
        if (h.Version != FrameHeader.CurrentVersion) { error = OpenError.BadVersion; return false; }
        if (frame.Body.Length < Sealer.TagSize) { error = OpenError.TooShort; return false; }
        if (!windows.TryGetValue(h.Sender, out var window)) windows[h.Sender] = window = new ReplayWindow();
        if (!window.IsFresh(h.Counter)) { error = OpenError.Replay; return false; }

        Span<byte> nonce = stackalloc byte[Sealer.NonceSize];
        Sealer.WriteNonce(nonce, h.Sender, h.Counter);
        Span<byte> aad = stackalloc byte[FrameHeader.Size];
        h.Write(aad);
        int n = frame.Body.Length - Sealer.TagSize;
        var output = new byte[n];
        try
        {
            aes.Decrypt(nonce, frame.Body.AsSpan(0, n), frame.Body.AsSpan(n, Sealer.TagSize), output, aad);
        }
        catch (CryptographicException)
        {
            error = OpenError.Tamper;
            return false;
        }
        window.Accept(h.Counter);
        plaintext = output;
        error = OpenError.None;
        return true;
    }

    public void Dispose() => aes.Dispose();
}

/// <summary>Sliding window of the last 64 counters accepted from one sender (RFC 6479 style bitmap).</summary>
public sealed class ReplayWindow
{
    public const int Size = 64;
    ulong highest;      // highest counter accepted; 0 = none yet
    ulong bitmap;       // bit k set = counter (highest - k) accepted

    public ulong Highest => highest;

    public bool IsFresh(ulong counter)
    {
        if (counter == 0) return false;                 // counters start at 1
        if (counter > highest) return true;
        ulong back = highest - counter;
        if (back >= Size) return false;                 // older than the window: refused
        return (bitmap & (1UL << (int)back)) == 0;
    }

    public void Accept(ulong counter)
    {
        if (!IsFresh(counter)) throw new InvalidOperationException("counter not fresh");
        if (counter > highest)
        {
            ulong shift = counter - highest;
            bitmap = shift >= Size ? 0 : bitmap << (int)shift;
            highest = counter;
            bitmap |= 1UL;
        }
        else
        {
            bitmap |= 1UL << (int)(highest - counter);
        }
    }
}
