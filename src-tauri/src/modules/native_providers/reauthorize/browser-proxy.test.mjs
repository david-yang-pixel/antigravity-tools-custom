import { test } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import http from 'node:http';
import { browserProxy } from './browser-proxy.mjs';
import { socksFixture } from './browser-proxy.fixture.mjs';

const listen = server => new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const close = server => new Promise(resolve => server.close(resolve));
function connect(address, target, payload='') {
    return new Promise((resolve, reject) => {
        const url = new URL(address); const socket = net.connect(Number(url.port), url.hostname);
        let response = '';
        const timer = setTimeout(() => {socket.destroy(); reject(new Error('proxy test timed out'));}, 4000);
        socket.on('error', reject);
        socket.on('data', chunk => {
            response += chunk.toString();
            if (response.includes('502 Bad Gateway') || (response.includes('\r\n\r\n') && response.endsWith(payload))) {
                clearTimeout(timer); socket.destroy(); resolve(response);
            }
        });
        socket.once('connect', () => socket.write(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n\r\n${payload}`));
    });
}

test('password-authenticated SOCKS5h keeps route identity, remote DNS and tunnel bytes', async () => {
    const sockets = new Set();
    const echo = net.createServer(socket => {sockets.add(socket); socket.on('close', () => sockets.delete(socket)); socket.pipe(socket);});
    await listen(echo);
    const socks = await socksFixture({username:'chatgpt-login',password:'route@password:encoded',route:() => ({host:'127.0.0.1',port:echo.address().port})});
    const proxy = await browserProxy(socks.url);
    try {
        const address = new URL(proxy.address);
        assert.equal(address.hostname, '127.0.0.1'); assert.equal(address.username, ''); assert.equal(address.password, '');
        assert(!proxy.address.includes('route@password'));
        const payload = 'first-client-bytes'.repeat(32768);
        const response = await connect(proxy.address, 'only-upstream-can-resolve.invalid:443', payload);
        assert.equal(response, `HTTP/1.1 200 Connection Established\r\n\r\n${payload}`);
        assert.deepEqual(socks.destinations, [{host:'only-upstream-can-resolve.invalid',port:443}]);
        assert.deepEqual(socks.methods, [[2]]);
    } finally {await proxy.close(); await socks.close(); for (const socket of sockets) socket.destroy(); await close(echo);}
});

test('plain HTTP passes through the selected SOCKS route without leaking proxy credentials', async () => {
    let seen;
    const origin = http.createServer((req, res) => {seen={url:req.url,headers:req.headers}; res.end('through-selected-node');});
    await listen(origin);
    const socks = await socksFixture(); const proxy = await browserProxy(socks.url);
    try {
        const url = new URL(proxy.address);
        const body = await new Promise((resolve, reject) => {
            http.get({hostname:url.hostname,port:url.port,path:`http://fixture.invalid:${origin.address().port}/login?test=1`,headers:{Host:'fixture.invalid','Proxy-Authorization':'should-be-stripped'}}, res => {
                let body=''; res.on('data', chunk => body+=chunk); res.on('end', () => resolve(body));
            }).on('error', reject);
        });
        assert.equal(body,'through-selected-node'); assert.equal(seen.url,'/login?test=1');
        assert.equal(seen.headers['proxy-authorization'],undefined); assert.equal(seen.headers.connection,'close');
        assert.equal(socks.destinations[0].host,'fixture.invalid');
    } finally {await proxy.close(); await socks.close(); await close(origin);}
});

test('SOCKS authentication failure stops before destination connection and reports a proxy error', async () => {
    const socks = await socksFixture(); const proxy = await browserProxy(socks.url.replace('route-password','wrong-password'));
    try {
        const response = await connect(proxy.address,'auth.openai.com:443');
        assert(response.startsWith('HTTP/1.1 502')); assert.equal(proxy.failure('auth.openai.com'),'PROXY_AUTH_FAILED');
        assert.equal(socks.destinations.length,0); assert(!response.includes('wrong-password'));
    } finally {await proxy.close(); await socks.close();}
});

test('authenticated HTTP upstream receives credentials only on its CONNECT request', async () => {
    const sockets = new Set(); let auth;
    const upstream = http.createServer();
    upstream.on('connect',(req,socket,head) => {
        auth=req.headers['proxy-authorization']; sockets.add(socket); socket.on('close',()=>sockets.delete(socket));
        socket.write('HTTP/1.1 200 Connection Established\r\n\r\n'); if (head.length) socket.write(head); socket.pipe(socket);
    });
    await listen(upstream);
    const proxy = await browserProxy(`http://user:p%40ss%3Aword@127.0.0.1:${upstream.address().port}`);
    try {
        const response = await connect(proxy.address,'auth.openai.com:443','tunnel-data');
        assert(response.endsWith('tunnel-data')); assert.equal(auth,`Basic ${Buffer.from('user:p@ss:word').toString('base64')}`);
        assert(!response.includes('Proxy-Authorization'));
    } finally {await proxy.close(); for (const socket of sockets) socket.destroy(); await close(upstream);}
});

test('direct SOCKS5h normalizes for Chromium and unsupported proxy errors contain no secrets', async () => {
    const proxy=await browserProxy('socks5h://127.0.0.1:1080');
    assert.equal(proxy.address,'socks5://127.0.0.1:1080'); await proxy.close();
    await assert.rejects(browserProxy('ftp://private-user:private-pass@example.com'), {code:'UNSUPPORTED_PROXY', message:'UNSUPPORTED_PROXY'});
});
