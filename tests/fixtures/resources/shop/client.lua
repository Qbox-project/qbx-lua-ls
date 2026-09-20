local debug = GetConvar('shop_debug', 'false')
LocalPlayer.state.isShopping = true

RegisterNetEvent('shop:bought', function(item)
    print(locale('buy.success', item), locale('buy.missing'), debug, Shop.getPrice(item))
end)

TriggerServerEvent('shop:buy', 'water', 1, 'extra')
TriggerServerEvent('shop:bought', 'water')
print(exports.mylib:Ping('hello', 'world'))
