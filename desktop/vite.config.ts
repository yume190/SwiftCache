import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

interface Descriptor {
  address: string;
  token: string;
}

function candidateDescriptors(): string[] {
  const explicit = process.env.SWIFTCACHE_DAEMON_FILE;
  if (explicit) return [explicit];
  const dataDir = process.env.SWIFTCACHE_DATA_DIR;
  if (dataDir) return [path.join(dataDir, 'daemon.json')];
  const home = os.homedir();
  return [
    path.join(home, 'Library', 'Application Support', 'dev.yume.SwiftCache', 'daemon.json'),
    path.join(process.env.XDG_DATA_HOME ?? path.join(home, '.local', 'share'), 'swiftcache', 'daemon.json'),
    path.join(process.env.LOCALAPPDATA ?? path.join(home, 'AppData', 'Local'), 'yume', 'SwiftCache', 'data', 'daemon.json'),
  ];
}

function readDescriptor(): Descriptor {
  const candidates = candidateDescriptors();
  for (const file of candidates) {
    let raw: string;
    try {
      raw = fs.readFileSync(file, 'utf8');
    } catch {
      continue;
    }
    const parsed = JSON.parse(raw) as Partial<Descriptor>;
    if (!parsed.address || !parsed.token) throw new Error(`${file} 缺少 address 或 token`);
    return { address: parsed.address, token: parsed.token };
  }
  throw new Error(
    `找不到 daemon.json（已檢查：${candidates.join('、')}）。請先啟動 swiftcache daemon，` +
      '並以 SWIFTCACHE_DATA_DIR 或 SWIFTCACHE_DAEMON_FILE 指向它。',
  );
}

function fail(res: http.ServerResponse, status: number, message: string) {
  res.statusCode = status;
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify({ error: message }));
}

// Forwards /api/* to the daemon named by its descriptor, injecting the bearer token.
function daemonProxy(): Plugin {
  return {
    name: 'swiftcache-daemon-proxy',
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (!req.url?.startsWith('/api/')) return next();
        let desc: Descriptor;
        try {
          desc = readDescriptor();
        } catch (e) {
          return fail(res, 502, e instanceof Error ? e.message : String(e));
        }
        const target = new URL(req.url, desc.address);
        const headers: http.OutgoingHttpHeaders = { ...req.headers, host: target.host };
        headers.authorization = `Bearer ${desc.token}`;
        delete headers.origin;
        delete headers.referer;
        const upstream = http.request(
          { hostname: target.hostname, port: target.port, path: target.pathname + target.search, method: req.method, headers },
          (up) => {
            res.writeHead(up.statusCode ?? 502, up.headers);
            up.pipe(res);
          },
        );
        upstream.on('error', (e) => fail(res, 502, `無法連線到 daemon ${desc.address}：${e.message}`));
        req.pipe(upstream);
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), daemonProxy()],
  clearScreen: false,
  server: { host: '127.0.0.1', port: 1420, strictPort: true },
  build: { target: 'es2022', outDir: 'dist' },
});
