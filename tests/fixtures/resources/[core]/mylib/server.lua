---Looks a player up by server id.
---@param source integer
---@return { name: string, money: number }? player
local function getPlayer(source)
    return { name = GetPlayerName(source), money = 0 }
end

exports('GetPlayer', getPlayer)

exports('Ping', function(message)
    return 'pong: ' .. message
end)

lib = lib or {}
