// Chromium cannot authenticate to a SOCKS proxy. Keep upstream credentials in this
// process and expose a temporary HTTP proxy on loopback only, without changing routes.
import net from 'node:net';
import tls from 'node:tls';

const failure = code => Object.assign(new Error(code), { code });
const track = (socket, sockets) => {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
    // Each handshake and tunnel handles failures; never emit an uncaught socket error.
    socket.on('error', () => {});
    return socket;
};
function read(socket, take) {
    return new Promise((resolve, reject) => {
        const cleanup = () => {
            socket.removeListener('readable', ready);
            socket.removeListener('error', rejected);
            socket.removeListener('end', ended);
            socket.removeListener('close', ended);
        };
        const rejected = () => { cleanup(); reject(failure('PROXY_CONNECTION_FAILED')); };
        const ended = () => { if (!ready()) rejected(); };
        function ready() {
            try {
                const value = take();
                if (value !== null) { cleanup(); resolve(value); return true; }
                if (socket.destroyed) { rejected(); return true; }
            } catch (error) { cleanup(); reject(error); return true; }
            return false;
        }
        socket.on('readable', ready).once('error', rejected).once('end', ended).once('close', ended);
        ready();
    });
}
const exactly = (socket, count) => read(socket, () => socket.read(count));
function headers(socket) {
    let buffer = Buffer.alloc(0);
    return read(socket, () => {
        let chunk;
        while ((chunk = socket.read()) !== null) {
            buffer = Buffer.concat([buffer, chunk]);
            const end = buffer.indexOf('\r\n\r\n');
            if (end >= 0) {
                if (end > 65536) throw failure('PROXY_CONNECTION_FAILED');
                if (buffer.length > end + 4) socket.unshift(buffer.subarray(end + 4));
                return buffer.subarray(0, end + 4).toString('latin1');
            }
            if (buffer.length > 65536) throw failure('PROXY_CONNECTION_FAILED');
        }
        return null;
    });
}
function open(proxy, sockets) {
    return new Promise((resolve, reject) => {
        const host = proxy.hostname.replace(/^\[|\]$/g, '');
        const port = Number(proxy.port || (proxy.protocol === 'https:' ? 443 : proxy.protocol === 'http:' ? 80 : 1080));
        const secure = proxy.protocol === 'https:';
        const socket = track(secure ? tls.connect({host, port, servername:net.isIP(host) ? undefined : host}) : net.connect({host, port}), sockets);
        socket.setTimeout(10000, () => socket.destroy(failure('PROXY_CONNECTION_FAILED')));
        const rejected = () => reject(failure('PROXY_CONNECTION_FAILED'));
        socket.once('error', rejected);
        socket.once(secure ? 'secureConnect' : 'connect', () => {
            socket.removeListener('error', rejected); resolve(socket);
        });
    });
}
const authority = (host, port) => `${host.includes(':') ? `[${host}]` : host}:${port}`;
async function tunnel(proxy, host, port, sockets) {
    const socket = await open(proxy, sockets);
    try {
        const username = decodeURIComponent(proxy.username);
        const password = decodeURIComponent(proxy.password);
        const authenticated = Boolean(username || password);
        if (proxy.protocol.startsWith('socks5')) {
            // When credentials select a Mihomo route, never negotiate anonymous access.
            socket.write(Buffer.from([5, 1, authenticated ? 2 : 0]));
            const selected = await exactly(socket, 2);
            if (selected[0] !== 5 || selected[1] !== (authenticated ? 2 : 0)) throw failure('PROXY_AUTH_FAILED');
            if (authenticated) {
                const user = Buffer.from(username); const pass = Buffer.from(password);
                if (!user.length || user.length > 255 || pass.length > 255) throw failure('UNSUPPORTED_PROXY');
                socket.write(Buffer.concat([Buffer.from([1, user.length]), user, Buffer.from([pass.length]), pass]));
                const result = await exactly(socket, 2);
                if (result[0] !== 1 || result[1] !== 0) throw failure('PROXY_AUTH_FAILED');
            }
            // Send the hostname to the upstream proxy, preserving socks5h remote DNS.
            const name = Buffer.from(host);
            if (!name.length || name.length > 255) throw failure('PROXY_CONNECTION_FAILED');
            const destination = Buffer.alloc(2); destination.writeUInt16BE(port);
            socket.write(Buffer.concat([Buffer.from([5, 1, 0, 3, name.length]), name, destination]));
            const result = await exactly(socket, 4);
            if (result[0] !== 5 || result[1] !== 0 || result[2] !== 0) throw failure('PROXY_CONNECTION_FAILED');
            const size = result[3] === 1 ? 4 : result[3] === 4 ? 16 : result[3] === 3 ? (await exactly(socket, 1))[0] : 0;
            if (!size) throw failure('PROXY_CONNECTION_FAILED');
            await exactly(socket, size + 2);
        } else {
            const target = authority(host, port);
            const auth = authenticated ? `Proxy-Authorization: Basic ${Buffer.from(`${username}:${password}`).toString('base64')}\r\n` : '';
            socket.write(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n${auth}\r\n`);
            const response = await headers(socket);
            const status = /^HTTP\/1\.[01] (\d{3})\b/.exec(response)?.[1];
            if (status === '407') throw failure('PROXY_AUTH_FAILED');
            if (!status || !status.startsWith('2')) throw failure('PROXY_CONNECTION_FAILED');
        }
        socket.setTimeout(0);
        return socket;
    } catch (error) { socket.destroy(); throw error; }
}

export async function browserProxy(raw) {
    if (!raw) return {address:null, failure:() => null, close:async () => {}};
    let proxy;
    try {
        proxy = new URL(raw);
        if (!['http:', 'https:', 'socks5:', 'socks5h:'].includes(proxy.protocol) || !proxy.hostname ||
            (proxy.pathname && proxy.pathname !== '/') || proxy.search || proxy.hash) throw new Error();
        decodeURIComponent(proxy.username); decodeURIComponent(proxy.password);
    } catch { throw failure('UNSUPPORTED_PROXY'); }
    // Chromium already supports these routes directly; normalize its SOCKS scheme.
    if (!proxy.username && !proxy.password) {
        if (proxy.protocol === 'socks5h:') proxy.protocol = 'socks5:';
        return {address:proxy.href, failure:() => null, close:async () => {}};
    }
    const sockets = new Set(); const failures = new Map();
    let closing = false;
    const server = net.createServer(client => {
        track(client, sockets);
        client.setTimeout(10000, () => client.destroy());
        let upstream, host;
        const serve = async () => {
            try {
                const request = await headers(client);
                const lines = request.slice(0, -4).split('\r\n');
                const [method, target, version] = lines.shift().split(' ');
                const connect = method === 'CONNECT';
                const url = new URL(connect ? `https://${target}` : target);
                if ((!connect && url.protocol !== 'http:') || url.username || url.password || !/^HTTP\/1\.[01]$/.test(version)) throw failure('PROXY_CONNECTION_FAILED');
                host = url.hostname.replace(/^\[|\]$/g, '');
                const port = Number(url.port || (connect ? 443 : 80));
                if (!host || !port || closing) throw failure('PROXY_CONNECTION_FAILED');
                upstream = await tunnel(proxy, host, port, sockets);
                if (closing || client.destroyed) { upstream.destroy(); return; }
                client.setTimeout(0);
                client.once('close', () => upstream.destroy());
                upstream.once('close', () => client.end());
                upstream.once('error', () => client.destroy());
                if (connect) client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
                else {
                    // One HTTP request per connection; no host changes inside a tunnel.
                    const kept = lines.filter(line => !/^(proxy-authorization|proxy-connection|connection):/i.test(line));
                    upstream.write(`${method} ${url.pathname}${url.search} ${version}\r\n${kept.join('\r\n')}\r\nConnection: close\r\n\r\n`);
                }
                client.pipe(upstream); upstream.pipe(client);
            } catch (error) {
                upstream?.destroy();
                if (host) failures.set(host, error.code === 'PROXY_AUTH_FAILED' ? 'PROXY_AUTH_FAILED' : 'PROXY_CONNECTION_FAILED');
                if (!client.destroyed) client.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
            }
        };
        void serve();
    });
    await new Promise((resolve, reject) => {
        server.once('error', () => reject(failure('PROXY_CONNECTION_FAILED')));
        server.listen(0, '127.0.0.1', resolve);
    });
    return {
        address:`http://127.0.0.1:${server.address().port}`,
        failure:host => failures.get(host),
        close:async () => {
            closing = true;
            for (const socket of sockets) socket.destroy();
            await new Promise(resolve => server.close(resolve));
        },
    };
}
