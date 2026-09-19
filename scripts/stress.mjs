// Opens every Lua file of a workspace and fires every feature at many positions, looking for
// crashes, internal errors and slow requests.
// usage: node scripts/stress.mjs <workspace-dir> [server-binary] [positions-per-file]
import { spawn } from 'node:child_process';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [workspace, binary = 'target/release/qbx-lua-ls', perFile = '25'] = process.argv.slice(2);
if (!workspace) {
    console.error('usage: node scripts/stress.mjs <workspace-dir> [server-binary] [positions-per-file]');
    process.exit(2);
}

function luaFiles(dir, out = []) {
    for (const entry of readdirSync(dir)) {
        if (entry === 'node_modules' || entry.startsWith('.')) continue;
        const full = join(dir, entry);
        if (statSync(full).isDirectory()) luaFiles(full, out);
        else if (entry.endsWith('.lua')) out.push(full);
    }
    return out;
}

const server = spawn(resolve(binary), [], { stdio: ['pipe', 'pipe', 'inherit'] });
let exited = false;
server.on('exit', (code) => {
    exited = true;
    if (code !== 0) console.error(`SERVER EXITED with code ${code}`);
});
let buffer = Buffer.alloc(0);
let nextId = 0;
const pending = new Map();
let recovered = 0;

server.stdout.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
        const headerEnd = buffer.indexOf('\r\n\r\n');
        if (headerEnd < 0) return;
        const length = Number(/Content-Length: (\d+)/i.exec(buffer.subarray(0, headerEnd).toString())[1]);
        if (buffer.length < headerEnd + 4 + length) return;
        const message = JSON.parse(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString());
        buffer = buffer.subarray(headerEnd + 4 + length);
        if (message.id !== undefined && message.method) send({ jsonrpc: '2.0', id: message.id, result: null });
        else if (message.id !== undefined) {
            pending.get(message.id)?.(message);
            pending.delete(message.id);
        } else if (message.method === 'window/logMessage' && message.params.message.includes('recovered')) recovered++;
    }
});

const send = (message) => {
    const body = JSON.stringify(message);
    server.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
};
const request = (method, params) =>
    new Promise((done, fail) => {
        if (exited) return fail(new Error('server is gone'));
        const id = ++nextId;
        const started = performance.now();
        pending.set(id, (message) => done({ message, ms: performance.now() - started }));
        send({ jsonrpc: '2.0', id, method, params });
    });

const rootUri = pathToFileURL(resolve(workspace)).href;
await request('initialize', { processId: process.pid, rootUri, capabilities: {}, workspaceFolders: [{ uri: rootUri, name: 'stress' }] });
send({ jsonrpc: '2.0', method: 'initialized', params: {} });

const files = luaFiles(resolve(workspace));
const slow = [];
const errors = [];
let requests = 0;
let seed = 12345;
const random = (max) => {
    seed = (seed * 1103515245 + 12345) % 2147483648;
    return seed % max;
};

async function call(method, params, file) {
    const { message, ms } = await request(method, params);
    requests++;
    if (message.error) errors.push(`${method} ${file}: ${message.error.message}`);
    if (ms > 100) slow.push(`${ms.toFixed(0)} ms ${method} ${file}`);
}

for (const file of files) {
    const uri = pathToFileURL(file).href;
    const text = readFileSync(file, 'utf8');
    const lines = text.split('\n');
    send({ jsonrpc: '2.0', method: 'textDocument/didOpen', params: { textDocument: { uri, languageId: 'lua', version: 1, text } } });
    const textDocument = { uri };
    const whole = { start: { line: 0, character: 0 }, end: { line: lines.length, character: 0 } };
    await call('textDocument/documentSymbol', { textDocument }, file);
    await call('textDocument/semanticTokens/full', { textDocument }, file);
    await call('textDocument/foldingRange', { textDocument }, file);
    await call('textDocument/inlayHint', { textDocument, range: whole }, file);
    for (let i = 0; i < Number(perFile); i++) {
        const line = random(lines.length);
        const position = { line, character: random(lines[line].length + 1) };
        const at = { textDocument, position };
        await call('textDocument/hover', at, file);
        await call('textDocument/completion', at, file);
        await call('textDocument/definition', at, file);
        await call('textDocument/signatureHelp', at, file);
        await call('textDocument/documentHighlight', at, file);
    }
    // Truncate the file mid-token to mimic typing, then ask again.
    const cut = random(text.length);
    send({ jsonrpc: '2.0', method: 'textDocument/didChange', params: { textDocument: { uri, version: 2 }, contentChanges: [{ text: text.slice(0, cut) }] } });
    const last = text.slice(0, cut).split('\n');
    const end = { textDocument, position: { line: last.length - 1, character: last[last.length - 1].length } };
    await call('textDocument/completion', end, file);
    await call('textDocument/hover', end, file);
    await call('textDocument/semanticTokens/full', { textDocument }, file);
    send({ jsonrpc: '2.0', method: 'textDocument/didClose', params: { textDocument } });
}

console.log(`${files.length} files, ${requests} requests, ${errors.length} errors, ${recovered} recovered panics, ${slow.length} slow (>100 ms)`);
errors.slice(0, 10).forEach((e) => console.log(`  error: ${e}`));
slow.slice(0, 10).forEach((s) => console.log(`  slow: ${s}`));
await request('shutdown', null);
send({ jsonrpc: '2.0', method: 'exit' });
process.exitCode = errors.length || recovered ? 1 : 0;
