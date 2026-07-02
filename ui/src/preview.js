// Lazy-loaded scientific preview mounts (vendor assets synced from web-dist).

const css = new Set();
function linkCss(href) {
  if (css.has(href)) return;
  const l = document.createElement("link");
  l.rel = "stylesheet";
  l.href = href;
  document.head.appendChild(l);
  css.add(href);
}

let katexMod;
async function katex() {
  if (!katexMod) {
    katexMod = (await import("/vendor/katex-Dn761jRB.js")).k;
    linkCss("/vendor/katex-DwwF5kvc.css");
  }
  return katexMod;
}

let rdkitInit;
async function rdkit() {
  if (!rdkitInit) {
    const mod = await import("/vendor/RDKit_minimal-B7RkdM0_.js");
    rdkitInit = mod.R.default();
  }
  return rdkitInit;
}

let mol3d;
async function mol3d() {
  if (!mol3d) {
    const mod = await import("/vendor/3Dmol-DfD4xImO.js");
    mol3d = mod._.default;
  }
  return mol3d;
}

let msaLoaded;
async function ensureMsa() {
  if (!msaLoaded) {
    await import("/vendor/nightingale-msa-5.6.0.js");
    msaLoaded = true;
  }
}

function escHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function fastaStats(text) {
  const lines = (text || "").split("\n");
  let seqs = 0;
  let maxLen = 0;
  let cur = 0;
  for (const raw of lines) {
    const line = raw.trim();
    if (!line || line.startsWith(";")) continue;
    if (line.startsWith(">")) {
      seqs += 1;
      cur = 0;
      continue;
    }
    cur += line.length;
    if (cur > maxLen) maxLen = cur;
  }
  return { seqs, maxLen };
}

function renderFasta(el, text) {
  const lines = (text || "").split("\n");
  let rows = "";
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const cls = line.startsWith(">") ? "rp-fasta-hdr" : "rp-fasta-seq";
    rows += `<tr><td class="rp-fasta-ln">${i + 1}</td><td class="${cls}">${escHtml(line) || "&nbsp;"}</td></tr>`;
  }
  const stats = fastaStats(text);
  const note = stats.seqs
    ? `<div class="rp-fasta-bar">${stats.seqs} sequences · ${stats.maxLen.toLocaleString()} positions</div>`
    : "";
  el.innerHTML = `${note}<div class="rp-fasta-wrap"><table class="rp-fasta-table"><tbody>${rows}</tbody></table></div>`;
}

/** @param {string} kind @param {HTMLElement} el @param {string} payloadJson */
export async function mountPreview(kind, el, payloadJson) {
  const p = JSON.parse(payloadJson);
  el.innerHTML = "";
  el.classList.add("rp-heavy");

  switch (kind) {
    case "latex": {
      const k = await katex();
      el.innerHTML = k.renderToString(p.tex, { displayMode: !!p.display, throwOnError: false });
      break;
    }
    case "pdf": {
      const src = p.b64 ? `data:application/pdf;base64,${p.b64}` : p.url;
      el.innerHTML = `<embed class="rp-pdf" src="${src}" type="application/pdf"/>`;
      break;
    }
    case "image": {
      const src = p.b64 ? `data:${p.mime || "image/png"};base64,${p.b64}` : p.url;
      el.innerHTML = `<img class="rp-img" src="${src}" alt="${p.alt || ""}"/>`;
      break;
    }
    case "structure": {
      const box = document.createElement("div");
      box.className = "rp-3dmol";
      el.appendChild(box);
      const $3Dmol = await mol3d();
      const v = $3Dmol.createViewer(box, { backgroundColor: "0x1e2024" });
      v.addModel(p.text, p.format || "pdb");
      v.setStyle({}, { cartoon: { color: "spectrum" } });
      v.zoomTo();
      v.render();
      break;
    }
    case "molecule": {
      const RDKit = await rdkit();
      const mol = RDKit.get_mol(p.smiles || p.text);
      if (!mol) {
        el.textContent = "Invalid molecule";
        break;
      }
      el.innerHTML = mol.get_svg(400, 300);
      mol.delete();
      break;
    }
    case "fasta": {
      renderFasta(el, p.text || "");
      break;
    }
    case "msa": {
      await ensureMsa();
      const text = p.text || p.fasta || "";
      const stats = fastaStats(text);
      const wrap = document.createElement("div");
      wrap.className = "rp-msa-wrap";
      const bar = document.createElement("div");
      bar.className = "rp-msa-bar";
      bar.textContent = `${stats.seqs} sequences · ${stats.maxLen.toLocaleString()} positions`;
      wrap.appendChild(bar);
      const tag = document.createElement("nightingale-msa");
      tag.setAttribute("width", "100%");
      tag.setAttribute("height", "420");
      tag.setAttribute("color-scheme", "clustal2");
      tag.setAttribute("label-width", "150");
      tag.setAttribute("tile-height", "20");
      tag.setAttribute("display-start", "1");
      tag.setAttribute("display-end", String(Math.max(stats.maxLen, 50)));
      wrap.appendChild(tag);
      el.appendChild(wrap);
      await customElements.whenDefined("nightingale-msa");
      tag.data = text;
      break;
    }
    default:
      el.textContent = p.text || "";
  }
}
