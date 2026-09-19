local settings = require '@mylib.modules.settings'
local garage = MyLib.createGarage('public', { coords = vector3(0, 0, 0), label = 'Test' })
local count = garage:getVehicleCount()
local ok, reason = garage:store('ABC123')
local rounded = MyLib.round(1.2345, 2)
local coords = GetEntityCoords(PlayerPedId())
local distance = #(coords - garage.point.coords)

RegisterNetEvent('myresource:client:notify', function(message, kind)
    print(message, kind, count, ok, reason, rounded, distance, settings.maxGarages)
end)

CreateThread(function()
    while true do
        Wait(Config.SpawnDistance > 10 and 0 or 500)
        TriggerServerEvent('myresource:server:ping', Config.Garages.legion.label)
    end
end)
