local Core = exports['qb-core']:GetCoreObject()
local Framework = exports.es_extended:getSharedObject()
local QB = Core
local ESX = Framework

QB.Functions.TriggerCallback('shared:call', function(result) print(result) end, 'water', 2)
ESX.TriggerServerCallback('shared:call', function(result) print(result) end, 42, 'legion')
lib.callback.await('shared:call', false, 'ox')
TriggerServerEvent('shared:call', true)
