using System.Text;
using FeverLink3.Crypto;
using FeverLink3.Protocol;
using Xunit;

namespace FeverLink3.Tests;

public class SealedChannelTests
{
    static readonly byte[] Secret = Enumerable.Range(1, 16).Select(i => (byte)i).ToArray();

    [Fact]
    public void Same_secret_and_password_give_the_same_key_and_different_ones_do_not()
    {
        var a = SessionKey.Derive(Secret);
        var b = SessionKey.Derive(Secret);
        var c = SessionKey.Derive(Secret, "hunter2");
        var d = SessionKey.Derive(SessionKey.NewSecret());
        Assert.Equal(a.Key, b.Key);
        Assert.NotEqual(a.Key, c.Key);
        Assert.NotEqual(a.Key, d.Key);
        Assert.Equal(SessionKey.KeySize, a.Key.Length);
    }

    [Fact]
    public void Sealed_frame_opens_to_the_same_plaintext()
    {
        var key = SessionKey.Derive(Secret);
        using var sealer = new Sealer(key, sender: 5);
        using var opener = new Opener(key);
        var text = Encoding.UTF8.GetBytes("{\"kind\":\"hello\"}");
        var frame = sealer.Seal(kind: 3, text);

        Assert.Equal((byte)3, frame.Header.Kind);
        Assert.Equal((ushort)5, frame.Header.Sender);
        Assert.Equal(1UL, frame.Header.Counter);
        Assert.Equal(text.Length + Sealer.TagSize, frame.Body.Length);
        Assert.True(opener.TryOpen(frame, out var plain, out var err), err.ToString());
        Assert.Equal(text, plain);
    }

    [Fact]
    public void Sealed_frame_survives_the_wire_format()
    {
        var key = SessionKey.Derive(Secret, "pw");
        using var sealer = new Sealer(key, 1);
        using var opener = new Opener(key);
        var frame = sealer.Seal(9, new byte[] { 1, 2, 3 });
        var decoded = FrameCodec.TryDecode(FrameCodec.Encode(frame), out _);
        Assert.NotNull(decoded);
        Assert.True(opener.TryOpen(decoded!, out var plain, out _));
        Assert.Equal(new byte[] { 1, 2, 3 }, plain);
    }

    [Theory]
    [InlineData(0)]    // ciphertext
    [InlineData(-1)]   // tag
    public void Altered_body_is_tamper(int where)
    {
        var key = SessionKey.Derive(Secret);
        using var sealer = new Sealer(key, 1);
        using var opener = new Opener(key);
        var frame = sealer.Seal(1, new byte[] { 10, 20, 30, 40 });
        var body = (byte[])frame.Body.Clone();
        int idx = where >= 0 ? where : body.Length + where;
        body[idx] ^= 0x01;
        Assert.False(opener.TryOpen(new Frame(frame.Header, body), out _, out var err));
        Assert.Equal(OpenError.Tamper, err);
    }

    [Fact]
    public void Altered_header_kind_or_sender_is_tamper()
    {
        var key = SessionKey.Derive(Secret);
        using var sealer = new Sealer(key, 1);
        using var opener = new Opener(key);
        var frame = sealer.Seal(1, new byte[] { 1 });
        var h = frame.Header;
        Assert.False(opener.TryOpen(new Frame(h with { Kind = 2 }, frame.Body), out _, out var e1));
        Assert.Equal(OpenError.Tamper, e1);
        Assert.False(opener.TryOpen(new Frame(h with { Sender = 2 }, frame.Body), out _, out var e2));
        Assert.Equal(OpenError.Tamper, e2);
    }

    [Fact]
    public void Wrong_password_is_tamper()
    {
        using var sealer = new Sealer(SessionKey.Derive(Secret, "right"), 1);
        using var opener = new Opener(SessionKey.Derive(Secret, "wrong"));
        var frame = sealer.Seal(1, new byte[] { 1 });
        Assert.False(opener.TryOpen(frame, out _, out var err));
        Assert.Equal(OpenError.Tamper, err);
    }

    [Fact]
    public void Replayed_frame_is_refused_and_a_forged_frame_does_not_burn_its_counter()
    {
        var key = SessionKey.Derive(Secret);
        using var sealer = new Sealer(key, 1);
        using var opener = new Opener(key);
        var f1 = sealer.Seal(1, new byte[] { 1 });
        var f2 = sealer.Seal(1, new byte[] { 2 });

        // a forgery carrying counter 2 arrives first: refused, and counter 2 stays usable by the real frame
        var forged = (byte[])f2.Body.Clone();
        forged[0] ^= 0xFF;
        Assert.False(opener.TryOpen(new Frame(f2.Header, forged), out _, out var e0));
        Assert.Equal(OpenError.Tamper, e0);

        Assert.True(opener.TryOpen(f2, out _, out _));
        Assert.True(opener.TryOpen(f1, out _, out _));      // out of order within the window: fine
        Assert.False(opener.TryOpen(f1, out _, out var e1));
        Assert.Equal(OpenError.Replay, e1);
        Assert.False(opener.TryOpen(f2, out _, out var e2));
        Assert.Equal(OpenError.Replay, e2);
    }

    [Fact]
    public void Counters_older_than_the_window_are_refused()
    {
        var key = SessionKey.Derive(Secret);
        using var sealer = new Sealer(key, 1);
        using var opener = new Opener(key);
        var early = sealer.Seal(1, new byte[] { 1 });
        Frame? last = null;
        for (int i = 0; i < ReplayWindow.Size; i++) last = sealer.Seal(1, new byte[] { 2 });
        Assert.True(opener.TryOpen(last!, out _, out _));    // highest = 65
        Assert.False(opener.TryOpen(early, out _, out var err));
        Assert.Equal(OpenError.Replay, err);
    }

    [Fact]
    public void Senders_have_independent_counters()
    {
        var key = SessionKey.Derive(Secret);
        using var a = new Sealer(key, 1);
        using var b = new Sealer(key, 2);
        using var opener = new Opener(key);
        Assert.True(opener.TryOpen(a.Seal(1, new byte[] { 1 }), out _, out _));
        Assert.True(opener.TryOpen(b.Seal(1, new byte[] { 1 }), out _, out _));   // counter 1 again, other sender
    }

    [Fact]
    public void Other_version_or_short_body_is_refused_without_decrypting()
    {
        using var opener = new Opener(SessionKey.Derive(Secret));
        Assert.False(opener.TryOpen(new Frame(new FrameHeader(2, 1, 1, 1), new byte[32]), out _, out var e1));
        Assert.Equal(OpenError.BadVersion, e1);
        Assert.False(opener.TryOpen(new Frame(new FrameHeader(1, 1, 1, 1), new byte[15]), out _, out var e2));
        Assert.Equal(OpenError.TooShort, e2);
    }

    [Fact]
    public void Replay_window_bitmap_tracks_out_of_order_counters()
    {
        var w = new ReplayWindow();
        Assert.False(w.IsFresh(0));
        w.Accept(5);
        Assert.True(w.IsFresh(3));
        Assert.False(w.IsFresh(5));
        w.Accept(3);
        Assert.False(w.IsFresh(3));
        w.Accept(5 + ReplayWindow.Size);        // slides the window past 5
        Assert.False(w.IsFresh(5));
        Assert.True(w.IsFresh(6));
        Assert.Throws<InvalidOperationException>(() => w.Accept(5));
    }
}
