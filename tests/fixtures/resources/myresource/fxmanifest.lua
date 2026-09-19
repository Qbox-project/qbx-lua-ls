fx_version 'cerulean'
game 'gta5'
lua54 'yes'

shared_scripts {
    '@mylib/init.lua',
    'shared/*.lua',
}
client_script 'client/main.lua'
server_script 'server/main.lua'
files { 'modules/*.lua' }
dependency 'mylib'
