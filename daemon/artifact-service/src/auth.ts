import { createPublicKey, verify as verifySignature } from "node:crypto";
import type http from "node:http";

/**
 * Microsoft Entra authentication for artifact-service.
 *
 * The caller supplies only `Authorization: Bearer <access token>`. Identity and
 * privilege are derived here from verified claims. Do not restore X-User-Id or
 * X-User-Role: both are caller-controlled strings and are not authentication.
 */

export interface ArtifactPrincipal {
  /** Stable catalog key. Admin is preserved for backward-compatible ownership. */
  id: string;
  tenantId: string;
  objectId: string;
  displayName: string;
  isAdmin: boolean;
  claims: Record<string, unknown>;
}

type Jwk = Record<string, unknown> & {
  kid?: string;
  kty?: string;
  alg?: string;
  n?: string;
  e?: string;
};

const TENANT_ID = process.env["ENTRA_TENANT_ID"] ?? "";
const CLIENT_ID = process.env["ARTIFACT_SERVICE_CLIENT_ID"] ?? "";
const REQUIRED_SCOPE = process.env["ARTIFACT_SERVICE_SCOPE"] ?? "access_as_user";
const ADMIN_ROLE = process.env["ARTIFACT_ADMIN_ROLE"] ?? "Artifact.Admin";
const ADMIN_OBJECT_IDS = new Set(
  (process.env["ARTIFACT_ADMIN_OBJECT_IDS"] ?? "")
    .split(",")
    .map((value) => value.trim().toLowerCase())
    .filter(Boolean),
);
const ALLOWED_CLIENT_IDS = new Set(
  (process.env["ARTIFACT_ALLOWED_CLIENT_IDS"] ?? "")
    .split(",")
    .map((value) => value.trim().toLowerCase())
    .filter(Boolean),
);
const OPENID_CONFIG = process.env["OPENID_CONFIG_URL"] ??
  `https://login.microsoftonline.com/${TENANT_ID || "common"}/v2.0/.well-known/openid-configuration`;

let openIdCache: { jwksUri: string; expiresAt: number } | null = null;
let jwksCache: { keys: Jwk[]; expiresAt: number } | null = null;

function decodeSegment(segment: string): Record<string, unknown> {
  return JSON.parse(Buffer.from(segment, "base64url").toString("utf8")) as Record<string, unknown>;
}

async function jwksUri(): Promise<string> {
  if (openIdCache && openIdCache.expiresAt > Date.now()) return openIdCache.jwksUri;
  const response = await fetch(OPENID_CONFIG);
  if (!response.ok) throw new Error(`OpenID configuration HTTP ${response.status}`);
  const body = await response.json() as { jwks_uri?: string };
  if (!body.jwks_uri) throw new Error("OpenID configuration lacks jwks_uri");
  openIdCache = { jwksUri: body.jwks_uri, expiresAt: Date.now() + 60 * 60 * 1000 };
  return body.jwks_uri;
}

async function signingKeys(): Promise<Jwk[]> {
  if (jwksCache && jwksCache.expiresAt > Date.now()) return jwksCache.keys;
  const response = await fetch(await jwksUri());
  if (!response.ok) throw new Error(`JWKS HTTP ${response.status}`);
  const body = await response.json() as { keys?: Jwk[] };
  jwksCache = { keys: body.keys ?? [], expiresAt: Date.now() + 60 * 60 * 1000 };
  return jwksCache.keys;
}

function bearerToken(req: http.IncomingMessage): string {
  const authorization = String(req.headers.authorization ?? "");
  const match = /^Bearer\s+(.+)$/i.exec(authorization);
  if (!match) throw new Error("Bearer access token is required");
  return match[1];
}

function strings(value: unknown): string[] {
  if (Array.isArray(value)) return value.filter((item): item is string => typeof item === "string");
  return typeof value === "string" ? value.split(/\s+/).filter(Boolean) : [];
}

export async function authenticateArtifactRequest(req: http.IncomingMessage): Promise<ArtifactPrincipal> {
  if (!TENANT_ID || !CLIENT_ID) {
    throw new Error("ENTRA_TENANT_ID and ARTIFACT_SERVICE_CLIENT_ID must be configured");
  }

  const token = bearerToken(req);
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("access token is not a JWT");
  const header = decodeSegment(parts[0]);
  const claims = decodeSegment(parts[1]);
  if (header["alg"] !== "RS256" || typeof header["kid"] !== "string") {
    throw new Error("unsupported access token signature");
  }
  const key = (await signingKeys()).find((candidate) => candidate.kid === header["kid"]);
  if (!key) throw new Error("access token signing key not found");
  const cryptoKey = createPublicKey({ key: key as any, format: "jwk" });
  const verified = verifySignature(
    "RSA-SHA256",
    Buffer.from(`${parts[0]}.${parts[1]}`),
    cryptoKey,
    Buffer.from(parts[2], "base64url"),
  );
  if (!verified) throw new Error("invalid access token signature");

  const now = Math.floor(Date.now() / 1000);
  if (typeof claims["exp"] !== "number" || claims["exp"] <= now) throw new Error("access token expired");
  if (typeof claims["nbf"] === "number" && claims["nbf"] > now + 60) throw new Error("access token is not active");
  if (claims["tid"] !== TENANT_ID) throw new Error("access token tenant mismatch");
  const acceptedIssuers = new Set([
    `https://login.microsoftonline.com/${TENANT_ID}/v2.0`,
    `https://sts.windows.net/${TENANT_ID}/`,
  ]);
  if (typeof claims["iss"] !== "string" || !acceptedIssuers.has(claims["iss"])) {
    throw new Error("access token issuer mismatch");
  }
  const audiences = strings(claims["aud"]);
  if (!audiences.some((audience) => audience === CLIENT_ID || audience === `api://${CLIENT_ID}`)) {
    throw new Error("access token audience mismatch");
  }
  if (!strings(claims["scp"]).includes(REQUIRED_SCOPE)) {
    throw new Error(`access token lacks required scope '${REQUIRED_SCOPE}'`);
  }
  const authorizedParty = String(claims["azp"] ?? claims["appid"] ?? "").trim().toLowerCase();
  if (ALLOWED_CLIENT_IDS.size > 0 && !ALLOWED_CLIENT_IDS.has(authorizedParty)) {
    throw new Error("access token client application is not allowed");
  }

  const tenantId = String(claims["tid"] ?? "").trim().toLowerCase();
  const objectId = String(claims["oid"] ?? "").trim().toLowerCase();
  if (!tenantId || !objectId) throw new Error("access token lacks stable tid/oid identity claims");
  const roles = strings(claims["roles"]);
  const isAdmin = ADMIN_OBJECT_IDS.has(objectId) || roles.includes(ADMIN_ROLE);
  return {
    // Keep the historical admin owner key while all ordinary users get an
    // immutable tenant/object key. preferred_username is display-only.
    id: isAdmin ? "admin" : `${tenantId}:${objectId}`,
    tenantId,
    objectId,
    displayName: String(claims["preferred_username"] ?? claims["name"] ?? objectId),
    isAdmin,
    claims,
  };
}
