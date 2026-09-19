---@class GaragePoint
---@field coords vector3 where the garage is
---@field label string
---@field slots? integer

---@alias GarageKind 'public'|'job'|'gang'

---Shared helpers for every resource.
MyLib = {}
MyLib.version = '1.2.0'
MyLib.math = {}

---Rounds a number to the given number of decimals.
---@param value number the value to round
---@param decimals? integer
---@return number rounded
function MyLib.round(value, decimals)
    local factor = 10 ^ (decimals or 0)
    return math.floor(value * factor + 0.5) / factor
end

---@param a number
---@param b number
---@return number
function MyLib.math.lerp(a, b)
    return a + (b - a) * 0.5
end

---Creates a garage.
---@param kind GarageKind
---@param point GaragePoint
---@return Garage
function MyLib.createGarage(kind, point)
    return setmetatable({ kind = kind, point = point }, Garage)
end

---@class Garage
---@field kind GarageKind
---@field point GaragePoint
Garage = {}
Garage.__index = Garage

---Returns how many vehicles are parked.
---@return integer count
function Garage:getVehicleCount()
    return 0
end

---@param plate string
---@return boolean success
---@return string? reason
function Garage:store(plate)
    return plate ~= '', nil
end
