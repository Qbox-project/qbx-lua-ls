// Ad-hoc probe: opens a virtual document inside a workspace and prints completions/hovers.
// usage: node scripts/probe.mjs <workspace> <virtual-file-path> <server-binary> < probe.lua
// Lines ending in `--^` request completion at the end of the code, `--?N` requests hover at column N.
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [workspace, virtualFile, binary] = process.argv.slice(2);
const source = readFileSync(0, 'utf8');
const server = spawn(resolve(binary), [], { stdio: ['pipe', 'pipe', 'inherit'] });
let buffer = Buffer.alloc(0), nextId = 0;
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
        if (message.id !== undefined && message.method) send({ jsonrpc: '2.0', id: message.id, result: null });
        else if (message.id !== undefined) { pending.get(message.id)?.(message.result); pending.delete(message.id); }
        else if (message.method === 'textDocument/publishDiagnostics' && message.params.uri === probedUri) diagnostics = message.params.diagnostics;
    }
});
let diagnostics = [], probedUri;
const send = (m) => { const b = JSON.stringify(m); server.stdin.write(`Content-Length: ${Buffer.byteLength(b)}\r\n\r\n${b}`); };
const request = (method, params) => new Promise((done) => { const id = ++nextId; pending.set(id, done); send({ jsonrpc: '2.0', id, method, params }); });

const rootUri = pathToFileURL(resolve(workspace)).href;
await request('initialize', { processId: null, rootUri, capabilities: {}, workspaceFolders: [{ uri: rootUri, name: 'probe' }] });
send({ jsonrpc: '2.0', method: 'initialized', params: {} });
const uri = (probedUri = pathToFileURL(resolve(virtualFile)).href);
const lines = source.split(/\r?\n/);
const clean = lines.map((l) => l.replace(/\s*--(\^|\?\d+)$/, ''));
send({ jsonrpc: '2.0', method: 'textDocument/didOpen', params: { textDocument: { uri, languageId: 'lua', version: 1, text: clean.join('\n') } } });
for (const [line, raw] of lines.entries()) {
    const marker = /--(\^|\?(\d+))$/.exec(raw);
    if (!marker) continue;
    if (marker[1] === '^') {
        const result = await request('textDocument/completion', { textDocument: { uri }, position: { line, character: clean[line].length } });
        const items = result?.items ?? [];
        console.log(`${clean[line].trim()}  =>  ${items.length} items: ${items.slice(0, 14).map((i) => i.label).join(', ')}`);
    } else {
        const result = await request('textDocument/hover', { textDocument: { uri }, position: { line, character: Number(marker[2]) } });
        console.log(`${clean[line].trim()}  @${marker[2]}  =>  ${(result?.contents?.value ?? '<none>').split('\n').filter((l) => l && !l.startsWith('```')).slice(0, 2).join(' | ')}`);
    }
}
console.log('file:', JSON.stringify(await request('qbx/fileInfo', { uri })));
await request('qbx/status', null);
await new Promise((r) => setTimeout(r, 200));
console.log('diagnostics:', diagnostics.map((d) => `${d.range.start.line + 1}:${d.code}${process.env.PROBE_VERBOSE ? ' ' + d.message : ''}`).join(process.env.PROBE_VERBOSE ? '\n' : ', ') || 'none');
await request('shutdown', null);
send({ jsonrpc: '2.0', method: 'exit' });
