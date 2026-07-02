using System.Management.Automation;
using System.Globalization;
using System.Text;

namespace FastFs.PowerShell;

[Cmdlet(VerbsCommon.Select, "FastFsHead", DefaultParameterSetName = "Lines")]
[Alias("head")]
[OutputType(typeof(object), typeof(string))]
public sealed class SelectFastFsHeadCommand : PSCmdlet
{
    private StringBuilder? _characterBuffer;
    private long _emittedLines;
    private long _usedCharacters;
    private bool _hasCharacterInput;
    private bool _isCharacterMode;

    [Parameter(Mandatory = true, ValueFromPipeline = true)]
    public PSObject? InputObject { get; set; }

    [Parameter(Position = 0, ParameterSetName = "Lines")]
    [Alias("n")]
    [ValidateRange(0, long.MaxValue)]
    public long LineCount { get; set; } = 10;

    [Parameter(Mandatory = true, ParameterSetName = "Characters")]
    [Alias("c")]
    [ValidateRange(0, long.MaxValue)]
    public long CharacterCount { get; set; }

    protected override void BeginProcessing()
    {
        _isCharacterMode = ParameterSetName == "Characters";
        if (_isCharacterMode && CharacterCount > 0)
        {
            _characterBuffer = new StringBuilder((int)Math.Min(CharacterCount, 4096));
        }
    }

    protected override void ProcessRecord()
    {
        if (InputObject is null)
        {
            return;
        }

        if (!_isCharacterMode)
        {
            if (_emittedLines < LineCount)
            {
                WriteObject(InputObject);
                _emittedLines++;
            }
            if (_emittedLines >= LineCount)
            {
                MarkLimitReached();
            }
            return;
        }

        if (_usedCharacters >= CharacterCount)
        {
            MarkLimitReached();
            return;
        }

        if (_hasCharacterInput)
        {
            AppendCharacterPrefix(Environment.NewLine, CharacterCount - _usedCharacters);
        }
        if (_usedCharacters < CharacterCount)
        {
            var text = InputObject.BaseObject as string
                ?? LanguagePrimitives.ConvertTo<string>(InputObject)
                ?? string.Empty;
            AppendCharacterPrefix(text, CharacterCount - _usedCharacters);
        }
        _hasCharacterInput = true;
        if (_usedCharacters >= CharacterCount)
        {
            MarkLimitReached();
        }
    }

    protected override void EndProcessing()
    {
        if (_isCharacterMode && _characterBuffer is { Length: > 0 })
        {
            WriteObject(_characterBuffer.ToString());
        }
    }

    private void AppendCharacterPrefix(string value, long remainingCharacters)
    {
        if (remainingCharacters <= 0 || value.Length == 0)
        {
            return;
        }

        var buffer = _characterBuffer
            ?? throw new InvalidOperationException("文字バッファが初期化されていません");
        var span = value.AsSpan();
        var asciiOffset = 0;
        var asciiCount = 0L;
        while (asciiOffset < span.Length && asciiCount < remainingCharacters)
        {
            if (span[asciiOffset] > 0x7f)
            {
                break;
            }
            if (span[asciiOffset] == '\r'
                && asciiOffset + 1 < span.Length
                && span[asciiOffset + 1] == '\n')
            {
                asciiOffset += 2;
            }
            else
            {
                asciiOffset++;
            }
            asciiCount++;
        }
        var asciiPrefixIsComplete = asciiOffset == span.Length
            || (asciiCount == remainingCharacters && span[asciiOffset] <= 0x7f);
        if (asciiPrefixIsComplete)
        {
            buffer.Append(span[..asciiOffset]);
            _usedCharacters += asciiCount;
            return;
        }

        var offset = 0;
        var elementCount = 0L;
        while (offset < span.Length && elementCount < remainingCharacters)
        {
            offset += StringInfo.GetNextTextElementLength(span[offset..]);
            elementCount++;
        }
        if (offset > 0)
        {
            buffer.Append(span[..offset]);
            _usedCharacters += elementCount;
        }
    }

    private void MarkLimitReached()
        => FastFsHeadCoordinator.SignalCurrentProducer();
}
