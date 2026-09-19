Config = {
    Debug = true,
    SpawnDistance = 25.0,
    Garages = {
        legion = { label = 'Legion Square', slots = 10 },
    },
}

function Config.isDebug()
    return Config.Debug
end
