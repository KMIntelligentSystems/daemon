// Generate the three ACA Job payloads as JSON for az rest PUT
// (design/deploy-rust-airlock.md §4.2). Secrets from process env (ACRPW, HMAC).
import { writeFileSync } from "node:fs";

const { ACRPW, HMAC, ENVID } = process.env;
if (!ACRPW || !HMAC || !ENVID) throw new Error("need ACRPW, HMAC, ENVID env vars");

const jobs = [
  {
    name: "wake-fred-capacity",
    cron: "0 14 16-20 * *", // G.17 capacity, mid-month
    args: ["--source", "fred", "--month", "auto", "--target", "mfg_capacity"],
  },
  {
    name: "wake-census-orders",
    cron: "0 14 5-9 * *", // M3 full report window
    args: ["--source", "census", "--month", "auto", "--target", "m3_new_orders",
           "--series", "m3_new_orders,m3_unfilled_orders"],
  },
  {
    name: "wake-census-shipments",
    cron: "0 14 5-9 * *",
    args: ["--source", "census", "--month", "auto", "--target", "m3_shipments",
           "--series", "m3_total_shipments_nsa"],
  },
];

for (const j of jobs) {
  const body = {
    location: "eastus",
    properties: {
      environmentId: ENVID,
      configuration: {
        triggerType: "Schedule",
        replicaTimeout: 300,
        replicaRetryLimit: 2,
        scheduleTriggerConfig: {
          cronExpression: j.cron,
          parallelism: 1,
          replicaCompletionCount: 1,
        },
        registries: [
          { server: "daemonairlock.azurecr.io", username: "daemonairlock", passwordSecretRef: "acr-pw" },
        ],
        secrets: [
          { name: "acr-pw", value: ACRPW },
          { name: "hmac-key", value: HMAC },
        ],
      },
      template: {
        containers: [
          {
            name: "job",
            image: "daemonairlock.azurecr.io/daemon-airlock:1.0.0",
            command: ["/usr/local/bin/job-entry.sh"],
            args: j.args,
            env: [
              { name: "DAEMON_HMAC_KEY", secretRef: "hmac-key" },
              { name: "AIRLOCK_URL", value: "http://daemon-airlock" },
            ],
            resources: { cpu: 0.25, memory: "0.5Gi" },
          },
        ],
      },
    },
  };
  writeFileSync(new URL(`./job-${j.name}.json`, import.meta.url), JSON.stringify(body));
  console.log("wrote job-" + j.name + ".json");
}
