local Core = exports['qb-core']:GetCoreObject()
local QB = Core

---@param source integer
---@param cb function
---@param qbItem string
---@param qbAmount integer
---@return boolean
local function buy(source, cb, qbItem, qbAmount)
    cb(qbItem, qbAmount, source)
    return true
end

QB.Functions.CreateCallback('shared:call', buy)
QB.Functions.CreateCallback('qb:only', function(source, cb, qbUnique)
    cb(qbUnique, source)
end)
