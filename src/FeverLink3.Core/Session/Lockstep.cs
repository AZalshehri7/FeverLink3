namespace FeverLink3.Session;

/// <summary>Game-time arithmetic shared by host and players.</summary>
public static class Lockstep
{
    /// <summary>Game-time units per simulation step.</summary>
    public const long Step = 200;
    /// <summary>Game-time units per real second at speed 1.</summary>
    public const double UnitsPerSecond = 1000;

    public static long RoundUpToStep(double t) => (long)Math.Ceiling(t / Step) * Step;

    /// <summary>Steps a stamp must lie ahead of the fastest clock so that every game receives the message before
    /// reaching it: a fixed reserve plus two steps per speed unit for the engine's batch.</summary>
    public static long LeadUnits(int speed) => Step * (4 + 2 * Math.Max(1, speed));
}

/// <summary>A player's last reported clock, extrapolated with its speed.</summary>
public sealed class PlayerClock
{
    public long T { get; init; }
    public int Speed { get; init; }
    public double At { get; init; }

    public double Estimate(double now) => T + Math.Max(0, now - At) * Speed * Lockstep.UnitsPerSecond;
}
