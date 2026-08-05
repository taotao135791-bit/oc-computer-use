// Secret redaction for anything the SDK (or an adapter) might log or print.
//
// Mirrors `cu_core::security::redact_json` exactly — the same rule in both
// languages: a field is a secret when its (lowercased) key contains `token`
// or `secret` (`control_token`, `observation_token`, `admin_token`,
// `client_secret`, …); string values in those fields become `[REDACTED]`,
// everything else passes through, and the input is never mutated (a copy is
// returned). Logging or printing a payload through `redactSecrets` can never
// leak a capability token.

/** Deep-copy `value` with every secret field replaced by `[REDACTED]`. */
export function redactSecrets(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(redactSecrets);
  }
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      const key = k.toLowerCase();
      if ((key.includes("token") || key.includes("secret")) && typeof v === "string") {
        out[k] = "[REDACTED]";
      } else {
        out[k] = redactSecrets(v);
      }
    }
    return out;
  }
  return value;
}
