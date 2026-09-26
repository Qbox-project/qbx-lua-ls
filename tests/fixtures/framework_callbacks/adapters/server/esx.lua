local Core = exports.es_extended:getSharedObject()
local ESX = Core

---@param source integer
---@param cb function
---@param esxVehicle number
---@param esxDepot string
---@return boolean
local function garage(source, cb, esxVehicle, esxDepot)
    cb(esxVehicle, esxDepot, source)
    return true
end

ESX.RegisterServerCallback('shared:call', garage)
ESX.RegisterServerCallback('esx:only', function(source, cb, esxUnique)
    cb(esxUnique, source)
end)
