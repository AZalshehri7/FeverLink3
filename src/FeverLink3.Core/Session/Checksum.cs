namespace FeverLink3.Session;

/// <summary>One part of the world whose hash differs between games, with each reporter's value (null = the
/// reporter did not hash that part).</summary>
public sealed record ChecksumDifference(string Part, IReadOnlyDictionary<ushort, string?> Values);

/// <summary>Result of one checksum round once every ready player has reported.</summary>
public sealed record ChecksumOutcome(int N, long Time, IReadOnlyList<ushort> Reporters, IReadOnlyList<ChecksumDifference> Differences)
{
    public bool Identical => Differences.Count == 0;
    public IEnumerable<string> DifferingParts => Differences.Select(d => d.Part);
}

/// <summary>Collects the reports of checksum rounds and compares them. Pure; the host drives it.</summary>
public sealed class ChecksumRounds
{
    readonly Dictionary<int, Dictionary<ushort, ChecksumReportMessage>> rounds = new();
    const int KeepRounds = 20;

    public void Open(int n) => rounds[n] = new Dictionary<ushort, ChecksumReportMessage>();

    /// <summary>Records a report. Returns the outcome when <paramref name="expected"/> reporters have all reported,
    /// else null. Reports for unknown rounds are ignored.</summary>
    public ChecksumOutcome? Report(ushort from, ChecksumReportMessage report, IReadOnlyCollection<ushort> expected)
    {
        ArgumentNullException.ThrowIfNull(report);
        ArgumentNullException.ThrowIfNull(expected);
        if (!rounds.TryGetValue(report.N, out var round)) return null;
        round[from] = report;
        foreach (var old in rounds.Keys.Where(k => k < report.N - KeepRounds).ToList()) rounds.Remove(old);
        if (!expected.All(round.ContainsKey)) return null;
        rounds.Remove(report.N);
        return Compare(report.N, round);
    }

    public static ChecksumOutcome Compare(int n, IReadOnlyDictionary<ushort, ChecksumReportMessage> reports)
    {
        ArgumentNullException.ThrowIfNull(reports);
        var reporters = reports.Keys.OrderBy(k => k).ToList();
        var parts = reports.Values.SelectMany(r => r.Parts.Keys).Distinct().OrderBy(p => p, StringComparer.Ordinal);
        var diffs = new List<ChecksumDifference>();
        foreach (var part in parts)
        {
            var values = reporters.ToDictionary(id => id, id => reports[id].Parts.TryGetValue(part, out var v) ? v : null);
            if (values.Values.Distinct().Count() > 1) diffs.Add(new ChecksumDifference(part, values));
        }
        long time = reporters.Count > 0 ? reports[reporters[0]].Time : 0;
        return new ChecksumOutcome(n, time, reporters, diffs);
    }
}
