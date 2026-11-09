import net from 'node:net';
import assert from 'node:assert/strict';

// A SOCKS5 server with the same password-authenticated protocol used by Mihomo.
export async function socksFixture({username='route-user', password='route-password', route=(host, port) => ({host:'127.0.0.1', port})} = {}) {
    const sockets = new Set(); const destinations = []; const methods = [];
    const track = socket => { sockets.add(socket); socket.on('error', () => {}); socket.once('close', () => sockets.delete(socket)); return socket; };
    const server = net.createServer(client => {
        track(client);
        let buffer = Buffer.alloc(0); let phase = 'hello';
        const data = chunk => {
            buffer = Buffer.concat([buffer, chunk]);
            while (true) {
                if (phase === 'hello') {
                    if (buffer.length < 2 || buffer.length < 2 + buffer[1]) return;
                    assert.equal(buffer[0], 5);
                    methods.push([...buffer.subarray(2, 2 + buffer[1])]);
                    assert.deepEqual(methods.at(-1), [2], 'route identity must not negotiate anonymous access');
                    buffer = buffer.subarray(2 + buffer[1]);
                    client.write(Buffer.from([5, 2])); phase = 'auth';
                } else if (phase === 'auth') {
                    if (buffer.length < 2 || buffer.length < 3 + buffer[1]) return;
                    const ulen = buffer[1]; const plen = buffer[2 + ulen];
                    if (buffer.length < 3 + ulen + plen) return;
                    const user = buffer.subarray(2, 2 + ulen).toString();
                    const pass = buffer.subarray(3 + ulen, 3 + ulen + plen).toString();
                    buffer = buffer.subarray(3 + ulen + plen);
                    if (user !== username || pass !== password) { phase = 'closed'; client.end(Buffer.from([1, 1])); return; }
                    client.write(Buffer.from([1, 0])); phase = 'connect';
                } else if (phase === 'connect') {
                    if (buffer.length < 5 || buffer.length < 7 + buffer[4]) return;
                    assert.deepEqual([...buffer.subarray(0, 4)], [5, 1, 0, 3]);
                    const size = buffer[4]; const host = buffer.subarray(5, 5 + size).toString();
                    const port = buffer.readUInt16BE(5 + size);
                    destinations.push({host, port}); buffer = buffer.subarray(7 + size);
                    phase = 'tunnel'; client.removeListener('data', data); client.pause();
                    const upstream = track(net.connect(route(host, port)));
                    upstream.once('connect', () => {
                        client.write(Buffer.from([5, 0, 0, 1, 127, 0, 0, 1, 0, 0]));
                        if (buffer.length) upstream.write(buffer);
                        client.pipe(upstream); upstream.pipe(client);
                    });
                    upstream.once('close', () => client.destroy()); client.once('close', () => upstream.destroy());
                    return;
                } else return;
            }
        };
        client.on('data', data);
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    return {url:`socks5h://${encodeURIComponent(username)}:${encodeURIComponent(password)}@127.0.0.1:${server.address().port}`, destinations, methods,
        close:async () => { for (const socket of sockets) socket.destroy(); await new Promise(resolve => server.close(resolve)); }};
}
