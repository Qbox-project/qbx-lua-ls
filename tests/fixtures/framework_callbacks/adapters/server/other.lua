lib.callback.register('shared:call', function(source, oxPayload)
    return oxPayload, source
end)
lib.callback.register('ox:only', function(source, oxUnique)
    return oxUnique, source
end)
RegisterNetEvent('shared:call', function(nativePayload)
    print(nativePayload)
end)
RegisterNetEvent('native:only', function(nativeUnique)
    print(nativeUnique)
end)
