// Starts the server over stdio against a folder, opens one file and reports timings and memory.
// usage: node scripts/bench.mjs <workspace-dir> [file-to-open] [server-binary]
import { spawn, execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [workspace, fileToOpen, binary = 'target/release/qbx-lua-ls'] = process.argv.slice(2);
if (!workspace) {
    console.error('usage: node scripts/bench.mjs <workspace-dir> [file-to-open] [server-binary]');
    process.exit(2);
}

const server = spawn(resolve(binary), [], { stdio: ['pipe', 'pipe', 'inherit'] });
let buffer = Buffer.alloc(0);
let nextId = 0;
const pending = new Map();

server.stdout.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
        const headerEnd = buffer.indexOf('\r\n\r\n');
        if (headerEnd < 0) return;
        const length = Number(/Content-Length: (\d+)/i.exec(buffer.subarray(0, headerEnd).toString())[1]);
        if (buffer.length < headerEnd + 4 + length) return;
        const message = JSON.parse(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString());
        buffer = buffer.subarray(headerEnd + 4 + length);
        if (message.id !== undefined && message.method) {
            send({ jsonrpc: '2.0', id: message.id, result: null });
        } else if (message.id !== undefined) {
            pending.get(message.id)?.(message.result);
            pending.delete(message.id);
        } else if (message.method === 'window/logMessage') {
            console.log(`server: ${message.params.message}`);
        }
    }
});

function send(message) {
    const body = JSON.stringify(message);
    server.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
}

function request(method, params) {
    const id = ++nextId;
    const started = performance.now();
    return new Promise((done) => {
        pending.set(id, (result) => done({ result, ms: performance.now() - started }));
        send({ jsonrpc: '2.0', id, method, params });
    });
}

function memoryMb() {
    if (process.platform === 'win32') {
        const out = execFileSync('powershell', ['-NoProfile', '-Command', `(Get-Process -Id ${server.pid}).WorkingSet64`]);
        return Number(out.toString().trim()) / 1024 / 1024;
    }
    const out = execFileSync('ps', ['-o', 'rss=', '-p', String(server.pid)]);
    return Number(out.toString().trim()) / 1024;
}

const rootUri = pathToFileURL(resolve(workspace)).href;
const init = await request('initialize', {
    processId: process.pid,
    rootUri,
    capabilities: {},
    workspaceFolders: [{ uri: rootUri, name: 'bench' }],
});
send({ jsonrpc: '2.0', method: 'initialized', params: {} });
const status = await request('qbx/status', null);
console.log(`initialize: ${init.ms.toFixed(0)} ms, index ready after: ${status.ms.toFixed(0)} ms`, status.result);
console.log(`memory after indexing: ${memoryMb().toFixed(1)} MB`);

if (fileToOpen) {
    const uri = pathToFileURL(resolve(fileToOpen)).href;
    const text = readFileSync(resolve(fileToOpen), 'utf8');
    send({ jsonrpc: '2.0', method: 'textDocument/didOpen', params: { textDocument: { uri, languageId: 'lua', version: 1, text } } });
    const lines = text.split('\n');
    const line = Math.floor(lines.length / 2);
    const position = { line, character: Math.min(4, lines[line].length) };
    for (const method of ['textDocument/documentSymbol', 'textDocument/semanticTokens/full', 'textDocument/foldingRange']) {
        const { ms } = await request(method, { textDocument: { uri } });
        console.log(`${method}: ${ms.toFixed(1)} ms`);
    }
    for (const method of ['textDocument/hover', 'textDocument/completion', 'textDocument/definition']) {
        const { ms } = await request(method, { textDocument: { uri }, position });
        console.log(`${method}: ${ms.toFixed(1)} ms`);
    }
    console.log(`memory with one document open: ${memoryMb().toFixed(1)} MB`);
}

await request('shutdown', null);
send({ jsonrpc: '2.0', method: 'exit' });
