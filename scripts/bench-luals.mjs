// Measures lua-language-server on the same workspace for comparison with scripts/bench.mjs.
// usage: node scripts/bench-luals.mjs <workspace-dir> <lua-language-server-binary> [library-dir ...]
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [workspace, binary, ...libraries] = process.argv.slice(2);
if (!workspace || !binary) {
    console.error('usage: node scripts/bench-luals.mjs <workspace-dir> <lua-language-server-binary> [library-dir ...]');
    process.exit(2);
}

const scratch = mkdtempSync(join(tmpdir(), 'luals-bench-'));
const server = spawn(resolve(binary), [`--logpath=${join(scratch, 'log')}`, `--metapath=${join(scratch, 'meta')}`], {
    stdio: ['pipe', 'pipe', 'inherit'],
});
let buffer = Buffer.alloc(0);
let nextId = 0;
const pending = new Map();
let activeProgress = 0;
let lastProgressEnd = 0;
let sawProgress = false;

const settings = {
    Lua: {
        runtime: { version: 'Lua 5.4', nonstandardSymbol: ['/**/', '`', '+=', '-=', '*=', '/=', '<<=', '>>=', '&=', '|=', '^='] },
        workspace: { library: libraries.map((l) => resolve(l)), checkThirdParty: false, maxPreload: 100000, preloadFileSize: 10000 },
        telemetry: { enable: false },
    },
};

function lookup(section) {
    return section ? section.split('.').reduce((value, key) => value?.[key], settings) ?? null : settings;
}

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
            const result = message.method === 'workspace/configuration' ? message.params.items.map((item) => lookup(item.section)) : null;
            send({ jsonrpc: '2.0', id: message.id, result });
        } else if (message.id !== undefined) {
            pending.get(message.id)?.(message.result);
            pending.delete(message.id);
        } else if (message.method === '$/progress') {
            sawProgress = true;
            if (message.params.value.kind === 'begin') activeProgress++;
            if (message.params.value.kind === 'end') {
                activeProgress--;
                lastProgressEnd = performance.now();
            }
        }
    }
});

function send(message) {
    const body = JSON.stringify(message);
    server.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
}

function request(method, params) {
    const id = ++nextId;
    return new Promise((done) => {
        pending.set(id, done);
        send({ jsonrpc: '2.0', id, method, params });
    });
}

function memoryMb() {
    if (process.platform === 'win32') {
        const out = execFileSync('powershell', ['-NoProfile', '-Command', `(Get-Process -Id ${server.pid}).WorkingSet64`]);
        return Number(out.toString().trim()) / 1024 / 1024;
    }
    return Number(execFileSync('ps', ['-o', 'rss=', '-p', String(server.pid)]).toString().trim()) / 1024;
}

const started = performance.now();
const rootUri = pathToFileURL(resolve(workspace)).href;
await request('initialize', {
    processId: process.pid,
    rootUri,
    capabilities: { window: { workDoneProgress: true }, workspace: { configuration: true } },
    workspaceFolders: [{ uri: rootUri, name: 'bench' }],
});
send({ jsonrpc: '2.0', method: 'initialized', params: {} });

let peak = 0;
for (;;) {
    await new Promise((r) => setTimeout(r, 1000));
    peak = Math.max(peak, memoryMb());
    const idleFor = performance.now() - lastProgressEnd;
    const elapsed = performance.now() - started;
    if ((sawProgress && activeProgress === 0 && idleFor > 5000) || elapsed > 180000) {
        const loaded = sawProgress ? (lastProgressEnd - started) / 1000 : NaN;
        console.log(`workspace loaded after ~${loaded.toFixed(1)} s`);
        console.log(`memory once idle: ${memoryMb().toFixed(1)} MB (peak ${peak.toFixed(1)} MB)`);
        break;
    }
}

await request('shutdown', null);
send({ jsonrpc: '2.0', method: 'exit' });
setTimeout(() => process.exit(0), 1000);
