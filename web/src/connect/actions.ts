export type ConnectClientActionKind =
  | "resume"
  | "composerSubmit"
  | "request"
  | "rawTerminal";

export type ConnectActionIdempotency = "idempotent" | "nonIdempotent";

export function classifyClientAction(
  kind: ConnectClientActionKind,
): ConnectActionIdempotency {
  return kind === "resume" || kind === "composerSubmit"
    ? "idempotent"
    : "nonIdempotent";
}

export function isIdempotentClientAction(
  kind: ConnectClientActionKind,
): boolean {
  return classifyClientAction(kind) === "idempotent";
}
