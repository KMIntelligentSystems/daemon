// Force-close ALL SMB handles on a file in Azure Files (data-plane REST, SharedKey).
// Usage: ACCOUNT=daemonstore KEY=<acct-key> SHARE=airlock-data FILE=sandbox.db node force-close-handles.mjs
import crypto from "node:crypto";

const { ACCOUNT, KEY, SHARE, FILE } = process.env;
if (!ACCOUNT || !KEY || !SHARE || !FILE) throw new Error("need ACCOUNT, KEY, SHARE, FILE");

const VERSION = "2022-11-02";

function sign(verb, canonicalizedHeaders, canonicalizedResource) {
  // String-to-sign for x-ms-version 2015-02-21+: VERB + 11 legacy fields
  // (Content-Length empty when 0) + headers + resource.
  // VERB + 11 legacy fields (each \n-terminated), then headers (already
  // \n-terminated) + resource concatenated directly — no extra separator.
  const sts =
    [verb, "", "", "", "", "", "", "", "", "", "", ""].join("\n") + "\n" +
    canonicalizedHeaders +
    canonicalizedResource;
  if (process.env.DEBUG_STS) console.error("MY STS:", JSON.stringify(sts));
  return crypto.createHmac("sha256", Buffer.from(KEY, "base64")).update(sts, "utf8").digest("base64");
}

async function forceClose(marker) {
  const date = new Date().toUTCString();
  const query = marker ? `comp=forceclosehandles&marker=${encodeURIComponent(marker)}` : "comp=forceclosehandles";
  const url = `https://${ACCOUNT}.file.core.windows.net/${SHARE}/${FILE}?${query}`;
  const canonHeaders = `x-ms-date:${date}\nx-ms-handle-id:*\nx-ms-version:${VERSION}\n`;
  let canonResource = `/${ACCOUNT}/${SHARE}/${FILE}\ncomp:forceclosehandles`;
  if (marker) canonResource += `\nmarker:${marker}`;
  const sig = sign("PUT", canonHeaders, canonResource);
  const res = await fetch(url, {
    method: "PUT",
    headers: {
      "x-ms-date": date,
      "x-ms-version": VERSION,
      "x-ms-handle-id": "*",
      Authorization: `SharedKey ${ACCOUNT}:${sig}`,
      "Content-Length": "0",
    },
  });
  const closed = res.headers.get("x-ms-number-of-handles-closed");
  const failed = res.headers.get("x-ms-number-of-handles-failed");
  const next = res.headers.get("x-ms-marker");
  console.log(`status=${res.status} closed=${closed} failed=${failed} marker=${next ?? "(none)"}`);
  if (!res.ok) console.log("body:", await res.text());
  return next;
}

let marker = null;
do {
  marker = await forceClose(marker);
} while (marker);
console.log("done");
