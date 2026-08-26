// Generic ACA volume patcher: adds an Azure Files volume mount to an app.
// Usage: APP=<name> STORE=<envStorageName> MOUNT=/data node patch-volumes-generic.mjs
// Reads ./app-show.json (az containerapp show output), PATCHes via az rest.
import { readFileSync, writeFileSync } from "node:fs";
import { execSync } from "node:child_process";

const { APP, STORE, MOUNT } = process.env;
if (!APP || !STORE || !MOUNT) throw new Error("need APP, STORE, MOUNT");

const app = JSON.parse(readFileSync("app-show.json", "utf8"));
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
      .map(({ volumeName, mountPath }) => ({ volumeName, mountPath })),
    { volumeName: "data", mountPath: MOUNT },
  ],
});

const cleanTpl = {
  containers: tpl.containers.map(cleanContainer),
  volumes: [{ name: "data", storageType: "AzureFile", storageName: STORE }],
  scale: {
    ...(tpl.scale?.minReplicas !== undefined ? { minReplicas: tpl.scale.minReplicas } : {}),
    ...(tpl.scale?.maxReplicas !== undefined ? { maxReplicas: tpl.scale.maxReplicas } : {}),
    ...(tpl.scale?.rules ? { rules: tpl.scale.rules } : {}),
  },
};

writeFileSync("app-patch.json", JSON.stringify({ properties: { template: cleanTpl } }));
const out = execSync(
  `az rest --method patch --url "https://management.azure.com${app.id}?api-version=2024-03-01" --body @app-patch.json --query properties.provisioningState -o tsv`,
  { shell: "bash" }
).toString().trim();
console.log("PATCH state:", out);
