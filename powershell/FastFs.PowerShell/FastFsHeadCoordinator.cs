namespace FastFs.PowerShell;

internal sealed class FastFsProducerState
{
    internal bool LimitReached { get; set; }
    internal FastFsProducerState? Previous { get; set; }
}

internal static class FastFsHeadCoordinator
{
    [ThreadStatic]
    private static FastFsProducerState? _currentProducer;

    internal static FastFsProducerState? EnterNativeProducer(FastFsProducerState producer)
    {
        var previous = _currentProducer;
        producer.Previous = previous;
        _currentProducer = producer;
        return previous;
    }

    internal static void ExitNativeProducer(FastFsProducerState? previous)
    {
        if (_currentProducer is not null)
        {
            _currentProducer.Previous = null;
        }
        _currentProducer = previous;
    }

    internal static void SignalCurrentProducer()
    {
        for (var producer = _currentProducer; producer is not null; producer = producer.Previous)
        {
            producer.LimitReached = true;
        }
    }
}
