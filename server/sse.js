const clients = new Set();

export function addClient(res) {
  res.writeHead(200, {
    'Content-Type': 'text/event-stream',
    'Cache-Control': 'no-cache',
    Connection: 'keep-alive',
  });
  res.write(':ok\n\n');
  clients.add(res);
  res.on('close', () => clients.delete(res));
}

export function broadcast(event, data) {
  const payload = `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const client of clients) client.write(payload);
}

// Heartbeat pour éviter la fermeture des connexions inactives (proxy Vite en dev)
setInterval(() => {
  for (const client of clients) client.write(':ping\n\n');
}, 25_000).unref();
