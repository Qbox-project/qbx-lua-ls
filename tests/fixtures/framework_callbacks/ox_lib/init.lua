-- Minimal fixture signature; callback payloads still come from resource registrations.
lib = {}
lib.callback = {}

---@param name string
---@param delay number|boolean
---@param ... any
function lib.callback.await(name, delay, ...) end
