// Umoja downloads: serves release files from the R2 bucket and records
// each ISO download in Workers Analytics Engine.
//
//   /                    small page with the latest release
//   /latest              302 to the latest ISO
//   /latest.json         the latest release manifest (written by CI)
//   /releases/<tag>/...  release files (.iso, .iso.sha256, .list)
//
// Layout in R2 (written by .github/workflows/image.yml):
//   releases/<tag>/<name>.iso, <name>.iso.sha256, <name>.list
//   latest.json  {"tag","version","iso","sha256","size","published"}

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method not allowed", { status: 405, headers: { Allow: "GET, HEAD" } });
    }
    const path = decodeURIComponent(url.pathname);

    if (path === "/" || path === "/index.html") {
      if (request.method === "GET") forward(request, env, ctx, "pageview", "/");
      return index(request, env);
    }
    if (path === "/latest" || path === "/latest.iso") {
      const latest = await manifest(env);
      if (!latest) return new Response("No release published yet\n", { status: 404 });
      return Response.redirect(new URL(`/releases/${latest.tag}/${latest.iso}`, url), 302);
    }
    if (path === "/latest.json") return serve(request, env, ctx, "latest.json", "no-cache");
    if (path.startsWith("/releases/") && !path.includes("..")) {
      return serve(request, env, ctx, path.slice(1), "public, max-age=31536000, immutable");
    }
    return new Response("Not found\n", { status: 404 });
  },
};

async function manifest(env) {
  const obj = await env.BUCKET.get("latest.json");
  return obj ? obj.json() : null;
}

async function serve(request, env, ctx, key, cacheControl) {
  if (request.method === "HEAD") {
    const obj = await env.BUCKET.head(key);
    if (!obj) return new Response(null, { status: 404 });
    const headers = objectHeaders(obj, cacheControl);
    headers.set("Content-Length", String(obj.size));
    return new Response(null, { headers });
  }

  let obj;
  try {
    obj = await env.BUCKET.get(key, { range: request.headers, onlyIf: request.headers });
  } catch (e) {
    // R2 rejects a Range it can't satisfy.
    const head = await env.BUCKET.head(key);
    if (!head) return new Response("Not found\n", { status: 404 });
    return new Response(null, { status: 416, headers: { "Content-Range": `bytes */${head.size}` } });
  }
  if (!obj) return new Response("Not found\n", { status: 404 });
  const headers = objectHeaders(obj, cacheControl);
  if (!("body" in obj) || !obj.body) {
    // A precondition (If-None-Match, If-Modified-Since, ...) failed.
    const status = request.headers.has("If-None-Match") || request.headers.has("If-Modified-Since") ? 304 : 412;
    return new Response(null, { status, headers });
  }

  let status = 200;
  const r = obj.range;
  if (request.headers.has("Range") && r) {
    const offset = r.offset ?? (r.suffix !== undefined ? obj.size - r.suffix : 0);
    const length = r.length ?? (r.suffix ?? obj.size - offset);
    headers.set("Content-Range", `bytes ${offset}-${offset + length - 1}/${obj.size}`);
    headers.set("Content-Length", String(length));
    status = 206;
    // Download managers fetch an ISO in several ranges; count the one that
    // starts at byte 0 only.
    if (offset === 0) count(request, env, ctx, key, obj.size);
  } else {
    headers.set("Content-Length", String(obj.size));
    count(request, env, ctx, key, obj.size);
  }
  return new Response(obj.body, { status, headers });
}

function objectHeaders(obj, cacheControl) {
  const headers = new Headers();
  obj.writeHttpMetadata(headers);
  headers.set("ETag", obj.httpEtag);
  headers.set("Accept-Ranges", "bytes");
  headers.set("Cache-Control", cacheControl);
  if (obj.uploaded) headers.set("Last-Modified", obj.uploaded.toUTCString());
  return headers;
}

// One Analytics Engine data point per ISO download:
//   index1 = tag, blob1 = file, blob2 = tag, blob3 = country,
//   blob4 = client ("browser", "curl", "wget", "other"), double1 = 1,
//   double2 = ISO size in bytes.
function count(request, env, ctx, key, size) {
  if (!key.endsWith(".iso") || !env.DOWNLOADS) return;
  const [, tag = "", file = ""] = key.split("/");
  const ua = (request.headers.get("User-Agent") || "").toLowerCase();
  const client = /curl/.test(ua) ? "curl" : /wget/.test(ua) ? "wget"
    : /mozilla/.test(ua) ? "browser" : "other";
  try {
    env.DOWNLOADS.writeDataPoint({
      indexes: [tag],
      blobs: [file, tag, request.cf?.country || "", client],
      doubles: [1, size],
    });
  } catch (e) {
    console.error("writeDataPoint failed", e);
  }
  forward(request, env, ctx, "Download", "/" + key, { tag, file, client });
}

// Optionally send the same events to a web analytics server, server side
// (no cookies, no client script). Set as Worker variables (dashboard or
// wrangler.toml [vars]):
//   ANALYTICS       plausible | umami | matomo
//   ANALYTICS_URL   the server's base URL, e.g. https://stats.example.org
//   ANALYTICS_SITE  Plausible: site domain; Umami: website id; Matomo: idsite
//   ANALYTICS_TOKEN (secret, Matomo only, optional) token_auth so Matomo
//                   takes the visitor's IP instead of Cloudflare's
function forward(request, env, ctx, name, path, props = {}) {
  const kind = (env.ANALYTICS || "").toLowerCase();
  if (!kind || !env.ANALYTICS_URL || !env.ANALYTICS_SITE) return;
  const base = env.ANALYTICS_URL.replace(/\/+$/, "");
  const page = new URL(path, request.url).toString();
  const ip = request.headers.get("CF-Connecting-IP") || "";
  const ua = request.headers.get("User-Agent") || "unknown";
  const referrer = request.headers.get("Referer") || "";
  const headers = { "Content-Type": "application/json", "User-Agent": ua, "X-Forwarded-For": ip };
  let req;
  if (kind === "plausible") {
    req = fetch(`${base}/api/event`, {
      method: "POST", headers,
      body: JSON.stringify({ name, url: page, domain: env.ANALYTICS_SITE, referrer, props }),
    });
  } else if (kind === "umami") {
    const u = new URL(page);
    req = fetch(`${base}/api/send`, {
      method: "POST", headers,
      body: JSON.stringify({ type: "event", payload: {
        website: env.ANALYTICS_SITE, hostname: u.hostname, url: u.pathname, referrer,
        language: (request.headers.get("Accept-Language") || "").split(",")[0],
        ...(name === "pageview" ? {} : { name, data: props }),
      } }),
    });
  } else if (kind === "matomo") {
    const q = new URLSearchParams({ idsite: env.ANALYTICS_SITE, rec: "1", apiv: "1", url: page,
      urlref: referrer, ua, rand: String(Math.random()).slice(2) });
    if (name === "pageview") q.set("action_name", "Umoja downloads");
    else q.set("download", page);
    if (env.ANALYTICS_TOKEN) { q.set("token_auth", env.ANALYTICS_TOKEN); q.set("cip", ip); }
    req = fetch(`${base}/matomo.php?${q}`, { headers: { "User-Agent": ua } });
  } else {
    return;
  }
  ctx.waitUntil(req.then((r) => { if (!r.ok) console.error(`${kind} answered ${r.status}`); })
    .catch((e) => console.error(`${kind} failed`, e)));
}

async function index(request, env) {
  const latest = await manifest(env);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
  const body = latest
    ? `<h1>Umoja ${esc(latest.version)}</h1>
<p><a class="button" href="/releases/${esc(latest.tag)}/${esc(latest.iso)}">Download the installer ISO</a></p>
<p>${esc(latest.iso)} &middot; ${(latest.size / 2 ** 30).toFixed(1)} GiB &middot; published ${esc(latest.published.slice(0, 10))}</p>
<p>SHA-256: <code>${esc(latest.sha256)}</code>
(<a href="/releases/${esc(latest.tag)}/${esc(latest.iso)}.sha256">.sha256</a>,
<a href="/releases/${esc(latest.tag)}/${esc(latest.iso.replace(/\.iso$/, ".list"))}">package list</a>)</p>
<p><a href="https://github.com/kenvandine/umoja/releases/tag/${esc(latest.tag)}">Release notes</a></p>`
    : "<h1>Umoja</h1><p>No release published yet.</p>";
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>Umoja downloads</title>
<style>
:root{color-scheme:light dark;--bg:#fff;--fg:#1d1d1d;--accent:#e95420}
@media (prefers-color-scheme:dark){:root{--bg:#1d1d1d;--fg:#f2f2f2}}
body{background:var(--bg);color:var(--fg);font:16px/1.5 "Ubuntu Sans",system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 16px}
a{color:var(--accent)}code{word-break:break-all}
.button{display:inline-block;background:var(--accent);color:#fff;padding:.6rem 1.2rem;border-radius:.5rem;text-decoration:none;font-weight:600}
</style></head><body>${body}</body></html>`;
  return new Response(request.method === "HEAD" ? null : html, {
    headers: { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-cache" },
  });
}
