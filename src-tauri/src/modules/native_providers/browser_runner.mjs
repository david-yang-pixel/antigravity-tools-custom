// Private, short-lived browser driver. Only its owning Rust process uses stdio.
// No account credentials, screenshots, input or grants are written to a log/file.
import http from 'node:http';
import readline from 'node:readline';
import { randomUUID } from 'node:crypto';
import { pathToFileURL } from 'node:url';

const WIDTH = 1000, HEIGHT = 720, LIMIT = 64 * 1024;
const providerDomains = ['trae.cn', 'trae.com.cn', 'trae.ai'];
const matchesDomain = (host, domains) => domains.some(d => host === d || host.endsWith(`.${d}`));

export function officialOrigin(value) {
    try {
        const url = new URL(value);
        return url.protocol === 'https:' && !url.username && !url.password && !url.port
            && url.origin === value && matchesDomain(url.hostname, providerDomains);
    } catch { return false; }
}

export function validateStart(config) {
    const auth = new URL(config.auth_url), callback = new URL(config.callback_url);
    const now = Math.floor(Date.now() / 1000);
    if (!officialOrigin(auth.origin) || auth.pathname !== '/authorization'
        || auth.username || auth.password || auth.hash
        || callback.protocol !== 'http:' || callback.hostname !== '127.0.0.1'
        || !callback.port || callback.pathname !== '/authorize' || callback.search || callback.hash
        || callback.username || callback.password
        || auth.searchParams.get('auth_callback_url') !== callback.href
        || !config.trace || auth.searchParams.get('login_trace_id') !== config.trace
        || !Number.isInteger(config.debug_port) || config.debug_port < 1 || config.debug_port > 65535
        || !Number.isInteger(config.expires_at) || config.expires_at <= now || config.expires_at > now + 610) {
        throw new Error('TRAE_BROWSER_START_FAILED');
    }
    return { auth, callback };
}

export function matchingTrace(query, trace) {
    if (Buffer.byteLength(query) > LIMIT) return false;
    const params = new URLSearchParams(query);
    const traces = [...params.getAll('loginTraceID'), ...params.getAll('login_trace_id')];
    return traces.length > 0 && traces.every(value => value === trace);
}

export function inputEvents(action) {
    if (action.type === 'click' && [action.x, action.y].every(Number.isFinite)
        && action.x >= 0 && action.x <= WIDTH && action.y >= 0 && action.y <= HEIGHT) {
        return [
            ['Input.dispatchMouseEvent', { type: 'mousePressed', x: action.x, y: action.y, button: 'left', clickCount: 1 }],
            ['Input.dispatchMouseEvent', { type: 'mouseReleased', x: action.x, y: action.y, button: 'left', clickCount: 1 }],
        ];
    }
    if (action.type === 'text' && typeof action.text === 'string' && action.text.length && Buffer.byteLength(action.text) <= 2048) {
        return [['Input.insertText', { text: action.text }]];
    }
    if (action.type === 'scroll' && Number.isFinite(action.delta_y) && Math.abs(action.delta_y) <= 1200) {
        return [['Input.dispatchMouseEvent', { type: 'mouseWheel', x: 500, y: 360, deltaX: 0, deltaY: action.delta_y }]];
    }
    if (action.type === 'reload') return [['Page.reload', { ignoreCache: true }]];
    const keys = { Enter: 13, Tab: 9, ShiftTab: 9, Backspace: 8, Delete: 46, Escape: 27,
        ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Home: 36, End: 35, SelectAll: 65 };
    if (action.type === 'key' && Object.hasOwn(keys, action.key)) {
        const key = action.key === 'SelectAll' ? 'a' : action.key === 'ShiftTab' ? 'Tab' : action.key;
        const modifiers = action.key === 'SelectAll' ? 2 : action.key === 'ShiftTab' ? 8 : 0;
        const params = { key, code: key.length === 1 ? 'KeyA' : key, windowsVirtualKeyCode: keys[action.key], modifiers };
        return [['Input.dispatchKeyEvent', { ...params, type: 'keyDown', ...(key === 'Enter' ? { text: '\r' } : {}) }],
            ['Input.dispatchKeyEvent', { ...params, type: 'keyUp' }]];
    }
    throw new Error('TRAE_BROWSER_COMMAND_FAILED');
}

async function connect(endpoint) {
    const socket = new WebSocket(endpoint);
    await new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('TRAE_BROWSER_START_FAILED')), 8000);
        socket.onopen = () => { clearTimeout(timeout); resolve(); };
        socket.onerror = () => { clearTimeout(timeout); reject(new Error('TRAE_BROWSER_START_FAILED')); };
    });
    let serial = 0;
    const pending = new Map();
    socket.onmessage = event => {
        let value; try { value = JSON.parse(event.data); } catch { return; }
        const waiter = pending.get(value.id);
        if (waiter) { clearTimeout(waiter.timer); pending.delete(value.id); value.error ? waiter.reject(new Error('TRAE_BROWSER_COMMAND_FAILED')) : waiter.resolve(value.result); }
    };
    socket.onclose = () => {
        for (const p of pending.values()) { clearTimeout(p.timer); p.reject(new Error('TRAE_BROWSER_STOPPED')); }
        pending.clear();
    };
    return {
        call(method, params = {}) {
            return new Promise((resolve, reject) => {
                const id = ++serial;
                const timer = setTimeout(() => { pending.delete(id); reject(new Error('TRAE_BROWSER_COMMAND_FAILED')); }, 8000);
                pending.set(id, { resolve, reject, timer });
                try { socket.send(JSON.stringify({ id, method, params })); }
                catch { clearTimeout(timer); pending.delete(id); reject(new Error('TRAE_BROWSER_STOPPED')); }
            });
        },
        close: () => socket.close(),
    };
}

export class LoginBrowser {
    constructor(emit) { this.emit = emit; this.grants = new Map(); this.pages = new Map(); this.completed = false; }
    async start(config) {
        const { auth, callback } = validateStart(config);
        this.config = config;
        this.callback = callback;
        this.server = http.createServer({ maxHeaderSize: LIMIT + 8192 }, (req, res) => void this.receive(req, res));
        this.server.requestTimeout = 10000;
        this.server.headersTimeout = 10000;
        this.server.maxConnections = 16;
        await new Promise((resolve, reject) => {
            this.server.once('error', () => reject(new Error('TRAE_BROWSER_START_FAILED')));
            this.server.listen(Number(callback.port), '127.0.0.1', resolve);
        });
        this.timer = setTimeout(() => this.stop(), Math.max(1, config.expires_at * 1000 - Date.now()));
        this.endpoint = `http://127.0.0.1:${config.debug_port}`;
        let version;
        for (let attempt = 0; attempt < 60; attempt++) {
            try { version = await fetch(`${this.endpoint}/json/version`, { signal: AbortSignal.timeout(800) }).then(r => r.json()); break; }
            catch { await new Promise(resolve => setTimeout(resolve, 100)); }
        }
        if (!version?.webSocketDebuggerUrl) throw new Error('TRAE_BROWSER_START_FAILED');
        this.browser = await connect(version.webSocketDebuggerUrl);
        await this.browser.call('Browser.setDownloadBehavior', { behavior: 'deny' });
        // Chromium 154 split local-network permission into local and loopback.
        for (const origin of new Set([auth.origin, ...providerDomains.flatMap(d => [`https://${d}`, `https://www.${d}`])])) {
            try { await this.browser.call('Browser.grantPermissions', { origin, permissions: ['localNetwork', 'loopbackNetwork'] }); }
            catch { try { await this.browser.call('Browser.grantPermissions', { origin, permissions: ['localNetworkAccess'] }); } catch { /* Older browsers do not implement this permission. */ } }
        }
        const page = await this.page();
        await page.call('Page.navigate', { url: auth.href });
        return { ready: true };
    }
    async page() {
        const targets = await fetch(`${this.endpoint}/json/list`, { signal: AbortSignal.timeout(2000) }).then(r => r.json());
        const pages = targets.filter(t => t.type === 'page' && t.webSocketDebuggerUrl);
        // A login provider may open a popup. Targets belong to this private profile.
        const target = pages[0];
        if (!target) throw new Error('TRAE_BROWSER_PAGE_UNAVAILABLE');
        for (const [id, page] of this.pages) if (!pages.some(p => p.id === id)) { page.close(); this.pages.delete(id); }
        if (!this.pages.has(target.id)) {
            const page = await connect(target.webSocketDebuggerUrl);
            await page.call('Page.enable');
            await page.call('Emulation.setDeviceMetricsOverride', { width: WIDTH, height: HEIGHT, deviceScaleFactor: 1, mobile: false });
            this.pages.set(target.id, page);
        }
        return this.pages.get(target.id);
    }
    async snapshot() {
        const page = await this.page();
        const result = await page.call('Runtime.evaluate', { expression: `JSON.stringify({location: location.origin + location.pathname, blocked: /当前区域不支持/.test(document.body?.innerText || '')})`, returnByValue: true });
        let info; try { info = JSON.parse(result.result.value); } catch { throw new Error('TRAE_BROWSER_PAGE_UNAVAILABLE'); }
        if (info.blocked) throw new Error('TRAE_BROWSER_REGION_BLOCKED');
        const { data } = await page.call('Page.captureScreenshot', { format: 'jpeg', quality: 75, captureBeyondViewport: false });
        if (data.length > 2 * 1024 * 1024 - 32) throw new Error('TRAE_BROWSER_PAGE_UNAVAILABLE');
        // Do not send grants/codes from URL queries or fragments to the viewer.
        return { image: `data:image/jpeg;base64,${data}`, width: WIDTH, height: HEIGHT, location: String(info.location).slice(0, 240) };
    }
    async input(action) {
        const events = inputEvents(action);
        const page = await this.page();
        for (const [method, params] of events) await page.call(method, params);
        return {};
    }
    reply(req, res, status, success) {
        const headers = { 'cache-control': 'no-store', 'referrer-policy': 'no-referrer', 'content-type': 'application/json',
            vary: 'Origin, Access-Control-Request-Headers, Access-Control-Request-Method' };
        if (officialOrigin(req.headers.origin)) Object.assign(headers, {
            'access-control-allow-origin': req.headers.origin, 'access-control-allow-credentials': 'true',
            'access-control-allow-methods': 'GET, POST, OPTIONS', 'access-control-allow-headers': 'content-type',
            'access-control-allow-private-network': 'true',
        });
        res.writeHead(status, headers);
        res.end(status === 204 ? undefined : JSON.stringify({ success }));
    }
    async receive(req, res) {
        try {
            if (req.headers.host !== this.callback.host || (req.headers.origin && !officialOrigin(req.headers.origin))) return this.reply(req, res, 403, false);
            if (req.url.length > LIMIT + 32) return this.reply(req, res, 413, false);
            const url = new URL(req.url, this.callback.origin);
            if (url.pathname !== '/authorize') return this.reply(req, res, 404, false);
            if (req.method === 'OPTIONS') return this.reply(req, res, 204, false);
            if (!['GET', 'POST'].includes(req.method)) return this.reply(req, res, 405, false);
            if (Math.floor(Date.now() / 1000) >= this.config.expires_at || this.completed) return this.reply(req, res, 410, false);
            let query = url.search.slice(1);
            if (req.method === 'POST') {
                let size = 0; const chunks = [];
                for await (const chunk of req) { size += chunk.length; if (size > LIMIT) return this.reply(req, res, 413, false); chunks.push(chunk); }
                query = Buffer.concat(chunks).toString('utf8');
            }
            if (!matchingTrace(query, this.config.trace)) return this.reply(req, res, 400, false);
            if (this.grants.size) return this.reply(req, res, 409, false);
            const request = randomUUID();
            const success = await new Promise(resolve => {
                const timer = setTimeout(() => { this.grants.delete(request); resolve(false); }, 90000);
                this.grants.set(request, { resolve, timer });
                this.emit({ event: 'grant', request, query });
            });
            this.completed = success;
            if (success) res.once('finish', () => setTimeout(() => this.stop(), 300));
            this.reply(req, res, success ? 200 : 400, success);
        } catch { if (!res.headersSent) this.reply(req, res, 400, false); else res.destroy(); }
    }
    grantResult(command) {
        const grant = this.grants.get(command.request);
        if (grant) { clearTimeout(grant.timer); this.grants.delete(command.request); grant.resolve(command.success === true); }
    }
    stop() {
        clearTimeout(this.timer);
        for (const g of this.grants.values()) { clearTimeout(g.timer); g.resolve(false); }
        this.grants.clear();
        this.server?.closeAllConnections(); this.server?.close();
        for (const page of this.pages.values()) page.close();
        if (this.browser) void this.browser.call('Browser.close').catch(() => undefined);
        this.emit({ event: 'done' });
    }
}

async function main() {
    const emit = value => process.stdout.write(`${JSON.stringify(value)}\n`);
    const browser = new LoginBrowser(emit);
    let started = false, queued = 0, chain = Promise.resolve();
    const lines = readline.createInterface({ input: process.stdin });
    lines.on('line', line => {
        if (line.length > LIMIT + 4096 || ++queued > 32) { browser.stop(); process.exit(1); }
        let command; try { command = JSON.parse(line); } catch { browser.stop(); process.exit(1); }
        if (command.op === 'close') { browser.stop(); process.exit(0); }
        if (command.op === 'grant_result') { queued--; browser.grantResult(command); return; }
        chain = chain.then(async () => {
            try {
                let result;
                if (command.op === 'start' && !started) { started = true; result = await browser.start(command); }
                else if (started && command.op === 'snapshot') result = await browser.snapshot();
                else if (started && command.op === 'input') result = await browser.input(command.action);
                else throw new Error('TRAE_BROWSER_COMMAND_FAILED');
                emit({ reply: command.id, result });
            } catch (error) {
                const allowed = ['TRAE_BROWSER_REGION_BLOCKED', 'TRAE_BROWSER_PAGE_UNAVAILABLE', 'TRAE_BROWSER_START_FAILED'];
                emit({ reply: command.id, error: allowed.includes(error.message) ? error.message : 'TRAE_BROWSER_COMMAND_FAILED' });
            } finally { queued--; }
        });
    });
    lines.on('close', () => { browser.stop(); process.exit(0); });
    process.on('SIGTERM', () => { browser.stop(); process.exit(0); });
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
