const API_BASE = "";

const elements = {
  contractId: document.getElementById("contractId"),
  loadContract: document.getElementById("loadContract"),
  loadTx: document.getElementById("loadTx"),
  txidInput: document.getElementById("txidInput"),
  blockIdInput: document.getElementById("blockIdInput"),
  statusText: document.getElementById("statusText"),
  functionList: document.getElementById("functionList"),
  functionCount: document.getElementById("functionCount"),
  functionHeaderTitle: document.getElementById("functionHeaderTitle"),
  functionTitle: document.getElementById("functionTitle"),
  functionBadge: document.getElementById("functionBadge"),
  argForm: document.getElementById("argForm"),
  callGraph: document.getElementById("callGraph"),
  readsList: document.getElementById("readsList"),
  writesList: document.getElementById("writesList"),
  effectChips: document.getElementById("effectChips"),
  txSummary: document.getElementById("txSummary"),
  txLabel: document.getElementById("txLabel"),
  txContract: document.getElementById("txContract"),
  txFunction: document.getElementById("txFunction"),
  txTxid: document.getElementById("txTxid"),
  txBlock: document.getElementById("txBlock"),
  txArgs: document.getElementById("txArgs"),
  argSection: document.getElementById("argSection"),
};

let currentNetwork = "testnet";
let currentAbi = null;
let currentEffects = null;
let selectedFunction = null;
let currentTx = null;
let currentMode = "contract";
let currentGraphView = "mermaid";

if (window.mermaid) {
  window.mermaid.initialize({
    startOnLoad: false,
    theme: "dark",
    securityLevel: "loose",
  });
}

document.querySelectorAll(".pill").forEach((btn) => {
  btn.addEventListener("click", () => {
    if (btn.dataset.network) {
      setNetwork(btn.dataset.network);
    }
    if (btn.dataset.mode) {
      setMode(btn.dataset.mode);
    }
    if (btn.dataset.graph) {
      currentGraphView = btn.dataset.graph;
      if (selectedFunction && currentEffects) {
        renderEffects(selectedFunction);
      } else if (currentTx) {
        renderTxSummary(currentTx);
      }
    }
  });
});

document.addEventListener("click", (event) => {
  const target = event.target;
  if (!target || !target.classList.contains("copyable")) {
    return;
  }
  const full = target.dataset.full;
  if (!full) {
    return;
  }
  navigator.clipboard
    .writeText(full)
    .then(() => {
      setStatus(`Copied ${full}`);
    })
    .catch(() => {
      setStatus("Failed to copy value.");
    });
});

elements.loadContract.addEventListener("click", () => {
  const contractId = elements.contractId.value.trim();
  if (!contractId) {
    setStatus("Enter a contract ID.");
    return;
  }
  loadContract(contractId, { updateUrl: true });
});

elements.loadTx.addEventListener("click", () => {
  const txid = normalizeHex(elements.txidInput.value.trim());
  const blockId = normalizeHex(elements.blockIdInput.value.trim());
  if (!txid) {
    setStatus("Enter a transaction ID.");
    return;
  }
  loadTxEffects(txid, blockId, { updateUrl: true });
});

function setStatus(message) {
  elements.statusText.textContent = message;
}

function updateModePanels() {
  const contractPanel = document.getElementById("contractInputPanel");
  const txPanel = document.getElementById("txInputPanel");
  document.body.classList.toggle("mode-transaction", currentMode === "transaction");
  if (currentMode === "transaction") {
    contractPanel.classList.add("hidden");
    txPanel.classList.remove("hidden");
    setStatus("Enter a transaction ID to analyze.");
    if (elements.functionHeaderTitle) {
      elements.functionHeaderTitle.textContent = "Transaction";
    }
    if (elements.txSummary) {
      elements.txSummary.classList.remove("hidden");
    }
    clearPanels();
  } else {
    txPanel.classList.add("hidden");
    contractPanel.classList.remove("hidden");
    setStatus("Waiting for a contract…");
    if (elements.functionHeaderTitle) {
      elements.functionHeaderTitle.textContent = "Functions";
    }
    if (elements.txSummary) {
      elements.txSummary.classList.add("hidden");
    }
    clearPanels();
  }
}

function setNetwork(network, { updateUrl: shouldUpdateUrl = true } = {}) {
  if (!network) {
    return;
  }
  currentNetwork = network;
  document
    .querySelectorAll(".pill[data-network]")
    .forEach((pill) => pill.classList.remove("active"));
  const active = document.querySelector(`.pill[data-network="${network}"]`);
  if (active) {
    active.classList.add("active");
  }
  if (shouldUpdateUrl) {
    updateUrl();
  }
}

function setMode(mode, { updateUrl: shouldUpdateUrl = true } = {}) {
  if (!mode) {
    return;
  }
  currentMode = mode;
  document
    .querySelectorAll(".pill[data-mode]")
    .forEach((pill) => pill.classList.remove("active"));
  const active = document.querySelector(`.pill[data-mode="${mode}"]`);
  if (active) {
    active.classList.add("active");
  }
  updateModePanels();
  if (shouldUpdateUrl) {
    updateUrl();
  }
}

function updateUrl() {
  const params = new URLSearchParams();
  if (currentNetwork) {
    params.set("network", currentNetwork);
  }
  if (currentMode === "transaction") {
    const txid = normalizeHex(elements.txidInput.value.trim());
    const blockId = normalizeHex(elements.blockIdInput.value.trim());
    if (txid) {
      params.set("txid", txid);
    }
    if (blockId) {
      params.set("blockId", blockId);
    }
    params.set("mode", "transaction");
  } else {
    const contractId = elements.contractId.value.trim();
    if (contractId) {
      params.set("contract", contractId);
    }
    params.set("mode", "contract");
  }
  const url = `${window.location.pathname}?${params.toString()}`;
  window.history.replaceState({}, "", url);
}

function clearPanels() {
  const placeholder =
    currentMode === "transaction"
      ? "Enter a transaction ID to analyze."
      : "Load a contract to view functions.";
  elements.functionList.innerHTML = `<div class="empty">${placeholder}</div>`;
  elements.functionCount.textContent = "0";
  elements.functionTitle.textContent =
    currentMode === "transaction" ? "Transaction" : "Select a function";
  elements.functionBadge.textContent = currentMode === "transaction" ? "Call" : "—";
  elements.argForm.innerHTML = '<div class="empty">Pick a function to see its arguments.</div>';
  elements.callGraph.innerHTML = '<div class="empty">No function selected.</div>';
  elements.readsList.textContent = "No data yet.";
  elements.writesList.textContent = "No data yet.";
  elements.effectChips.innerHTML = `
    <span class="chip">Reads: —</span>
    <span class="chip">Writes: —</span>
    <span class="chip">Calls: —</span>
    <span class="chip">Assets: —</span>
  `;
  if (elements.txLabel) {
    elements.txLabel.textContent = "contract-call";
  }
  if (elements.txContract) {
    elements.txContract.textContent = "—";
  }
  if (elements.txFunction) {
    elements.txFunction.textContent = "—";
  }
  if (elements.txTxid) {
    elements.txTxid.textContent = "—";
  }
  if (elements.txBlock) {
    elements.txBlock.textContent = "—";
  }
  if (elements.txArgs) {
    elements.txArgs.innerHTML = '<div class="empty">No arguments.</div>';
  }
}

async function loadContract(contractId, { updateUrl: shouldUpdateUrl } = {}) {
  clearPanels();
  setStatus("Loading ABI and effects…");
  selectedFunction = null;
  currentTx = null;
  if (shouldUpdateUrl) {
    updateUrl();
  }

  try {
    const [abi, effects] = await Promise.all([
      fetchAbi(contractId, currentNetwork),
      fetchEffects(contractId, currentNetwork),
    ]);
    currentAbi = abi;
    currentEffects = effects;
    setStatus(`Loaded ${contractId}.`);
    renderFunctionList();
  } catch (err) {
    setStatus(`Failed to load contract: ${err.message}`);
    elements.functionList.innerHTML = `<div class="empty">${err.message}</div>`;
  }
}

async function loadTxEffects(txid, blockId, { updateUrl: shouldUpdateUrl } = {}) {
  clearPanels();
  setStatus("Analyzing transaction…");
  selectedFunction = null;
  currentAbi = null;
  currentEffects = null;
  currentTx = null;
  elements.functionList.innerHTML = '<div class="empty">Loading…</div>';
  if (shouldUpdateUrl) {
    updateUrl();
  }

  try {
    const payload = await fetchTxEffects(txid, blockId, currentNetwork);
    currentTx = payload;
    setStatus(`Loaded tx ${payload.txid}.`);
    renderTxSummary(payload);
  } catch (err) {
    setStatus(`Failed to load tx: ${err.message}`);
    elements.functionList.innerHTML = `<div class="empty">${err.message}</div>`;
  }
}

async function fetchAbi(contractId, network) {
  const apiHost =
    network === "mainnet"
      ? "https://api.hiro.so"
      : "https://api.testnet.hiro.so";
  const apiId = toApiContractId(contractId);
  const res = await fetch(`${apiHost}/v2/contracts/interface/${apiId}`);
  let data = null;
  try {
    data = await res.json();
  } catch (err) {
    data = null;
  }
  if (!res.ok) {
    const message =
      data?.error ||
      data?.message ||
      `Hiro API error (${res.status}) for ${apiId}`;
    throw new Error(message);
  }
  const abi = data?.abi || data;
  if (!abi || !abi.functions) {
    const message =
      data?.error ||
      data?.message ||
      "No ABI returned for this contract.";
    throw new Error(message);
  }
  return abi;
}

function toApiContractId(contractId) {
  if (contractId.includes("/")) {
    return contractId;
  }
  const parts = contractId.split(".");
  if (parts.length === 2) {
    return `${parts[0]}/${parts[1]}`;
  }
  return contractId;
}

async function fetchEffects(contractId, network) {
  const res = await fetch(
    `${API_BASE}/api/effects?contract=${encodeURIComponent(
      contractId
    )}&network=${encodeURIComponent(network)}`
  );
  if (!res.ok) {
    const payload = await res.json().catch(() => ({}));
    throw new Error(payload.error || "Unable to load effects.");
  }
  return res.json();
}

async function fetchTxEffects(txid, blockId, network) {
  const url = new URL(`${API_BASE}/api/tx-effects`, window.location.origin);
  url.searchParams.set("txid", txid);
  if (blockId) {
    url.searchParams.set("blockId", blockId);
  }
  url.searchParams.set("network", network);
  const res = await fetch(url.toString());
  if (!res.ok) {
    const payload = await res.json().catch(() => ({}));
    throw new Error(payload.error || payload.details || "Unable to load tx effects.");
  }
  return res.json();
}

function renderFunctionList() {
  const functions = (currentAbi?.functions || []).slice();
  elements.functionCount.textContent = functions.length.toString();
  elements.functionList.innerHTML = "";

  if (!functions.length) {
    elements.functionList.innerHTML =
      '<div class="empty">No functions found in ABI.</div>';
    return;
  }

  const accessOrder = {
    public: 0,
    read_only: 1,
    "read-only": 1,
    readonly: 1,
    private: 2,
  };

  functions.sort((left, right) => {
    const leftOrder = accessOrder[left.access] ?? 3;
    const rightOrder = accessOrder[right.access] ?? 3;
    if (leftOrder !== rightOrder) {
      return leftOrder - rightOrder;
    }
    return left.name.localeCompare(right.name);
  });

  functions.forEach((fn) => {
    const accessLabel = fn.access
      ? fn.access.replace("_", "-")
      : "unknown";
    const card = document.createElement("button");
    card.className = "function-item";
    card.type = "button";
    card.innerHTML = `
      <div class="function-name">${fn.name}</div>
      <div class="function-meta">${accessLabel} • ${
        fn.args.length
      } args</div>
    `;
    card.addEventListener("click", () => {
      document
        .querySelectorAll(".function-item")
        .forEach((item) => item.classList.remove("active"));
      card.classList.add("active");
      selectFunction(fn);
    });
    elements.functionList.appendChild(card);
  });
}

function selectFunction(fn) {
  selectedFunction = fn;
  elements.functionTitle.textContent = fn.name;
  elements.functionBadge.textContent = fn.access;
  renderArgs(fn);
  renderEffects(fn);
}

function renderTxSummary(payload) {
  elements.functionCount.textContent = "1";
  elements.functionList.innerHTML = "";
  const card = document.createElement("button");
  card.className = "function-item active";
  card.type = "button";
  card.innerHTML = `
    <div class="function-name">${payload.label}</div>
    <div class="function-meta">${renderHexValue(payload.txid)}</div>
  `;
  elements.functionList.appendChild(card);
  elements.functionTitle.textContent = "Transaction";
  elements.functionBadge.textContent = payload.label;
  elements.argForm.innerHTML =
    '<div class="empty">This view is derived from an on‑chain transaction.</div>';
  if (elements.txLabel) {
    elements.txLabel.textContent = payload.label || "contract-call";
  }
  if (elements.txContract) {
    elements.txContract.textContent = payload.call_contract || "—";
  }
  if (elements.txFunction) {
    elements.txFunction.textContent = payload.call_function || "—";
  }
  if (elements.txTxid) {
    setHexText(elements.txTxid, payload.txid);
  }
  if (elements.txBlock) {
    setHexText(elements.txBlock, payload.index_block_hash);
  }
  if (elements.txArgs) {
    if (payload.call_args && payload.call_args.length) {
      elements.txArgs.innerHTML = payload.call_args
        .map(
          (arg, index) =>
            `<div class="tx-arg">#${index} ${escapeHtml(arg)}</div>`
        )
        .join("");
    } else {
      elements.txArgs.innerHTML = '<div class="empty">No arguments.</div>';
    }
  }
  renderTxEffects(payload);
}

function renderArgs(fn) {
  if (!fn.args.length) {
    elements.argForm.innerHTML =
      '<div class="empty">This function takes no arguments.</div>';
    return;
  }
  elements.argForm.innerHTML = "";
  fn.args.forEach((arg, index) => {
    const row = document.createElement("div");
    row.className = "arg-row";
    row.innerHTML = `
      <label>${arg.name} <span class="muted">(${formatType(arg.type)})</span></label>
      <input data-arg-index="${index}" placeholder="Enter ${arg.name}..." />
    `;
    elements.argForm.appendChild(row);
  });
  elements.argForm.querySelectorAll("input").forEach((input) => {
    input.addEventListener("input", () => {
      if (selectedFunction) {
        renderEffects(selectedFunction);
      }
    });
  });
}

function getArgValues() {
  const args = {};
  elements.argForm.querySelectorAll("input").forEach((input) => {
    const index = Number(input.dataset.argIndex);
    args[index] = input.value.trim();
  });
  return args;
}

function renderEffects(fn) {
  const effectsEntry = currentEffects?.effects?.[fn.name];
  if (!effectsEntry) {
    elements.readsList.textContent = "No effects data.";
    elements.writesList.textContent = "No effects data.";
    elements.callGraph.innerHTML = '<div class="empty">No call data.</div>';
    return;
  }

  const argValues = getArgValues();
  const reads = decorateEffectSet(effectsEntry.reads || [], argValues);
  const writes = decorateEffectSet(effectsEntry.writes || [], argValues);
  const calls =
    currentEffects?.calls?.[fn.name] ||
    effectsEntry.contract_calls ||
    [];

  const assetCount = [...reads, ...writes].filter((entry) => entry.kind === "asset").length;

  elements.effectChips.innerHTML = `
    <span class="chip">Reads: ${reads.length}</span>
    <span class="chip">Writes: ${writes.length}</span>
    <span class="chip">Calls: ${calls.length}</span>
    <span class="chip">Assets: ${assetCount}</span>
  `;

  elements.readsList.innerHTML = renderEffectItems(reads);
  elements.writesList.innerHTML = renderEffectItems(writes);
  renderCallGraph(fn.name, calls, argValues);
}

function renderTxEffects(payload) {
  const effects = payload.effects;
  if (!effects) {
    elements.readsList.textContent = "No effects data.";
    elements.writesList.textContent = "No effects data.";
    elements.callGraph.innerHTML = '<div class="empty">No call data.</div>';
    return;
  }

  const reads = decorateEffectSet(effects.reads || [], {});
  const writes = decorateEffectSet(effects.writes || [], {});
  const calls = payload.call_graph || [];
  const assetCount = [...reads, ...writes].filter((entry) => entry.kind === "asset").length;

  elements.effectChips.innerHTML = `
    <span class="chip">Reads: ${reads.length}</span>
    <span class="chip">Writes: ${writes.length}</span>
    <span class="chip">Calls: ${calls.length}</span>
    <span class="chip">Assets: ${assetCount}</span>
  `;

  elements.readsList.innerHTML = renderEffectItems(reads);
  elements.writesList.innerHTML = renderEffectItems(writes);

  if (!calls.length) {
    elements.callGraph.innerHTML = '<div class="empty">No contract calls.</div>';
  } else {
    renderTxCallGraph(calls);
  }
}

function renderCallGraph(rootFunction, calls, argValues) {
  if (!calls.length) {
    elements.callGraph.innerHTML = '<div class="empty">No contract calls.</div>';
    return;
  }
  const edges = calls.map((call) => {
    const targetContract = formatContractRef(call.contract, argValues);
    const targetLabel = formatCallLabel(targetContract, call.function);
    return {
      from: rootFunction,
      to: targetLabel.full,
      toDisplay: targetLabel.display,
      toFull: targetLabel.full,
      contract: targetContract,
      contractDisplay: shortenContractId(targetContract),
      function: call.function,
    };
  });
  renderGraphView(edges);
}

function renderTxCallGraph(edges) {
  const mapped = edges.map((edge) => {
    const fromLabel = formatCallLabel(edge.from_contract, edge.from_function);
    const toLabel = formatCallLabel(edge.to_contract, edge.to_function);
    return {
      from: fromLabel.full,
      fromDisplay: fromLabel.display,
      fromFull: fromLabel.full,
      to: toLabel.full,
      toDisplay: toLabel.display,
      toFull: toLabel.full,
      contract: edge.to_contract,
      contractDisplay: shortenContractId(edge.to_contract),
      function: edge.to_function,
    };
  });
  renderGraphView(mapped);
}

function renderGraphView(edges) {
  renderMermaidGraph(edges);
}

function renderMermaidGraph(edges) {
  if (!window.mermaid) {
    elements.callGraph.innerHTML = '<div class="empty">Mermaid not available.</div>';
    return;
  }
  const nodes = new Map();
  const addNode = (label) => {
    if (!nodes.has(label)) {
      nodes.set(label, `n${nodes.size}`);
    }
    return nodes.get(label);
  };

  const lines = ["flowchart LR"];
  edges.forEach((edge) => {
    const fromId = addNode(edge.from);
    const toId = addNode(edge.to);
    const fromLabel = edge.fromDisplay || edge.from;
    const toLabel = edge.toDisplay || edge.to;
    lines.push(
      `  ${fromId}["${escapeMermaidLabel(shortenCallLabel(fromLabel))}"] --> ${toId}["${escapeMermaidLabel(
        shortenCallLabel(toLabel)
      )}"]`
    );
  });

  const graph = lines.join("\n");
  const renderId = `mermaid-${Date.now()}`;
  window.mermaid
    .render(renderId, graph)
    .then((result) => {
      elements.callGraph.innerHTML = result.svg;
    })
    .catch(() => {
      elements.callGraph.innerHTML = '<div class="empty">Failed to render diagram.</div>';
    });
}

function decorateEffectSet(effects, argValues) {
  return effects.map((entry) => {
    if (entry.Contract) {
      const contractInfo = formatContractDisplay(entry.Contract, argValues);
      const locationInfo = formatLocationBadge(entry.Contract);
      return {
        kind: "contract",
        labelPrefix: contractInfo.display,
        suffix: contractInfo.suffix,
        copyValue: contractInfo.copyValue,
        warn: entry.Contract.contract === "<any>",
        badge: locationInfo.badge,
        name: locationInfo.name,
      };
    }
    if (entry.AssetOwnership) {
      const principal = formatPrincipalDisplay(entry.AssetOwnership.principal, argValues);
      const assetInfo = formatAssetDisplay(entry.AssetOwnership.asset);
      return {
        kind: "asset",
        labelPrefix: `${assetInfo.display} ·`,
        principalDisplay: principal.display,
        copyValue: principal.copyValue,
        warn: entry.AssetOwnership.principal === "<any>",
        assetCopyValue: assetInfo.copyValue,
        assetDisplay: assetInfo.display,
        badge: assetInfo.badge,
        name: assetInfo.name,
      };
    }
    if (entry.AccountNonce) {
      const principal = formatPrincipalDisplay(entry.AccountNonce.principal, argValues);
      return {
        kind: "nonce",
        labelPrefix: "account-nonce",
        principalDisplay: principal.display,
        copyValue: principal.copyValue,
        warn: entry.AccountNonce.principal === "<any>",
        badge: "chain",
      };
    }
    if (entry.ChainState) {
      return {
        kind: "chain",
        badge: "chain",
        name: `chain ${entry.ChainState}`,
        label: "",
        warn: false,
      };
    }
    return { kind: "unknown", label: JSON.stringify(entry), warn: false };
  });
}

function renderEffectItems(items) {
  if (!items.length) {
    return '<div class="empty">None</div>';
  }
  return items
    .map((item) => {
      const badge = item.badge
        ? `<span class="badge-pill ${item.badge}">${item.badge}</span>`
        : "";
      const name = item.name ? `<span>${escapeHtml(item.name)}</span>` : "";
      if (item.principalDisplay) {
        const principal = escapeHtml(item.principalDisplay);
        const full = item.copyValue ? escapeHtml(item.copyValue) : "";
        const span = item.copyValue
          ? `<span class="copyable" data-full="${full}" title="${full}">${principal}</span>`
          : `<span title="${full}">${principal}</span>`;
        const prefix = item.assetCopyValue
          ? `<span class="copyable" data-full="${escapeHtml(
              item.assetCopyValue
            )}" title="${escapeHtml(item.assetCopyValue)}">${escapeHtml(
              item.labelPrefix || ""
            )}</span>`
          : escapeHtml(item.labelPrefix || "");
        const suffix = item.suffix ? ` ${escapeHtml(item.suffix)}` : "";
        return `<div class="effect-item ${item.warn ? "warn" : ""}"><div class="effect-line">${badge}${name}${prefix} ${span}${suffix}</div></div>`;
      }
      if (item.copyValue) {
        const display = escapeHtml(item.labelPrefix || "");
        const full = escapeHtml(item.copyValue);
        const suffix = item.suffix ? ` ${escapeHtml(item.suffix)}` : "";
        const span = `<span class="copyable" data-full="${full}" title="${full}">${display}</span>`;
        return `<div class="effect-item ${item.warn ? "warn" : ""}"><div class="effect-line">${badge}${name}${span}${suffix}</div></div>`;
      }
      return `<div class="effect-item ${item.warn ? "warn" : ""}"><div class="effect-line">${badge}${name}${escapeHtml(
        item.label
      )}</div></div>`;
    })
    .join("");
}

function formatType(type) {
  if (typeof type === "string") {
    return type;
  }
  if (!type || typeof type !== "object") {
    return "unknown";
  }
  if (type.optional) {
    return `optional<${formatType(type.optional)}>`;
  }
  if (type.list) {
    const listType = type.list.type || type.list;
    const length =
      typeof type.list.length === "number" ? ` ${type.list.length}` : "";
    return `list<${formatType(listType)}>${length}`;
  }
  if (type.tuple) {
    const entries = Object.entries(type.tuple)
      .map(([name, value]) => `${name}: ${formatType(value)}`)
      .join(", ");
    return `tuple{${entries}}`;
  }
  if (type.response) {
    const okType = formatType(type.response.ok);
    const errType = formatType(type.response.error);
    return `response<${okType}, ${errType}>`;
  }
  if (type.buff) {
    const length =
      typeof type.buff.length === "number" ? type.buff.length : null;
    return length ? `buff[${length}]` : "buff";
  }
  if (type["string-ascii"]) {
    const length =
      typeof type["string-ascii"].length === "number"
        ? type["string-ascii"].length
        : null;
    return length ? `string-ascii[${length}]` : "string-ascii";
  }
  if (type["string-utf8"]) {
    const length =
      typeof type["string-utf8"].length === "number"
        ? type["string-utf8"].length
        : null;
    return length ? `string-utf8[${length}]` : "string-utf8";
  }
  if (type.trait_reference || type["trait-reference"]) {
    const trait = type.trait_reference || type["trait-reference"];
    if (typeof trait === "string") {
      return `trait<${trait}>`;
    }
    if (trait?.name && trait?.contract_id) {
      return `trait<${trait.contract_id}.${trait.name}>`;
    }
    if (trait?.name) {
      return `trait<${trait.name}>`;
    }
    return "trait";
  }
  if (type.principal) {
    return "principal";
  }
  return JSON.stringify(type);
}

function formatContract(contract, argValues) {
  const contractId = formatContractRef(contract.contract, argValues);
  const location = contract.location
    ? contract.location.DataMap
      ? `map ${contract.location.DataMap}`
      : contract.location.DataVar
        ? `var ${contract.location.DataVar}`
        : "any"
    : "any";
  return `${contractId} (${location})`;
}

function formatContractDisplay(contract, argValues) {
  const contractId = formatContractRef(contract.contract, argValues);
  const location = contract.location
    ? contract.location.DataMap
      ? `map ${contract.location.DataMap}`
      : contract.location.DataVar
        ? `var ${contract.location.DataVar}`
        : "any"
    : "any";
  return {
    display: shortenContractId(contractId),
    copyValue: isCopyablePrincipal(contractId) ? contractId : null,
    suffix: "",
  };
}

function formatAsset(asset, argValues) {
  const principal = formatPrincipal(asset.principal, argValues);
  return `${asset.asset} · ${principal}`;
}

function formatAssetDisplay(assetId) {
  if (!assetId) {
    return { display: assetId, copyValue: null, badge: null, name: null };
  }
  if (assetId === "stx") {
    return { display: "stx", copyValue: null, badge: "stx", name: null };
  }
  const parts = assetId.split(".");
  if (parts.length < 3) {
    return {
      display: shortenContractId(assetId),
      copyValue: isCopyablePrincipal(assetId) ? assetId : null,
      badge: null,
      name: null,
    };
  }
  const contract = `${parts[0]}.${parts[1]}`;
  const rest = parts.slice(2).join(".");
  const kindMatch = rest.match(/\((ft|nft)\)\s*$/);
  const tokenKind = kindMatch ? kindMatch[1] : null;
  const restName = rest.replace(/\s*\((ft|nft)\)\s*$/, "");
  const display = `${shortenContractId(contract)}.${restName}`;
  return {
    display,
    copyValue: assetId,
    badge: tokenKind,
    name: null,
  };
}

function formatLocationBadge(contractEntry) {
  const location = contractEntry.location;
  if (!location) {
    return { badge: null, name: null };
  }
  if (location.DataMap) {
    return { badge: "map", name: location.DataMap };
  }
  if (location.DataVar) {
    return { badge: "var", name: location.DataVar };
  }
  return { badge: null, name: null };
}

function formatContractRef(ref, argValues) {
  if (ref.startsWith("$arg")) {
    const index = Number(ref.replace("$arg[", "").replace("]", ""));
    return argValues[index] || ref;
  }
  return ref;
}

function formatPrincipal(ref, argValues) {
  if (ref.startsWith("$arg")) {
    const index = Number(ref.replace("$arg[", "").replace("]", ""));
    return argValues[index] || ref;
  }
  return ref;
}

function formatPrincipalDisplay(ref, argValues) {
  const full = formatPrincipal(ref, argValues);
  const isCopyable = isCopyablePrincipal(full);
  return {
    display: shortenPrincipal(full),
    copyValue: isCopyable ? full : null,
  };
}

function normalizeHex(value) {
  if (!value) {
    return value;
  }
  return value.startsWith("0x") ? value.slice(2) : value;
}

function escapeMermaidLabel(value) {
  return value.replace(/"/g, '\\"');
}

function shortenPrincipal(value) {
  if (!value || value.length <= 10) {
    return value;
  }
  if (value.includes(".") || value.includes("/")) {
    return shortenContractId(value);
  }
  return shortenAddress(value);
}

function isHexValue(value) {
  if (!value) {
    return false;
  }
  const trimmed = value.startsWith("0x") ? value.slice(2) : value;
  return /^[0-9a-fA-F]+$/.test(trimmed);
}

function shortenHexValue(value, head = 6, tail = 6) {
  if (!value) {
    return value;
  }
  const hasPrefix = value.startsWith("0x");
  const raw = hasPrefix ? value.slice(2) : value;
  if (raw.length <= head + tail + 3) {
    return value;
  }
  const start = raw.slice(0, head);
  const end = raw.slice(-tail);
  return `${hasPrefix ? "0x" : ""}${start}...${end}`;
}

function formatHexValue(value) {
  if (!value) {
    return { display: "—", full: null };
  }
  if (isHexValue(value)) {
    const display = shortenHexValue(value);
    if (display !== value) {
      return { display, full: value };
    }
  }
  return { display: value, full: null };
}

function renderHexValue(value) {
  const formatted = formatHexValue(value);
  const display = escapeHtml(formatted.display);
  if (formatted.full) {
    const full = escapeHtml(formatted.full);
    return `<span class="copyable" data-full="${full}" title="${full}">${display}</span>`;
  }
  return display;
}

function setHexText(element, value) {
  if (!element) {
    return;
  }
  const formatted = formatHexValue(value);
  if (formatted.full) {
    const full = escapeHtml(formatted.full);
    element.innerHTML = `<span class="copyable" data-full="${full}" title="${full}">${escapeHtml(
      formatted.display
    )}</span>`;
  } else {
    element.textContent = formatted.display;
    if (formatted.display && formatted.display !== "—") {
      element.title = formatted.display;
    }
  }
}

function escapeHtml(value) {
  if (value === null || value === undefined) {
    return "";
  }
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function isCopyablePrincipal(value) {
  if (!value) {
    return false;
  }
  return value.startsWith("SP") || value.startsWith("ST");
}

function shortenAddress(value) {
  if (!value || value.length <= 10) {
    return value;
  }
  const start = value.slice(0, 4);
  const end = value.slice(-5);
  return `${start}...${end}`;
}

function shortenContractId(value) {
  if (!value) {
    return value;
  }
  const delimiter = value.includes(".") ? "." : value.includes("/") ? "/" : null;
  if (!delimiter) {
    return shortenAddress(value);
  }
  const parts = value.split(delimiter);
  if (parts.length < 2) {
    return shortenAddress(value);
  }
  const address = parts.shift();
  const rest = parts.join(delimiter);
  return `${shortenAddress(address)}${delimiter}${rest}`;
}

function shortenCallLabel(value) {
  if (!value) {
    return value;
  }
  const dotIndex = value.indexOf(".");
  if (dotIndex === -1) {
    if (value.startsWith("SP") || value.startsWith("ST")) {
      return shortenAddress(value);
    }
    return value;
  }
  const contract = value.slice(0, dotIndex);
  const suffix = value.slice(dotIndex + 1);
  return `${shortenContractId(contract)}.${suffix}`;
}

function formatCallLabel(contractId, fnName) {
  const full = `${contractId}.${fnName}`;
  return {
    full,
    display: shortenCallLabel(full),
  };
}

updateModePanels();

function initFromUrl() {
  const params = new URLSearchParams(window.location.search);
  const network = params.get("network");
  const mode = params.get("mode");
  const contract = params.get("contract");
  const txid = params.get("txid");
  const blockId = params.get("blockId");

  if (network) {
    setNetwork(network, { updateUrl: false });
  }

  if (mode === "transaction" || txid) {
    setMode("transaction", { updateUrl: false });
    if (txid) {
      elements.txidInput.value = txid;
    }
    if (blockId) {
      elements.blockIdInput.value = blockId;
    }
    updateUrl();
    if (txid) {
      loadTxEffects(normalizeHex(txid), normalizeHex(blockId || ""), {
        updateUrl: false,
      });
    }
    return;
  }

  if (contract) {
    setMode("contract", { updateUrl: false });
    elements.contractId.value = contract;
    updateUrl();
    loadContract(contract, { updateUrl: false });
  }
}

initFromUrl();
