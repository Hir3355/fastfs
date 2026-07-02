using System.Buffers.Binary;
using System.Text;

namespace FastFs.PowerShell;

internal ref struct WireReader
{
    private readonly ReadOnlySpan<byte> _data;
    private int _offset;

    internal WireReader(ReadOnlySpan<byte> data)
    {
        _data = data;
        _offset = 0;
    }

    internal byte ReadByte()
    {
        EnsureAvailable(1);
        return _data[_offset++];
    }

    internal ushort ReadUInt16()
    {
        EnsureAvailable(sizeof(ushort));
        var value = BinaryPrimitives.ReadUInt16LittleEndian(_data[_offset..]);
        _offset += sizeof(ushort);
        return value;
    }

    internal uint ReadUInt32()
    {
        EnsureAvailable(sizeof(uint));
        var value = BinaryPrimitives.ReadUInt32LittleEndian(_data[_offset..]);
        _offset += sizeof(uint);
        return value;
    }

    internal ulong ReadUInt64()
    {
        EnsureAvailable(sizeof(ulong));
        var value = BinaryPrimitives.ReadUInt64LittleEndian(_data[_offset..]);
        _offset += sizeof(ulong);
        return value;
    }

    internal string ReadString()
    {
        var length = checked((int)ReadUInt32());
        EnsureAvailable(length);
        var value = Encoding.UTF8.GetString(_data.Slice(_offset, length));
        _offset += length;
        return value;
    }

    internal string? ReadOptionalString()
    {
        var length = ReadUInt32();
        if (length == uint.MaxValue)
        {
            return null;
        }
        var byteLength = checked((int)length);
        EnsureAvailable(byteLength);
        var value = Encoding.UTF8.GetString(_data.Slice(_offset, byteLength));
        _offset += byteLength;
        return value;
    }

    internal void EnsureFinished()
    {
        if (_offset != _data.Length)
        {
            throw new InvalidDataException("fastfs.dll のイベント末尾に余分なデータがあります");
        }
    }

    private void EnsureAvailable(int length)
    {
        if (length < 0 || _offset > _data.Length - length)
        {
            throw new InvalidDataException("fastfs.dll のイベントが途中で切れています");
        }
    }
}

