local QB = exports['qb-core']:GetCoreObject()
if IsDuplicityVersion() then
    QB.Functions.CreateCallback('qb:guarded', function(source, cb, guardedPayload)
        cb(guardedPayload, source)
    end)
else
    QB.Functions.TriggerCallback('qb:guarded', function(result) print(result) end, 'guarded')
end
