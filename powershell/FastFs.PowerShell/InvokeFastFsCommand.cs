using System.Buffers.Binary;
using System.Management.Automation;
using System.Runtime.ExceptionServices;
using System.Text;

namespace FastFs.PowerShell;

[Cmdlet(VerbsLifecycle.Invoke, "FastFs")]
[Alias("fastfs")]
[OutputType(typeof(FastFsEntry), typeof(string))]
public sealed class InvokeFastFsCommand : PSCmdlet
{
    private List<string>? _pipelineOperands;
    private Exception? _callbackException;
    private int _reportedErrorCount;
    private int _stopRequested;
    private readonly FastFsProducerState _producerState = new();
    private readonly object _cancellationLock = new();
    private NativeMethods.FastFsCancellationHandle? _cancellation;

    [Parameter(Mandatory = true, Position = 0)]
    [ValidateSet("ls", "touch", "find", "cat", "sed", "rg")]
    public string Command { get; set; } = string.Empty;

    [Parameter(Position = 1, ValueFromRemainingArguments = true)]
    public string[] Operand { get; set; } = [];

    [Parameter(ValueFromPipeline = true)]
    public PSObject? InputObject { get; set; }

    [Parameter]
    [Alias("a")]
    public SwitchParameter All { get; set; }

    [Parameter]
    [Alias("R")]
    public SwitchParameter Recursive { get; set; }

    [Parameter]
    public string? Name { get; set; }

    [Parameter]
    public string? Type { get; set; }

    [Parameter]
    [Alias("n")]
    public SwitchParameter SuppressAutomaticOutput { get; set; }

    [Parameter]
    [Alias("i")]
    public SwitchParameter IgnoreCase { get; set; }

    [Parameter]
    [Alias("S")]
    public SwitchParameter SmartCase { get; set; }

    [Parameter]
    [Alias("F")]
    public SwitchParameter FixedStrings { get; set; }

    [Parameter]
    [Alias("w")]
    public SwitchParameter WordRegexp { get; set; }

    [Parameter]
    [Alias("x")]
    public SwitchParameter LineRegexp { get; set; }

    [Parameter]
    [Alias("C")]
    [ValidateRange(0, int.MaxValue)]
    public int? Context { get; set; }

    [Parameter]
    [Alias("B")]
    [ValidateRange(0, int.MaxValue)]
    public int? BeforeContext { get; set; }

    [Parameter]
    [ValidateRange(0, int.MaxValue)]
    public int? AfterContext { get; set; }

    [Parameter]
    [Alias("g")]
    public string[]? Glob { get; set; }

    [Parameter]
    [Alias("m")]
    [ValidateRange(0, long.MaxValue)]
    public long? MaxCount { get; set; }

    [Parameter]
    public SwitchParameter Hidden { get; set; }

    [Parameter]
    public SwitchParameter NoIgnore { get; set; }

    [Parameter]
    public SwitchParameter Follow { get; set; }

    [Parameter]
    public SwitchParameter Text { get; set; }

    [Parameter]
    public SwitchParameter FilesWithMatches { get; set; }

    [Parameter]
    public SwitchParameter Count { get; set; }

    [Parameter]
    [Alias("e")]
    public string? Pattern { get; set; }

    protected override void ProcessRecord()
    {
        if (InputObject is null)
        {
            return;
        }

        var value = InputObject.BaseObject switch
        {
            FileSystemInfo fileSystemInfo => fileSystemInfo.FullName,
            FastFsEntry entry => entry.Path,
            _ => LanguagePrimitives.ConvertTo<string>(InputObject)
        };
        if (!string.IsNullOrEmpty(value))
        {
            (_pipelineOperands ??= []).Add(value);
        }
    }

    protected override void EndProcessing()
    {
        var command = NormalizeCommand(Command);
        var args = new List<string>(Operand.Length + (_pipelineOperands?.Count ?? 0) + 20);
        AppendOptions(command, args);
        args.AddRange(Operand);
        if (_pipelineOperands is not null)
        {
            args.AddRange(_pipelineOperands);
        }
        var request = BuildRequest(command, args);
        var callback = new NativeMethods.EventCallback(ReceiveEvent);
        using var cancellation = NativeMethods.CreateCancellation();
        lock (_cancellationLock)
        {
            _cancellation = cancellation;
            if (Volatile.Read(ref _stopRequested) != 0)
            {
                cancellation.Cancel();
            }
        }

        int result;
        try
        {
            result = NativeMethods.Execute(
                request,
                (UIntPtr)request.Length,
                callback,
                IntPtr.Zero,
                cancellation);
        }
        finally
        {
            lock (_cancellationLock)
            {
                if (ReferenceEquals(_cancellation, cancellation))
                {
                    _cancellation = null;
                }
            }
        }
        GC.KeepAlive(callback);

        if (_callbackException is not null)
        {
            ExceptionDispatchInfo.Capture(_callbackException).Throw();
        }
        var expectedResult = result == 0
            || (result == 1 && _reportedErrorCount > 0)
            || (result == 130
                && (_producerState.LimitReached || Volatile.Read(ref _stopRequested) != 0));
        if (!expectedResult)
        {
            var exception = new InvalidOperationException($"fastfs.dll の実行に失敗しました。終了コード: {result}");
            ThrowTerminatingError(new ErrorRecord(
                exception,
                "FastFs.NativeExecutionFailed",
                ErrorCategory.NotSpecified,
                Command));
        }
    }

    protected override void StopProcessing()
    {
        Interlocked.Exchange(ref _stopRequested, 1);
        lock (_cancellationLock)
        {
            if (_cancellation is { IsClosed: false, IsInvalid: false })
            {
                _cancellation.Cancel();
            }
        }
    }

    private static string NormalizeCommand(string command) => command switch
    {
        "ls" => "ls",
        "touch" => "touch",
        "find" => "find",
        "cat" => "cat",
        "sed" => "sed",
        "rg" => "rg",
        _ when command.Equals("ls", StringComparison.OrdinalIgnoreCase) => "ls",
        _ when command.Equals("touch", StringComparison.OrdinalIgnoreCase) => "touch",
        _ when command.Equals("find", StringComparison.OrdinalIgnoreCase) => "find",
        _ when command.Equals("cat", StringComparison.OrdinalIgnoreCase) => "cat",
        _ when command.Equals("sed", StringComparison.OrdinalIgnoreCase) => "sed",
        _ when command.Equals("rg", StringComparison.OrdinalIgnoreCase) => "rg",
        _ => command
    };

    private void AppendOptions(string command, List<string> args)
    {
        if (command == "ls")
        {
            if (All)
            {
                args.Add("-a");
            }
            if (Recursive)
            {
                args.Add("-R");
            }
            return;
        }
        if (command == "find")
        {
            if (Name is not null)
            {
                args.Add("-name");
                args.Add(Name);
            }
            if (Type is not null)
            {
                args.Add("-type");
                args.Add(Type);
            }
            return;
        }
        if (command == "sed")
        {
            if (SuppressAutomaticOutput)
            {
                args.Add("-n");
            }
            return;
        }
        if (command != "rg")
        {
            return;
        }

        if (SuppressAutomaticOutput)
        {
            args.Add("-n");
        }
        if (IgnoreCase) args.Add("-i");
        if (SmartCase) args.Add("-S");
        if (FixedStrings) args.Add("-F");
        if (WordRegexp) args.Add("-w");
        if (LineRegexp) args.Add("-x");
        if (All || Text) args.Add("-a");
        if (Recursive || Follow) args.Add("-L");
        if (Hidden) args.Add("--hidden");
        if (NoIgnore) args.Add("--no-ignore");
        if (FilesWithMatches) args.Add("--files-with-matches");
        if (Count) args.Add("--count");
        if (Pattern is not null)
        {
            args.Add("-e");
            args.Add(Pattern);
        }
        if (Context.HasValue)
        {
            args.Add("-C");
            args.Add(Context.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
        }
        if (BeforeContext.HasValue)
        {
            args.Add("-B");
            args.Add(BeforeContext.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
        }
        if (AfterContext.HasValue)
        {
            args.Add("-A");
            args.Add(AfterContext.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
        }
        if (Glob is not null)
        {
            foreach (var glob in Glob)
            {
                args.Add("-g");
                args.Add(glob);
            }
        }
        if (MaxCount.HasValue)
        {
            args.Add("-m");
            args.Add(MaxCount.Value.ToString(System.Globalization.CultureInfo.InvariantCulture));
        }
    }

    private static byte[] BuildRequest(string command, IReadOnlyList<string> args)
    {
        var totalLength = 8 + sizeof(uint) + Encoding.UTF8.GetByteCount(command);
        foreach (var arg in args)
        {
            totalLength = checked(totalLength + sizeof(uint) + Encoding.UTF8.GetByteCount(arg));
        }

        var request = new byte[totalLength];
        "FFS1"u8.CopyTo(request);
        BinaryPrimitives.WriteUInt32LittleEndian(request.AsSpan(4), checked((uint)args.Count + 1));
        var offset = 8;
        WriteRequestString(request, ref offset, command);
        foreach (var arg in args)
        {
            WriteRequestString(request, ref offset, arg);
        }
        return request;
    }

    private static void WriteRequestString(byte[] request, ref int offset, string value)
    {
        var byteCount = Encoding.UTF8.GetByteCount(value);
        BinaryPrimitives.WriteUInt32LittleEndian(request.AsSpan(offset), checked((uint)byteCount));
        offset += sizeof(uint);
        offset += Encoding.UTF8.GetBytes(value.AsSpan(), request.AsSpan(offset, byteCount));
    }

    private unsafe int ReceiveEvent(IntPtr data, UIntPtr length, IntPtr context)
    {
        try
        {
            var byteLength = checked((int)length.ToUInt64());
            if (data == IntPtr.Zero || byteLength == 0)
            {
                throw new InvalidDataException("fastfs.dll から空のイベントを受信しました");
            }

            var reader = new WireReader(new ReadOnlySpan<byte>((void*)data, byteLength));
            var fullyRead = reader.ReadByte() switch
            {
                1 => ReadEntryBatch(ref reader),
                2 => ReadTextBatch(ref reader),
                3 => ReadNativeError(ref reader),
                _ => throw new InvalidDataException("fastfs.dll から不明なイベントを受信しました")
            };
            if (!fullyRead)
            {
                return 1;
            }
            reader.EnsureFinished();
            return 0;
        }
        catch (Exception exception)
        {
            _callbackException = exception;
            return 1;
        }
    }

    private bool ReadEntryBatch(ref WireReader reader)
    {
        var count = reader.ReadUInt32();
        for (var index = 0U; index < count; index++)
        {
            var kindValue = reader.ReadByte();
            var flags = reader.ReadByte();
            _ = reader.ReadUInt16();
            var length = reader.ReadUInt64();
            var modified = reader.ReadUInt64();
            var path = reader.ReadString();
            var previous = FastFsHeadCoordinator.EnterNativeProducer(_producerState);
            try
            {
                WriteObject(new FastFsEntry
                {
                    Path = path,
                    Kind = kindValue switch
                    {
                        1 => "file",
                        2 => "directory",
                        3 => "symlink",
                        _ => "other"
                    },
                    Length = (flags & 1) == 0 ? null : checked((long)length),
                    ModifiedUnixMilliseconds = (flags & 2) == 0
                        ? null
                        : checked((long)modified),
                    IsReadOnly = (flags & 4) != 0
                });
            }
            finally
            {
                FastFsHeadCoordinator.ExitNativeProducer(previous);
            }
            if (_producerState.LimitReached)
            {
                return false;
            }
        }
        return true;
    }

    private bool ReadTextBatch(ref WireReader reader)
    {
        var count = reader.ReadUInt32();
        for (var index = 0U; index < count; index++)
        {
            var text = reader.ReadString();
            var previous = FastFsHeadCoordinator.EnterNativeProducer(_producerState);
            try
            {
                WriteObject(text);
            }
            finally
            {
                FastFsHeadCoordinator.ExitNativeProducer(previous);
            }
            if (_producerState.LimitReached)
            {
                return false;
            }
        }
        return true;
    }

    private bool ReadNativeError(ref WireReader reader)
    {
        _reportedErrorCount++;
        var code = reader.ReadString();
        var categoryName = reader.ReadString();
        var message = reader.ReadString();
        var path = reader.ReadOptionalString();
        var exception = new IOException(message);
        var previous = FastFsHeadCoordinator.EnterNativeProducer(_producerState);
        try
        {
            WriteError(new ErrorRecord(
                exception,
                $"FastFs.{code}",
                ParseCategory(categoryName),
                path));
        }
        finally
        {
            FastFsHeadCoordinator.ExitNativeProducer(previous);
        }
        return !_producerState.LimitReached;
    }

    private static ErrorCategory ParseCategory(string? category) => category switch
    {
        "InvalidArgument" => ErrorCategory.InvalidArgument,
        "ObjectNotFound" => ErrorCategory.ObjectNotFound,
        "PermissionDenied" => ErrorCategory.PermissionDenied,
        "ReadError" => ErrorCategory.ReadError,
        "WriteError" => ErrorCategory.WriteError,
        _ => ErrorCategory.NotSpecified
    };
}
