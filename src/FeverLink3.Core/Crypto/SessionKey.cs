using System.Security.Cryptography;
using System.Text;

namespace FeverLink3.Crypto;

/// <summary>
/// The one symmetric key of a session, derived from the join code's secret and the optional lobby password.
/// <para>
/// <c>ikm = secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations, 32 bytes)</c> when a
/// password is set, else <c>ikm = secret</c>; <c>key = HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session", 32)</c>.
/// </para>
/// The stretching makes a leaked code without its password expensive to brute-force. No forward secrecy: the key
/// lives for the session, as the architecture document states.
/// </summary>
public sealed class SessionKey
{
    public const int SecretSize = 16;
    public const int KeySize = 32;
    public const int PasswordIterations = 600_000;

    static readonly byte[] Salt = Encoding.ASCII.GetBytes("FeverLink3 v1");
    static readonly byte[] Info = Encoding.ASCII.GetBytes("session");

    SessionKey(byte[] key) => Key = key;

    /// <summary>32 key bytes. Treat as secret; never log.</summary>
    public byte[] Key { get; }

    public static byte[] NewSecret() => RandomNumberGenerator.GetBytes(SecretSize);

    public static SessionKey Derive(ReadOnlySpan<byte> secret, string? password = null)
    {
        if (secret.Length != SecretSize) throw new ArgumentException($"secret must be {SecretSize} bytes", nameof(secret));
        byte[] ikm;
        if (string.IsNullOrEmpty(password))
        {
            ikm = secret.ToArray();
        }
        else
        {
            var stretched = Rfc2898DeriveBytes.Pbkdf2(Encoding.UTF8.GetBytes(password), secret.ToArray(), PasswordIterations, HashAlgorithmName.SHA256, KeySize);
            ikm = new byte[SecretSize + KeySize];
            secret.CopyTo(ikm);
            stretched.CopyTo(ikm.AsSpan(SecretSize));
        }
        var key = HKDF.DeriveKey(HashAlgorithmName.SHA256, ikm, KeySize, Salt, Info);
        return new SessionKey(key);
    }
}
