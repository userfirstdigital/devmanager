import { Terminal } from "lucide-react";
import { useState, type FormEvent } from "react";

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
      // Hit /pair — the server validates the token, mints the remembered
      // auth cookie for this DevManager instance, and 303s to "/". fetch()
      // with follow will land us back on the SPA with the cookie set. We
      // then force a full navigation so the app reinitialises against the
      // authenticated WS.
      const response = await fetch(
        `/pair?t=${encodeURIComponent(trimmed)}`,
        { credentials: "include" },
      );
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
      // Full-page load so App.tsx's effect re-runs with the new cookie.
      window.location.href = "/";
    } catch (err) {
      setSubmitting(false);
      setError(`Pair failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  return (
    <div className="min-h-screen bg-zinc-900 text-zinc-100 flex items-center justify-center px-4">
      <div className="max-w-md w-full bg-zinc-800 border border-zinc-700 rounded-lg shadow-xl p-6">
        <div className="flex items-center gap-2 mb-4">
          <Terminal className="size-5 text-indigo-400" />
          <h1 className="text-lg font-semibold">DevManager Web</h1>
        </div>
        <p className="text-sm text-zinc-300 mb-4">
          This browser is not paired with DevManager. If someone sent you an
          invite link, open that directly. Otherwise open the desktop app, go
          to{" "}
          <strong className="text-zinc-100">
            Remote → Host → Browser Access
          </strong>
          , copy the browser pair token, and paste it below.
        </p>
        <form onSubmit={onSubmit} className="space-y-3">
          <label className="block">
            <span className="block text-xs font-medium text-zinc-400 mb-1">
              Browser pair token
            </span>
            <input
              type="text"
              inputMode="text"
              autoComplete="off"
              autoCapitalize="characters"
              spellCheck={false}
              value={token}
              onChange={(e) => setToken(e.target.value)}
              className="w-full px-3 py-2 bg-zinc-950 border border-zinc-700 rounded text-sm text-zinc-100 font-mono tracking-wider placeholder:text-zinc-600 focus:outline-none focus:border-indigo-500"
              disabled={submitting}
              autoFocus
            />
          </label>
          <button
            type="submit"
            disabled={submitting || token.trim().length === 0}
            className="w-full px-3 py-2 bg-indigo-600 hover:bg-indigo-500 text-white text-sm font-medium rounded disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {submitting ? "Pairing..." : "Pair browser"}
          </button>
        </form>
        {error && (
          <p className="text-xs text-red-400 mt-3" role="alert">
            {error}
          </p>
        )}
        <p className="text-xs text-zinc-500 mt-4">
          Pairing is remembered for this browser on this DevManager instance,
          so you usually only need to do this once unless you clear cookies or
          the host revokes browser access.
        </p>
      </div>
    </div>
  );
}
