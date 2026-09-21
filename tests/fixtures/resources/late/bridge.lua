local selected = GetResourceKvpString('late:inventory')

if selected == 'ox' then
    exports.ox_inventory:AddItem(1, 'water', 1)
elseif selected == 'kartik' then
    exports['kartik-evidence']:DropEvidence('fingerprint')
else
    exports.some_fallback:Notify('none')
end

exports.not_installed_anywhere:DoThing()
