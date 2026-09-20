function locale(key, ...)
    return key:format(...)
end

Shop = {}
Shop.items = {}

function Shop.getPrice(item)
    return Shop.items[item]
end

Config = {
    ShopName = 'General Store',
    TaxRate = 0.21,
    OpenAtNight = false,
}
