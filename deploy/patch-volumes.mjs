// Patch the daemon-airlock Container App: add the Azure Files volume at /data.
// Rebuilds properties.template with only writeable fields (API 2024-03-01 rejects
// server-side extras like imageType / cooldownPeriod that `show` returns).
// PATCH merges properties, so secrets (redacted in show output) are untouched.
import { readFileSync, writeFileSync } from "node:fs";

const app = JSON.parse(readFileSync(new URL("./app.json", import.meta.url), "utf8"));
const tpl = app.properties.template;

const cleanContainer = (c) => ({
  name: c.name,
  image: c.image,
  ...(c.command ? { command: c.command } : {}),
  ...(c.args ? { args: c.args } : {}),
  ...(c.env ? { env: c.env.map(({ name, value, secretRef }) => ({ name, ...(value !== undefined ? { value } : {}), ...(secretRef ? { secretRef } : {}) })) } : {}),
  ...(c.resources ? { resources: c.resources } : {}),
  volumeMounts: [
    ...(c.volumeMounts ?? []).filter((m) => m.volumeName !== "data")
      .map(({ volumeName, mountPath, subPath }) => ({ volumeName, mountPath, ...(subPath ? { subPath } : {}) })),
    { volumeName: "data", mountPath: "/data" },
  ],
});

const cleanTpl = {
  containers: tpl.containers.map(cleanContainer),
  ...(tpl.initContainers ? { initContainers: tpl.initContainers } : {}),
  volumes: [{ name: "data", storageType: "AzureFile", storageName: "airlockfiles" }],
  scale: {
    ...(tpl.scale?.minReplicas !== undefined ? { minReplicas: tpl.scale.minReplicas } : {}),
    ...(tpl.scale?.maxReplicas !== undefined ? { maxReplicas: tpl.scale.maxReplicas } : {}),
    ...(tpl.scale?.rules ? { rules: tpl.scale.rules } : {}),
  },
};

writeFileSync(new URL("./patch.json", import.meta.url), JSON.stringify({ properties: { template: cleanTpl } }));
console.log("patch.json written; containers:", cleanTpl.containers.map((c) => c.name).join(","),
  "| scale:", JSON.stringify(cleanTpl.scale));
