using System.Buffers.Binary;
using FeverLink3.Protocol;
using Xunit;

namespace FeverLink3.Tests;

public class FrameTests
{
    [Fact]
    public void Header_round_trips()
    {
        var h = new FrameHeader(1, 0x2A, 513, 0x0102030405060708UL);
        var b = h.ToBytes();
        Assert.Equal(FrameHeader.Size, b.Length);
        Assert.Equal(h, FrameHeader.Read(b));
    }

    [Fact]
    public async Task Frame_round_trips_through_a_stream()
    {
        var body = new byte[1000];
        Random.Shared.NextBytes(body);
        var frame = new Frame(new FrameHeader(1, 7, 3, 42), body);
        var ms = new MemoryStream();
        await FrameCodec.WriteAsync(ms, frame);
        await FrameCodec.WriteAsync(ms, new Frame(new FrameHeader(1, 8, 3, 43), Array.Empty<byte>()));
        ms.Position = 0;

        var first = await FrameCodec.ReadAsync(ms);
        var second = await FrameCodec.ReadAsync(ms);
        var end = await FrameCodec.ReadAsync(ms);

        Assert.NotNull(first);
        Assert.Equal(frame.Header, first!.Header);
        Assert.Equal(body, first.Body);
        Assert.NotNull(second);
        Assert.Equal((byte)8, second!.Header.Kind);
        Assert.Empty(second.Body);
        Assert.Null(end);
    }

    [Fact]
    public void TryDecode_waits_for_a_whole_frame_then_reports_what_it_consumed()
    {
        var bytes = FrameCodec.Encode(new FrameHeader(1, 1, 1, 1), new byte[] { 9, 9, 9 });
        Assert.Null(FrameCodec.TryDecode(bytes.AsSpan(0, bytes.Length - 1), out int consumed));
        Assert.Equal(0, consumed);
        var two = bytes.Concat(bytes).ToArray();
        var f = FrameCodec.TryDecode(two, out consumed);
        Assert.NotNull(f);
        Assert.Equal(bytes.Length, consumed);
        Assert.Equal(new byte[] { 9, 9, 9 }, f!.Body);
    }

    [Fact]
    public async Task Oversized_length_is_refused_before_any_allocation()
    {
        var buf = new byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(buf, (uint)FrameCodec.MaxFrameLength + 1);
        var ms = new MemoryStream(buf);
        await Assert.ThrowsAsync<ProtocolException>(async () => await FrameCodec.ReadAsync(ms));
        Assert.Throws<ProtocolException>(() => FrameCodec.TryDecode(buf, out _));
    }

    [Fact]
    public async Task Length_shorter_than_a_header_is_refused()
    {
        var buf = new byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(buf, 3);
        await Assert.ThrowsAsync<ProtocolException>(async () => await FrameCodec.ReadAsync(new MemoryStream(buf)));
    }

    [Fact]
    public async Task Stream_ending_inside_a_frame_is_an_error_not_a_clean_end()
    {
        var bytes = FrameCodec.Encode(new FrameHeader(1, 1, 1, 1), new byte[100]);
        var ms = new MemoryStream(bytes, 0, bytes.Length - 10);
        await Assert.ThrowsAnyAsync<Exception>(async () => await FrameCodec.ReadAsync(ms));
    }

    [Fact]
    public void Encode_refuses_a_body_over_the_limit()
    {
        Assert.Throws<ProtocolException>(() => FrameCodec.Encode(new FrameHeader(1, 1, 1, 1), new byte[FrameCodec.MaxFrameLength]));
    }
}
