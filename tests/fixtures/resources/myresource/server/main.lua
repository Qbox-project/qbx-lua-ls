RegisterNetEvent('myresource:server:ping', function(label)
    local src = source
    local player = exports.mylib:GetPlayer(src)
    if player then
        TriggerClientEvent('myresource:client:notify', src, player.name .. label, 'info')
    end
end)

lib.callback.register('myresource:getGarages', function(source)
    return Config.Garages
end)
