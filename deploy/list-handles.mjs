// List SMB handles on an Azure Files share/directory/file (data-plane REST, SharedKey).
// Usage: ACCOUNT=daemonstore KEY=<acct-key> SHARE=refresh-data FSPATH=<dir-or-file-or-""> node list-handles.mjs
import crypto from "node:crypto";

const { ACCOUNT, KEY, SHARE } = process.env;
const PATH = process.env.FSPATH ?? "";
if (!ACCOUNT || !KEY || !SHARE) throw new Error("need ACCOUNT, KEY, SHARE");

const VERSION = "2022-11-02";

function sign(verb, canonicalizedHeaders, canonicalizedResource) {
  const sts =
    [verb, "", "", "", "", "", "", "", "", "", "", ""].join("\n") + "\n" +
    canonicalizedHeaders +
    canonicalizedResource;
  return crypto.createHmac("sha256", Buffer.from(KEY, "base64")).update(sts, "utf8").digest("base64");
}

async function listHandles(marker) {
  const date = new Date().toUTCString();
  const segs = ["comp=listhandles"];
  if (marker) segs.push(`marker=${encodeURIComponent(marker)}`);
  const url = `https://${ACCOUNT}.file.core.windows.net/${SHARE}${PATH ? "/" + PATH : ""}?${segs.join("&")}`;
  const canonHeaders = `x-ms-date:${date}\nx-ms-version:${VERSION}\n`;
  let canonResource = `/${ACCOUNT}/${SHARE}${PATH ? "/" + PATH : ""}\ncomp:listhandles`;
  if (marker) canonResource += `\nmarker:${marker}`;
  const sig = sign("GET", canonHeaders, canonResource);
  const res = await fetch(url, {
    headers: { "x-ms-date": date, "x-ms-version": VERSION, Authorization: `SharedKey ${ACCOUNT}:${sig}` },
  });
  const text = await res.text();
  if (!res.ok) { console.log(`status=${res.status} body:`, text); return null; }
  const entries = [...text.matchAll(/<Handle>([\s\S]*?)<\/Handle>/g)].map((m) => {
    const get = (tag) => m[1].match(new RegExp(`<${tag}>(.*?)</${tag}>`))?.[1] ?? "";
    return { handleId: get("HandleId"), path: decodeURIComponent(get("Path")), clientIp: get("ClientIp"), openTime: get("OpenTime") };
  });
  for (const e of entries) console.log(`${e.handleId}  ${e.path}  client=${e.clientIp}  opened=${e.openTime}`);
  const next = text.match(/<NextMarker>(.*?)<\/NextMarker>/)?.[1];
  return next || null;
}

let marker = null;
do { marker = await listHandles(marker); } while (marker);
console.log("done");
