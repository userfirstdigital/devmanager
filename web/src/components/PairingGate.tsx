import { LockKeyhole, MonitorSmartphone } from "lucide-react";
import { useState, type FormEvent } from "react";

import { buildPairingUrl } from "../lib/browserIdentity";

export function PairingGate() {
  const [token, setToken] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const trimmed = token.trim();
    if (!trimmed) {
      setError("Enter the browser pair token from the desktop app.");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const response = await fetch(buildPairingUrl(trimmed), {
        credentials: "include",
      });
      if (!response.ok && response.status !== 0) {
        const retryAfter = Number(response.headers.get("Retry-After") ?? "0");
        setSubmitting(false);
        setError(
          response.status === 429 && retryAfter > 0
            ? `Too many attempts. Wait ${retryAfter}s and try again.`
            : response.status === 401 && retryAfter > 0
              ? `Token rejected. Wait ${retryAfter}s before trying again.`
              : response.status === 401
                ? "Token rejected."
                : `Pair failed (HTTP ${response.status})`,
        );
        return;
      }
      window.location.href = "/sessions";
    } catch (reason) {
      setSubmitting(false);
      setError(
        `Pair failed: ${reason instanceof Error ? reason.message : String(reason)}`,
      );
    }
  };

  return (
    <main className="dm-pairing-state">
      <div className="dm-pairing-card">
        <span className="dm-pairing-icon" aria-hidden="true">
          <MonitorSmartphone size={28} />
        </span>
        <h1>Connect to DevManager</h1>
        <p className="dm-pairing-intro">
          Pair this iPhone once, then DevManager will reconnect automatically
          whenever you return.
        </p>
        <form onSubmit={onSubmit} className="dm-pairing-form">
          <label>
            <span>Browser pair token</span>
            <input
              type="text"
              inputMode="text"
              autoComplete="off"
              autoCapitalize="characters"
              spellCheck={false}
              value={token}
              onChange={(event) => setToken(event.target.value)}
              disabled={submitting}
              placeholder="Paste token"
            />
          </label>
          <button
            type="submit"
            disabled={submitting || token.trim().length === 0}
          >
            {submitting ? "Pairing…" : "Pair browser"}
          </button>
        </form>
        {error ? (
          <p className="dm-pairing-error" role="alert">
            {error}
          </p>
        ) : null}
        <p className="dm-pairing-help">
          <LockKeyhole size={14} aria-hidden="true" />
          Find the token in the desktop app under Remote, Host, Browser Access.
        </p>
      </div>
    </main>
  );
}
