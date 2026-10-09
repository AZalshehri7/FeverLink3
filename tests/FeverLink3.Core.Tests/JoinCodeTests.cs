using System.Net;
using FeverLink3.Crypto;
using FeverLink3.Lobby;
using Xunit;

namespace FeverLink3.Tests;

public class JoinCodeTests
{
    static readonly byte[] Secret = Enumerable.Range(100, 16).Select(i => (byte)i).ToArray();

    [Fact]
    public void Code_round_trips_with_ipv4_and_ipv6_endpoints()
    {
        var eps = new[] { new IPEndPoint(IPAddress.Parse("192.168.1.20"), 28090), new IPEndPoint(IPAddress.Parse("2001:db8::1"), 28091) };
        var code = JoinCode.Create(eps, Secret, new DateTimeOffset(2026, 10, 9, 12, 0, 0, TimeSpan.Zero));
        var text = code.Encode();
        var back = JoinCode.TryDecode(text);
        Assert.NotNull(back);
        Assert.Equal(code, back);
        Assert.Equal(2, back!.Endpoints.Count);
        Assert.Equal(28091, back.Endpoints[1].Port);
    }

    [Fact]
    public void One_ipv4_endpoint_is_53_characters()
    {
        var code = JoinCode.Create(new[] { new IPEndPoint(IPAddress.Parse("89.12.34.56"), 28090) }, Secret);
        Assert.Equal(53, code.Encode().Length);
    }

    [Fact]
    public void Typing_variations_are_accepted()
    {
        var code = JoinCode.Create(new[] { new IPEndPoint(IPAddress.Loopback, 1) }, Secret);
        var text = code.Encode();
        var typed = string.Concat(text.ToLowerInvariant().Select((c, i) => i % 5 == 4 ? c + "-" : c.ToString()));
        Assert.Equal(code, JoinCode.TryDecode(" " + typed + " "));
    }

    [Fact]
    public void A_changed_character_is_refused()
    {
        var text = JoinCode.Create(new[] { new IPEndPoint(IPAddress.Loopback, 1) }, Secret).Encode();
        for (int i = 0; i < text.Length; i++)
        {
            char c = text[i] == 'A' ? 'B' : 'A';
            var broken = text[..i] + c + text[(i + 1)..];
            Assert.Null(JoinCode.TryDecode(broken));
        }
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("not a code")]
    [InlineData("ABCDEFG")]
    [InlineData("A1")]
    public void Garbage_is_null_not_an_exception(string? text)
    {
        Assert.Null(JoinCode.TryDecode(text));
    }

    [Fact]
    public void Secret_feeds_the_session_key()
    {
        var code = JoinCode.Create(Array.Empty<IPEndPoint>(), Secret);
        var back = JoinCode.TryDecode(code.Encode())!;
        Assert.Equal(SessionKey.Derive(Secret).Key, SessionKey.Derive(back.Secret).Key);
    }

    [Fact]
    public void Base32_matches_rfc_4648_vectors()
    {
        Assert.Equal("MZXW6YTBOI", JoinCode.Base32Encode("foobar"u8));
        Assert.Equal("foobar"u8.ToArray(), JoinCode.Base32Decode("mzxw6ytboi======"));
        Assert.Equal("MY", JoinCode.Base32Encode("f"u8));
        Assert.Null(JoinCode.Base32Decode("M1"));
    }
}
