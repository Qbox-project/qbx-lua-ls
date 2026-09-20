RegisterNetEvent('shop:buy', function(item, amount)
    if type(amount) ~= 'number' or amount < 1 then return end
    local price = Shop.getPrice(item)
    TriggerClientEvent('shop:bought', source, item, price)
end)

RegisterNetEvent('shop:refund', function(src, amount)
    local player = exports.mylib:GetPlayer(src)
    player.Functions.AddMoney('cash', amount)
    MySQL.query('UPDATE shop SET refunded = ' .. amount)
end)
