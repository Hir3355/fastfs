using System.Management.Automation;

namespace FastFs.PowerShell;

public sealed class FastFsEntry
{
    private string? _name;
    private DateTimeOffset? _modified;
    private bool _modifiedInitialized;

    public required string Kind { get; init; }
    public long? Length { get; init; }
    internal long? ModifiedUnixMilliseconds { get; init; }
    public DateTimeOffset? Modified
    {
        get
        {
            if (!_modifiedInitialized)
            {
                _modified = ModifiedUnixMilliseconds is long value
                    ? DateTimeOffset.FromUnixTimeMilliseconds(value)
                    : null;
                _modifiedInitialized = true;
            }
            return _modified;
        }
    }
    public required string Path { get; init; }

    [Hidden]
    public string Name => _name ??= GetName(Path);

    [Hidden]
    public bool IsReadOnly { get; init; }

    [Hidden]
    public bool IsDirectory => string.Equals(Kind, "directory", StringComparison.Ordinal);

    public override string ToString() => Path;

    private static string GetName(string path)
    {
        var trimmedPath = System.IO.Path.TrimEndingDirectorySeparator(path);
        var name = System.IO.Path.GetFileName(trimmedPath);
        return string.IsNullOrEmpty(name) ? path : name;
    }
}
