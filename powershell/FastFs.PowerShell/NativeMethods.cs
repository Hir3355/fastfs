using System.Reflection;
using System.Runtime.InteropServices;

namespace FastFs.PowerShell;

internal static class NativeMethods
{
    private const uint ExpectedAbiVersion = 3;
    private static readonly Lazy<uint> AbiVersion = new(GetAbiVersion);
    private static readonly object LibraryLock = new();
    private static IntPtr _libraryHandle;

    internal delegate int EventCallback(IntPtr data, UIntPtr length, IntPtr context);

    static NativeMethods()
    {
        NativeLibrary.SetDllImportResolver(typeof(NativeMethods).Assembly, ResolveLibrary);
    }

    [DllImport("fastfs", EntryPoint = "fastfs_abi_version", CallingConvention = CallingConvention.Winapi)]
    private static extern uint GetAbiVersion();

    [DllImport("fastfs", EntryPoint = "fastfs_execute_v3", CallingConvention = CallingConvention.Winapi)]
    private static extern int ExecuteV3(
        byte[] request,
        UIntPtr requestLength,
        EventCallback callback,
        IntPtr context,
        FastFsCancellationHandle cancellation);

    [DllImport("fastfs", EntryPoint = "fastfs_cancellation_create", CallingConvention = CallingConvention.Winapi)]
    private static extern FastFsCancellationHandle CreateCancellationNative();

    [DllImport("fastfs", EntryPoint = "fastfs_cancellation_cancel", CallingConvention = CallingConvention.Winapi)]
    private static extern void CancelCancellationNative(FastFsCancellationHandle cancellation);

    [DllImport("fastfs", EntryPoint = "fastfs_cancellation_destroy", CallingConvention = CallingConvention.Winapi)]
    private static extern void DestroyCancellationNative(IntPtr cancellation);

    internal static int Execute(
        byte[] request,
        UIntPtr requestLength,
        EventCallback callback,
        IntPtr context,
        FastFsCancellationHandle cancellation)
    {
        EnsureAbiVersion();
        return ExecuteV3(request, requestLength, callback, context, cancellation);
    }

    internal static FastFsCancellationHandle CreateCancellation()
    {
        EnsureAbiVersion();
        var cancellation = CreateCancellationNative();
        if (cancellation.IsInvalid)
        {
            cancellation.Dispose();
            throw new OutOfMemoryException("FastFs のキャンセルトークンを作成できませんでした");
        }
        return cancellation;
    }

    private static void EnsureAbiVersion()
    {
        uint abiVersion;
        try
        {
            abiVersion = AbiVersion.Value;
        }
        catch (EntryPointNotFoundException exception)
        {
            throw new BadImageFormatException(
                "FastFs.PowerShell.dll と fastfs.dll のABIが一致しません",
                exception);
        }
        if (abiVersion != ExpectedAbiVersion)
        {
            throw new BadImageFormatException(
                $"fastfs.dll のABI {abiVersion} は未対応です。必要なABI: {ExpectedAbiVersion}");
        }
    }

    internal sealed class FastFsCancellationHandle : SafeHandle
    {
        public FastFsCancellationHandle()
            : base(IntPtr.Zero, ownsHandle: true)
        {
        }

        public override bool IsInvalid => handle == IntPtr.Zero || handle == new IntPtr(-1);

        internal void Cancel()
            => CancelCancellationNative(this);

        protected override bool ReleaseHandle()
        {
            try
            {
                DestroyCancellationNative(handle);
                return true;
            }
            catch
            {
                return false;
            }
        }
    }

    private static IntPtr ResolveLibrary(
        string libraryName,
        Assembly assembly,
        DllImportSearchPath? searchPath)
    {
        if (!string.Equals(libraryName, "fastfs", StringComparison.OrdinalIgnoreCase))
        {
            return IntPtr.Zero;
        }

        if (_libraryHandle != IntPtr.Zero)
        {
            return _libraryHandle;
        }

        lock (LibraryLock)
        {
            if (_libraryHandle != IntPtr.Zero)
            {
                return _libraryHandle;
            }
            var moduleDirectory = Path.GetDirectoryName(assembly.Location)
                ?? throw new DllNotFoundException("FastFs モジュールの場所を取得できませんでした");
            var libraryPath = Path.Combine(moduleDirectory, "fastfs.dll");
            if (!NativeLibrary.TryLoad(libraryPath, out _libraryHandle))
            {
                throw new DllNotFoundException($"Rust ライブラリが見つからないか、読み込めません: {libraryPath}");
            }
            return _libraryHandle;
        }
    }
}
