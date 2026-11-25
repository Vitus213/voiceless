// A console that stands between the page and a real standalone console:
// the page and its assets come from `upstream`, every /api call is
// answered from a script, and the event stream is pushed by the test.
import { createServer, type ServerResponse } from 'node:http';
import { once } from 'node:events';

type Script = {
  /** The token the page must send; change it to model a restarted runtime. */
  token(): string;
  /** The status of the snapshot a (re)connecting page reads. */
  snapshot(): object;
  /** The JSON body for an /api path. */
  answer(path: string): unknown;
};

export type FakeConsole = {
  url: string;
  /** Every /api request the page made, in order. */
  requests: Array<{ path: string; body: unknown; token: string | undefined }>;
  /** Send an event to every page that is listening. */
  push(event: object): void;
  /** End every event stream: the page reconnects and reads the snapshot again. */
  drop(): void;
  close(): Promise<void>;
};

export async function fakeConsole(upstream: string, script: Script): Promise<FakeConsole> {
  const streams = new Set<ServerResponse>();
  const requests: FakeConsole['requests'] = [];
  const send = (response: ServerResponse, event: object) => {
    response.write(`data: ${JSON.stringify(event)}\n\n`);
  };
  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url!, upstream);
      const token = script.token();
      if (url.pathname === '/api/events') {
        if (url.searchParams.get('token') !== token) {
          response.writeHead(403).end();
          return;
        }
        response.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
        streams.add(response);
        response.on('close', () => streams.delete(response));
        send(response, { type: 'snapshot', status: script.snapshot(), default_mode: 'quick' });
        return;
      }
      if (url.pathname.startsWith('/api/')) {
        if (request.headers['x-voiceless-token'] !== token) {
          response.writeHead(403).end(JSON.stringify({ error: 'stale console token' }));
          return;
        }
        let data = '';
        for await (const chunk of request) data += chunk;
        requests.push({ path: url.pathname, body: data ? JSON.parse(data) : null, token: request.headers['x-voiceless-token'] });
        response.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify(script.answer(url.pathname)));
        return;
      }
      const page = url.pathname === '/' || url.pathname === '/index.html';
      const fetched = await fetch(url);
      const body = await fetched.text();
      response.writeHead(fetched.status, { 'Content-Type': fetched.headers.get('content-type')! });
      response.end(page ? body.replace(/(name="voiceless-token" content=")[^"]+/, `$1${token}`) : body);
    } catch {
      response.writeHead(500).end();
    }
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('the fake console did not bind TCP');
  return {
    url: `http://127.0.0.1:${address.port}/`,
    requests,
    push: (event) => { for (const stream of streams) send(stream, event); },
    drop: () => { for (const stream of streams) stream.end(); },
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((done) => server.close(() => done()));
    },
  };
}
