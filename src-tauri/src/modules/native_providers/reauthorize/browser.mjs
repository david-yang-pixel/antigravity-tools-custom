// Built-in OAuth worker. Only standard Node APIs are used; Chrome is installed once.
// Secrets arrive on stdin. Never print page contents, URLs, input or browser stderr.
import { spawn } from 'node:child_process';
import { createHmac } from 'node:crypto';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { browserProxy } from './browser-proxy.mjs';

const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const fail = code => { throw Object.assign(new Error(code), { code }); };
export function totp(secret, seconds = Math.floor(Date.now() / 1000)) {
    const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
    let bits = '';
    for (const char of secret.toUpperCase().replace(/[=\s]/g, '')) {
        const value = alphabet.indexOf(char);
        if (value < 0) fail('INVALID_2FA');
        bits += value.toString(2).padStart(5, '0');
    }
    const key = Buffer.from((bits.match(/.{8}/g) || []).map(byte => parseInt(byte, 2)));
    if (key.length < 5) fail('INVALID_2FA');
    const counter = Buffer.alloc(8);
    counter.writeBigUInt64BE(BigInt(Math.floor(seconds / 30)));
    const hash = createHmac('sha1', key).update(counter).digest();
    const offset = hash[hash.length - 1] & 15;
    return String((hash.readUInt32BE(offset) & 0x7fffffff) % 1000000).padStart(6, '0');
}

class DevTools {
    constructor(socket) {
        this.socket = socket;
        this.pending = new Map();
        this.sequence = 0;
        this.listeners = [];
        socket.addEventListener('message', event => {
            let data;
            try { data = JSON.parse(String(event.data)); } catch { return; }
            if (data.id) {
                const pending = this.pending.get(data.id);
                if (pending) {
                    this.pending.delete(data.id); clearTimeout(pending.timer);
                    data.error ? pending.reject(new Error('BROWSER_COMMAND_FAILED')) : pending.resolve(data.result);
                }
            } else this.listeners.forEach(listener => listener(data));
        });
        socket.addEventListener('close', () => {
            for (const pending of this.pending.values()) {
                clearTimeout(pending.timer); pending.reject(new Error('BROWSER_CLOSED'));
            }
            this.pending.clear();
        });
    }
    call(method, params = {}, sessionId) {
        return new Promise((resolve, reject) => {
            const id = ++this.sequence;
            const timer = setTimeout(() => { this.pending.delete(id); reject(new Error('BROWSER_COMMAND_TIMEOUT')); }, 10000);
            this.pending.set(id, { resolve, reject, timer });
            this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
        });
    }
}

// Runs inside the official login document. Return only recognized stages, never its text.
function formStep(input) {
    if (location.origin !== input.origin) return 'waiting';
    const visible = element => element && element.getClientRects().length && getComputedStyle(element).visibility !== 'hidden';
    const body = document.body?.innerText || '';
    if (/verify you are human|complete the security check|确认您是人类|验证您是人类/i.test(body) ||
        [...document.querySelectorAll('iframe')].some(frame => visible(frame) && /challenges\.cloudflare|hcaptcha|recaptcha/.test(frame.src))) return 'challenge';
    if (/check your inbox|check your email|code.*sent.*email|email.*verification code|检查您的邮箱|查看您的邮箱|发送.*邮箱/i.test(body)) return 'email_verification';
    if (/incorrect password|wrong password|invalid password|密码错误|密码不正确/i.test(body)) return 'invalid_credentials';
    if (/account.*deactivated|account.*suspended|账户.*停用|账号.*停用/i.test(body)) return 'account_unavailable';
    const find = selector => [...document.querySelectorAll(selector)].find(visible);
    const fill = (element, value, stage) => {
        if (!input.allowed.includes(stage)) return stage;
        const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
        setter.call(element, value);
        element.dispatchEvent(new Event('input', { bubbles: true }));
        element.dispatchEvent(new Event('change', { bubbles: true }));
        const form = element.closest('form');
        const submit = form && [...form.querySelectorAll('button[type="submit"],input[type="submit"]')].find(button => visible(button) && !button.disabled);
        if (submit) submit.click();
        else if (form) form.requestSubmit();
        else return 'unsupported';
        return stage + '_submitted';
    };
    const password = find('input[type="password"]');
    if (password) return fill(password, input.password, 'password');
    const email = find('input[type="email"],input[name="username"],input[autocomplete="username"]');
    if (email) return fill(email, input.email, 'email');
    const code = find('input[autocomplete="one-time-code"],input[name="code"],input[name="totp"],input[name="otp"]');
    if (code && /authenticator|authentication app|two.factor|totp|验证器|身份验证应用/i.test(body + location.pathname)) return fill(code, input.code, 'totp');
    const consent = [...document.querySelectorAll('button')].find(button => visible(button) && !button.disabled && /^(continue|allow|authorize|accept|继续|允许|授权)$/i.test(button.innerText.trim()));
    if (consent && input.allowed.includes('consent')) { consent.click(); return 'consent_submitted'; }
    return 'waiting';
}

export async function authorize(input, options = {}) {
    const origin = new URL(input.auth_url).origin;
    if (origin !== 'https://auth.openai.com' && origin !== options.testOrigin) fail('INVALID_LOGIN_ORIGIN');
    const profile = input.profile || await mkdtemp(join(tmpdir(), 'abv-chatgpt-'));
    let browser, socket, deadline, proxyRoute;
    let stopping = false;
    const cleanup = async () => {
        if (stopping) return;
        stopping = true;
        socket?.close();
        if (browser && browser.exitCode === null) {
            browser.kill('SIGTERM');
            for (let i = 0; i < 20 && browser.exitCode === null; i++) await pause(100);
            if (browser.exitCode === null) browser.kill('SIGKILL');
        }
        await proxyRoute?.close();
        // Chromium owns this temporary profile exclusively. No persistent account data.
        await rm(profile, { recursive: true, force: true, maxRetries: 3 }).catch(() => {});
    };
    const terminate = () => { void cleanup().finally(() => process.exit(1)); };
    process.once('SIGTERM', terminate); process.once('SIGINT', terminate);
    try {
        const args = ['--headless=new', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0',
            `--user-data-dir=${profile}`, '--no-first-run', '--no-default-browser-check',
            '--disable-dev-shm-usage', '--disable-background-networking', '--password-store=basic'];
        if (process.getuid?.() === 0) args.push('--no-sandbox');
        proxyRoute = await browserProxy(input.proxy);
        if (proxyRoute.address) args.push(`--proxy-server=${proxyRoute.address}`, '--proxy-bypass-list=<-loopback>');
        else args.push('--no-proxy-server');
        browser = spawn(input.browser, args, { stdio: 'ignore', windowsHide: true });
        let launchError = false;
        browser.on('error', () => { launchError = true; });
        let devtools;
        for (let i = 0; i < 100; i++) {
            if (launchError || browser.exitCode !== null) fail('BROWSER_UNAVAILABLE');
            try { devtools = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).trim().split('\n'); } catch {}
            if (devtools?.length === 2 && /^\d+$/.test(devtools[0])) break;
            await pause(100);
        }
        if (!devtools || devtools.length !== 2) fail('BROWSER_UNAVAILABLE');
        socket = new WebSocket(`ws://127.0.0.1:${devtools[0]}${devtools[1]}`);
        await new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('BROWSER_UNAVAILABLE')), 5000);
            socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
            socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('BROWSER_UNAVAILABLE')); }, { once: true });
        });
        const cdp = new DevTools(socket);
        const { targetId } = await cdp.call('Target.createTarget', { url: 'about:blank' });
        const { sessionId } = await cdp.call('Target.attachToTarget', { targetId, flatten: true });
        let callback;
        cdp.listeners.push(event => {
            if (event.method !== 'Fetch.requestPaused' || event.sessionId !== sessionId) return;
            const request = event.params;
            try {
                const url = new URL(request.request.url);
                if (url.origin === 'http://localhost:1455' && url.pathname === '/auth/callback') callback = url.href;
            } catch {}
            void cdp.call('Fetch.fulfillRequest', { requestId: request.requestId, responseCode: 200,
                body: Buffer.from('Authorization received. You can close this window.').toString('base64') }, sessionId).catch(() => {});
        });
        await cdp.call('Fetch.enable', { patterns: [{ urlPattern: 'http://localhost:1455/auth/callback*' }] }, sessionId);
        await cdp.call('Page.navigate', { url: input.auth_url }, sessionId);
        const counts = { email: 0, password: 0, totp: 0, consent: 0 };
        const last = {};
        let timeout = false;
        deadline = setTimeout(() => { timeout = true; }, options.timeoutMs || 150000);
        while (!timeout) {
            if (callback) return { callback };
            const proxyError = proxyRoute.failure(new URL(input.auth_url).hostname);
            if (proxyError) fail(proxyError);
            const allowed = Object.keys(counts).filter(stage => counts[stage] < (stage === 'consent' ? 3 : 1) && Date.now() - (last[stage] || 0) > 3000);
            const payload = { origin, email: input.email, password: input.password, code: totp(input.two_factor), allowed };
            let result;
            try {
                const current = await cdp.call('Runtime.evaluate', { expression: 'location.origin', returnByValue: true }, sessionId);
                if (current.result?.value !== origin) { await pause(500); continue; }
                const response = await cdp.call('Runtime.evaluate', { expression: `(${formStep.toString()})(${JSON.stringify(payload)})`, returnByValue: true }, sessionId);
                result = response.result?.value;
            } catch { if (callback) return { callback }; }
            if (result?.endsWith('_submitted')) { const stage = result.slice(0, -10); counts[stage]++; last[stage] = Date.now(); }
            if (['challenge', 'email_verification', 'invalid_credentials', 'account_unavailable', 'unsupported'].includes(result)) fail(result.toUpperCase());
            await pause(500);
        }
        fail('LOGIN_TIMEOUT');
    } finally {
        clearTimeout(deadline);
        process.removeListener('SIGTERM', terminate); process.removeListener('SIGINT', terminate);
        await cleanup();
    }
}

if (process.argv.includes('--abv-login-worker')) {
    const errors = new Set(['INVALID_2FA', 'BROWSER_UNAVAILABLE', 'UNSUPPORTED_PROXY', 'PROXY_AUTH_FAILED', 'PROXY_CONNECTION_FAILED', 'CHALLENGE',
        'EMAIL_VERIFICATION', 'INVALID_CREDENTIALS', 'ACCOUNT_UNAVAILABLE', 'UNSUPPORTED', 'LOGIN_TIMEOUT']);
    try {
        let raw = '';
        for await (const part of process.stdin) { raw += part; if (raw.length > 32768) fail('INVALID_INPUT'); }
        process.stdout.write(JSON.stringify(await authorize(JSON.parse(raw))));
    } catch (error) {
        process.stdout.write(JSON.stringify({ error: errors.has(error.code) ? error.code : 'LOGIN_FAILED' }));
    }
}
