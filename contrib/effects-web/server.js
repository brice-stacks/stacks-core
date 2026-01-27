/* Minimal server to render the effects preview UI and proxy stacks-inspect. */
const http = require("http");
const fs = require("fs");
const path = require("path");
const { spawn } = require("child_process");
const { URL } = require("url");

const args = process.argv.slice(2);
const opts = {
  db: null,
  port: 3939,
  network: "xenon",
  bin: process.env.STACKS_INSPECT_BIN || null,
};

for (let i = 0; i < args.length; i += 1) {
  const arg = args[i];
  if (arg === "--db") {
    opts.db = args[i + 1];
    i += 1;
  } else if (arg === "--port") {
    opts.port = Number(args[i + 1]);
    i += 1;
  } else if (arg === "--network") {
    opts.network = args[i + 1];
    i += 1;
  } else if (arg === "--bin") {
    opts.bin = args[i + 1];
    i += 1;
  }
}

const repoRoot = path.resolve(__dirname, "../..");
const publicDir = path.join(__dirname, "public");
const defaultBin = path.join(repoRoot, "target", "debug", "stacks-inspect");
const stacksInspectBin = opts.bin || defaultBin;

function sendJson(res, status, payload) {
  res.writeHead(status, {
    "Content-Type": "application/json; charset=utf-8",
  });
  res.end(JSON.stringify(payload, null, 2));
}

function sendText(res, status, text) {
  res.writeHead(status, { "Content-Type": "text/plain; charset=utf-8" });
  res.end(text);
}

function serveStatic(req, res) {
  const url = new URL(req.url, `http://${req.headers.host}`);
  let relPath = url.pathname === "/" ? "/index.html" : url.pathname;
  relPath = relPath.replace(/\.\./g, "");
  const filePath = path.join(publicDir, relPath);
  if (!filePath.startsWith(publicDir)) {
    sendText(res, 403, "Forbidden");
    return;
  }
  fs.readFile(filePath, (err, data) => {
    if (err) {
      sendText(res, 404, "Not found");
      return;
    }
    const ext = path.extname(filePath);
    const contentType =
      ext === ".html"
        ? "text/html; charset=utf-8"
        : ext === ".css"
          ? "text/css; charset=utf-8"
          : ext === ".js"
            ? "text/javascript; charset=utf-8"
            : "application/octet-stream";
    res.writeHead(200, { "Content-Type": contentType });
    res.end(data);
  });
}

function runStacksInspect(contract, network, res) {
  if (!opts.db) {
    sendJson(res, 400, { error: "Missing --db path to chainstate root." });
    return;
  }
  const args = [
    "--network",
    normalizeNetwork(network || opts.network),
    "contract-effects",
    opts.db,
    contract,
    "--json",
  ];

  const proc = spawn(stacksInspectBin, args, {
    cwd: repoRoot,
    env: process.env,
  });

  let stdout = "";
  let stderr = "";
  proc.stdout.on("data", (chunk) => {
    stdout += chunk.toString();
  });
  proc.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });
  proc.on("close", (code) => {
    if (code !== 0) {
      sendJson(res, 500, {
        error: "stacks-inspect failed",
        details: stderr.trim(),
      });
      return;
    }
    try {
      const parsed = JSON.parse(stdout);
      sendJson(res, 200, parsed);
    } catch (err) {
      sendJson(res, 500, {
        error: "Failed to parse stacks-inspect output",
        details: err.message,
        raw: stdout,
      });
    }
  });
}

function runTxEffects(txid, blockId, network, res) {
  if (!opts.db) {
    sendJson(res, 400, { error: "Missing --db path to chainstate root." });
    return;
  }
  const args = [
    "--network",
    normalizeNetwork(network || opts.network),
    "txid-effects",
    opts.db,
    txid,
    "--json",
    "--graph",
  ];
  if (blockId) {
    args.push("--block-id", blockId);
  }

  const proc = spawn(stacksInspectBin, args, {
    cwd: repoRoot,
    env: process.env,
  });

  let stdout = "";
  let stderr = "";
  proc.stdout.on("data", (chunk) => {
    stdout += chunk.toString();
  });
  proc.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });
  proc.on("close", (code) => {
    if (code !== 0) {
      sendJson(res, 500, {
        error: "stacks-inspect failed",
        details: stderr.trim(),
      });
      return;
    }
    try {
      const parsed = JSON.parse(stdout);
      sendJson(res, 200, parsed);
    } catch (err) {
      sendJson(res, 500, {
        error: "Failed to parse stacks-inspect output",
        details: err.message,
        raw: stdout,
      });
    }
  });
}

function normalizeNetwork(network) {
  if (!network) {
    return opts.network;
  }
  if (network === "testnet") {
    return "xenon";
  }
  return network;
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  if (url.pathname === "/api/effects") {
    const contract = url.searchParams.get("contract");
    const network = url.searchParams.get("network");
    if (!contract) {
      sendJson(res, 400, { error: "Missing ?contract=" });
      return;
    }
    runStacksInspect(contract, network, res);
    return;
  }
  if (url.pathname === "/api/tx-effects") {
    const txid = url.searchParams.get("txid");
    const blockId = url.searchParams.get("blockId");
    const network = url.searchParams.get("network");
    if (!txid) {
      sendJson(res, 400, { error: "Missing ?txid=" });
      return;
    }
    runTxEffects(txid, blockId, network, res);
    return;
  }
  serveStatic(req, res);
});

server.listen(opts.port, () => {
  const binStatus = fs.existsSync(stacksInspectBin) ? "ok" : "missing";
  console.log(`Effects UI running at http://localhost:${opts.port}`);
  console.log(`stacks-inspect: ${stacksInspectBin} (${binStatus})`);
  console.log(`network: ${opts.network}`);
  console.log(`db: ${opts.db || "(not set)"}`);
});
