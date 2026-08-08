'use strict';

const http = require('http');
const https = require('https');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { URL } = require('url');

const CONFIG_PATH = path.resolve(process.env.RELAY_CONFIG_PATH || path.join(__dirname, 'config.json'));
let config;

try {
  config = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
} catch (error) {
  console.error('[FATAL] Cannot read config.json:', error.message);
  process.exit(1);
}

const HOST = config.host || '0.0.0.0';
const PORT = Number(config.port);
const RELAY_SECRET = config.secret;
const KIRO_SERVER = config.kiroServer;
const KIRO_API_KEY = config.kiroApiKey;
const DEFAULT_REGION = config.region || 'us-east-1';
const KIRO_TIMEOUT_MS = Number(config.kiroTimeoutMs || 15000);
const DELIVERY_LOG_FILE = path.resolve(path.dirname(CONFIG_PATH), config.deliveryLogFile || 'delivery-log.ndjson');
const MAX_BODY_BYTES = 64 * 1024;
const MAX_RESPONSE_BYTES = 64 * 1024;
const DELIVERY_ID_PATTERN = /^[A-Za-z0-9._:-]{1,100}$/;
const REGION_PATTERN = /^[A-Za-z0-9-]{1,64}$/;

if (!Number.isInteger(PORT) || PORT < 1 || PORT > 65535) throw new Error('port must be 1-65535');
if (typeof RELAY_SECRET !== 'string' || RELAY_SECRET.length < 24 || RELAY_SECRET === 'REPLACE_WITH_RANDOM_SECRET') {
  throw new Error('config.json secret must be a random value of at least 24 characters');
}
if (typeof KIRO_API_KEY !== 'string' || KIRO_API_KEY.length < 16 || KIRO_API_KEY === 'REPLACE_WITH_KIRO_ADMIN_API_KEY') {
  throw new Error('config.json kiroApiKey must contain the user kiro-rs admin API key');
}
if (typeof KIRO_SERVER !== 'string' || !KIRO_SERVER) throw new Error('config.json kiroServer is required');
if (!REGION_PATTERN.test(DEFAULT_REGION)) throw new Error('config.json region is invalid');
if (!Number.isInteger(KIRO_TIMEOUT_MS) || KIRO_TIMEOUT_MS < 1000 || KIRO_TIMEOUT_MS > 60000) {
  throw new Error('kiroTimeoutMs must be an integer from 1000 to 60000');
}

const serverUrl = new URL(KIRO_SERVER);
if (!['http:', 'https:'].includes(serverUrl.protocol)) throw new Error('kiroServer must use http or https');
fs.mkdirSync(path.dirname(DELIVERY_LOG_FILE), { recursive: true, mode: 0o700 });
try { fs.chmodSync(CONFIG_PATH, 0o600); } catch { /* Windows may not apply POSIX modes. */ }
if (fs.existsSync(DELIVERY_LOG_FILE)) fs.chmodSync(DELIVERY_LOG_FILE, 0o600);

function log(message) {
  console.log('[' + new Date().toISOString() + '] ' + message);
}

function json(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(payload),
    'Cache-Control': 'no-store',
  });
  res.end(payload);
}

function sameSecret(received) {
  const left = Buffer.from(String(received || ''));
  const right = Buffer.from(RELAY_SECRET);
  return left.length === right.length && crypto.timingSafeEqual(left, right);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let tooLarge = false;
    req.on('data', (chunk) => {
      if (tooLarge) return;
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        tooLarge = true;
        reject(Object.assign(new Error('request body too large'), { status: 413 }));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => { if (!tooLarge) resolve(Buffer.concat(chunks)); });
    req.on('error', (error) => { if (!tooLarge) reject(error); });
  });
}

function sha256(value) {
  return crypto.createHash('sha256').update(value).digest('hex');
}

function appendDelivery(record) {
  fs.appendFileSync(DELIVERY_LOG_FILE, JSON.stringify(record) + '\n', { encoding: 'utf8', mode: 0o600 });
  fs.chmodSync(DELIVERY_LOG_FILE, 0o600);
}

function readDeliveries() {
  if (!fs.existsSync(DELIVERY_LOG_FILE)) return [];
  return fs.readFileSync(DELIVERY_LOG_FILE, 'utf8').split('\n').filter(Boolean).map((line) => {
    try { return JSON.parse(line); } catch { return null; }
  }).filter(Boolean);
}

function postCredential(key, pushRegion) {
  return new Promise((resolve, reject) => {
    const target = new URL(serverUrl.toString().replace(/\/$/, '') + '/api/admin/credentials');
    const payload = Buffer.from(JSON.stringify({
      kiroApiKey: key,
      authMethod: 'api_key',
      authRegion: pushRegion,
      apiRegion: pushRegion,
    }));
    const client = target.protocol === 'https:' ? https : http;
    const request = client.request({
      hostname: target.hostname,
      port: target.port || (target.protocol === 'https:' ? 443 : 80),
      path: target.pathname + target.search,
      method: 'POST',
      timeout: KIRO_TIMEOUT_MS,
      headers: {
        Accept: 'application/json',
        'Content-Type': 'application/json',
        'Content-Length': payload.length,
        'x-api-key': KIRO_API_KEY,
      },
    }, (response) => {
      const chunks = [];
      let size = 0;
      let tooLarge = false;
      response.on('data', (chunk) => {
        if (tooLarge) return;
        size += chunk.length;
        if (size > MAX_RESPONSE_BYTES) {
          tooLarge = true;
          response.destroy(new Error('kiro-rs response too large'));
          return;
        }
        chunks.push(chunk);
      });
      response.on('end', () => {
        if (tooLarge) return;
        const raw = Buffer.concat(chunks).toString('utf8');
        let body = null;
        try { body = JSON.parse(raw); } catch { /* Keep body null. */ }
        resolve({ status: response.statusCode || 0, body });
      });
      response.on('error', reject);
    });
    request.on('timeout', () => request.destroy(new Error('kiro-rs request timed out')));
    request.on('error', reject);
    request.write(payload);
    request.end();
  });
}

const importsInFlight = new Map();

async function importDelivery(key, pushRegion, deliveryId, keyHash) {
  const previous = readDeliveries().filter((record) => record.deliveryId === deliveryId);
  if (previous.some((record) => record.keySha256 && record.keySha256 !== keyHash)) {
    return { status: 409, body: { ok: false, error: 'delivery_id is bound to a different key' } };
  }
  const delivered = previous.find((record) => record.status === 'delivered');
  if (delivered) {
    return {
      status: 200,
      body: { ok: true, duplicate: true, credentialId: delivered.credentialId || null },
    };
  }

  const active = importsInFlight.get(deliveryId);
  if (active) {
    if (active.keySha256 !== keyHash) {
      return { status: 409, body: { ok: false, error: 'delivery_id is concurrently bound to a different key' } };
    }
    return active.promise;
  }

  const promise = (async () => {
    try {
      const response = await postCredential(key, pushRegion);
      const data = response.body || {};
      const ok = response.status >= 200 && response.status < 300 && (data.success || data.credentialId || data.id);
      if (!ok) {
        appendDelivery({
          deliveryId,
          keySha256: keyHash,
          status: 'failed',
          upstreamStatus: response.status,
          at: new Date().toISOString(),
        });
        log('kiro-rs rejected delivery ' + deliveryId + ': HTTP ' + response.status);
        return { status: 502, body: { ok: false, error: 'kiro-rs rejected key', upstream_status: response.status } };
      }

      const credentialId = data.credentialId || data.id || null;
      appendDelivery({
        deliveryId,
        keySha256: keyHash,
        status: 'delivered',
        ...(credentialId ? { credentialId } : {}),
        at: new Date().toISOString(),
      });
      log('delivery imported into kiro-rs: deliveryId=' + deliveryId + ', credentialId=' + (credentialId || 'unknown'));
      return { status: 200, body: { ok: true, credentialId } };
    } catch (error) {
      appendDelivery({
        deliveryId,
        keySha256: keyHash,
        status: 'failed',
        error: error.message,
        at: new Date().toISOString(),
      });
      log('delivery failed: deliveryId=' + deliveryId + ', error=' + error.message);
      return { status: 502, body: { ok: false, error: 'kiro-rs unavailable' } };
    }
  })();

  importsInFlight.set(deliveryId, { keySha256: keyHash, promise });
  try {
    return await promise;
  } finally {
    importsInFlight.delete(deliveryId);
  }
}

const server = http.createServer(async (req, res) => {
  const route = (req.url || '').split('?')[0];
  if (req.method === 'GET' && route === '/health') {
    return json(res, 200, { ok: true, service: 'key-relay', target: 'kiro-rs' });
  }
  if (req.method !== 'POST' || route !== '/push') return json(res, 404, { error: 'not found' });

  try {
    const payload = JSON.parse((await readBody(req)).toString('utf8'));
    const authorization = String(req.headers.authorization || '').replace(/^Bearer\s+/i, '');
    const suppliedSecret = payload.secret || req.headers['x-relay-secret'] || authorization;
    if (!sameSecret(suppliedSecret)) return json(res, 401, { error: 'invalid secret' });
    if (typeof payload.key !== 'string' || !payload.key.trim()) return json(res, 400, { error: 'missing key' });
    if (payload.delivery_id !== undefined && (typeof payload.delivery_id !== 'string' || !DELIVERY_ID_PATTERN.test(payload.delivery_id))) {
      return json(res, 400, { error: 'delivery_id is invalid' });
    }

    const key = payload.key.trim();
    const keyHash = sha256(key);
    if (payload.key_sha256 !== undefined && payload.key_sha256 !== keyHash) {
      return json(res, 400, { error: 'key_sha256 does not match key' });
    }
    const pushRegion = typeof payload.region === 'string' && payload.region ? payload.region : DEFAULT_REGION;
    if (!REGION_PATTERN.test(pushRegion)) return json(res, 400, { error: 'region is invalid' });
    const deliveryId = payload.delivery_id || ('legacy-' + crypto.randomBytes(16).toString('hex'));
    const result = await importDelivery(key, pushRegion, deliveryId, keyHash);
    return json(res, result.status, result.body);
  } catch (error) {
    if (error.status === 413) return json(res, 413, { error: error.message });
    if (error instanceof SyntaxError) return json(res, 400, { error: 'invalid json' });
    log('request failed: ' + error.message);
    return json(res, 502, { ok: false, error: 'relay request failed' });
  }
});

server.listen(PORT, HOST, () => {
  log('key-relay listening on ' + HOST + ':' + PORT);
  log('receive endpoint: POST /push');
});

function shutdown() { server.close(() => process.exit(0)); }
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
